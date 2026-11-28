//! The exchange loop: whole UDS messages in from a [`Transport`], through [`SimEcu::exchange`],
//! the ECU's responses back out after the delays it gives, and its state kept in the state file.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sim_ecu::persist::{default_ecu_config, load_ecu_state, read_ecu_config, save_ecu_state};
use sim_ecu::{Addressing, SimEcu};

/// How long a wait for a request runs at most before the loop looks at the stop flag again.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whole UDS messages to and from the client: the part of the stand-in ECU that depends on the
/// operating system.
pub trait Transport {
    /// The next request and the addressing it arrived with, or `None` when none arrives within
    /// `timeout` (a wait cut short by a signal also gives `None`).
    fn recv(&mut self, timeout: Duration) -> io::Result<Option<(Addressing, Vec<u8>)>>;
    /// Sends `message` as a response, whatever the addressing of the request.
    fn send(&mut self, message: &[u8]) -> io::Result<()>;
}

/// Why the ECU could not be created.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration file; the text names the file and keeps the cause.
    #[error("{0}")]
    Config(String),
    #[error("state file {}: {message}", path.display())]
    State { path: PathBuf, message: String },
    #[error("state file {} cannot be written: {source}", path.display())]
    Save { path: PathBuf, source: io::Error },
}

/// Why [`Station::run`] ended with an error. The state file keeps what the ECU did before.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("receiving a request failed: {0}")]
    Receive(io::Error),
    #[error("sending a response failed: {0}")]
    Send(io::Error),
    #[error("state file {} cannot be written: {source}", path.display())]
    Save { path: PathBuf, source: io::Error },
}

/// A simulated ECU and the state file that keeps it.
#[derive(Debug)]
pub struct Station {
    ecu: SimEcu,
    state: Option<PathBuf>,
}

impl Station {
    /// Creates the ECU. If `state` names an existing file, the ECU continues from it and `config`
    /// is not read; the ECU is then powered up again, as a restarted process stands for a power
    /// cycle: its session and security state are gone, and an ECU that went silent (an
    /// interrupted transfer, a power loss fault) answers again, with its flash progress kept.
    /// Otherwise the ECU is built from the JSON `EcuConfig` in `config`, or from the built-in
    /// configuration `sim-vci` also uses. The state file, if any, is written at once.
    pub fn start(config: Option<&Path>, state: Option<&Path>) -> Result<Self, StartError> {
        let restored = match state {
            Some(path) => load_ecu_state(path).map_err(|message| StartError::State {
                path: path.to_path_buf(),
                message,
            })?,
            None => None,
        };
        let ecu = match restored {
            Some(mut ecu) => {
                ecu.reconnect();
                ecu
            }
            None => SimEcu::new(match config {
                Some(path) => read_ecu_config(path).map_err(StartError::Config)?,
                None => default_ecu_config(),
            }),
        };
        let mut station = Self {
            ecu,
            state: state.map(Path::to_path_buf),
        };
        station
            .save()
            .map_err(|(path, source)| StartError::Save { path, source })?;
        Ok(station)
    }

    /// Writes the ECU's state to the state file, if there is one.
    fn save(&mut self) -> Result<(), (PathBuf, io::Error)> {
        match &self.state {
            Some(path) => save_ecu_state(path, &mut self.ecu).map_err(|e| (path.clone(), e)),
            None => Ok(()),
        }
    }

    fn save_or_fail(&mut self) -> Result<(), ServeError> {
        self.save()
            .map_err(|(path, source)| ServeError::Save { path, source })
    }

    /// Answers requests from `transport` until `stop` is set, then writes the state file once
    /// more (the ECU's timers are kept as the time left in them). Each message the ECU sends goes
    /// out once its delay from its request has passed; requests keep being received meanwhile,
    /// so a delayed response, a response pending chain or a repeated response holds up no other
    /// request. A stop drops the messages still waiting, as a power loss would. A request whose
    /// effect cannot be written to the state file ends the run with its response unsent, as
    /// `sim-vci` fails the request: the next process must not miss a change that was answered.
    pub fn run<T: Transport>(
        &mut self,
        transport: &mut T,
        stop: &AtomicBool,
    ) -> Result<(), ServeError> {
        // Ordered by when each message is due; messages due at the same time keep their order.
        let mut waiting: Vec<(Instant, Vec<u8>)> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let due = waiting.partition_point(|(at, _)| *at <= now);
            for (_, message) in waiting.drain(..due) {
                transport.send(&message).map_err(ServeError::Send)?;
            }
            let timeout = waiting.first().map_or(POLL_INTERVAL, |(at, _)| {
                at.saturating_duration_since(now).min(POLL_INTERVAL)
            });
            let Some((addressing, request)) =
                transport.recv(timeout).map_err(ServeError::Receive)?
            else {
                continue;
            };
            self.answer(addressing, &request, &mut waiting)?;
        }
        self.save_or_fail()
    }

    /// One exchange: the ECU handles `request`, its state is written, and the messages it sends
    /// join `waiting`, each due its delay after the request was received.
    fn answer(
        &mut self,
        addressing: Addressing,
        request: &[u8],
        waiting: &mut Vec<(Instant, Vec<u8>)>,
    ) -> Result<(), ServeError> {
        let received = Instant::now();
        let sends = self.ecu.exchange(addressing, request).into_sends();
        self.save_or_fail()?;
        for (delay_ms, message) in sends {
            let at = received + Duration::from_millis(delay_ms.into());
            let index = waiting.partition_point(|(due, _)| *due <= at);
            waiting.insert(index, (at, message));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

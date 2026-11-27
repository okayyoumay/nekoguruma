//! The exchange loop: whole UDS messages in from a [`Transport`], through [`SimEcu::exchange`],
//! the ECU's responses back out after the delays it gives, and its state kept in the state file.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sim_ecu::persist::{default_ecu_config, load_ecu_state, read_ecu_config, save_ecu_state};
use sim_ecu::{Addressing, SimEcu};

/// How long a wait for a request, or for a response's delay to pass, runs before it looks at the
/// stop flag again.
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
    /// more (the ECU's timers are kept as the time left in them). A request whose effect cannot
    /// be written to the state file ends the run with its response unsent, as `sim-vci` fails the
    /// request: the next process must not miss a change that was answered.
    pub fn run<T: Transport>(
        &mut self,
        transport: &mut T,
        stop: &AtomicBool,
    ) -> Result<(), ServeError> {
        while !stop.load(Ordering::Relaxed) {
            let Some((addressing, request)) =
                transport.recv(POLL_INTERVAL).map_err(ServeError::Receive)?
            else {
                continue;
            };
            self.answer(transport, stop, addressing, &request)?;
        }
        self.save_or_fail()
    }

    /// One exchange: the ECU handles `request`, its state is written, and each message it sends
    /// goes out once its delay from the request has passed. A stop during the wait drops the
    /// messages still waiting, as a power loss would.
    fn answer<T: Transport>(
        &mut self,
        transport: &mut T,
        stop: &AtomicBool,
        addressing: Addressing,
        request: &[u8],
    ) -> Result<(), ServeError> {
        let received = Instant::now();
        let mut sends: Vec<(u32, Vec<u8>)> = self
            .ecu
            .exchange(addressing, request)
            .into_sends()
            .collect();
        self.save_or_fail()?;
        // Stable, so messages with the same delay keep their order.
        sends.sort_by_key(|(delay_ms, _)| *delay_ms);
        for (delay_ms, message) in sends {
            if !wait_until(received + Duration::from_millis(delay_ms.into()), stop) {
                return Ok(());
            }
            transport.send(&message).map_err(ServeError::Send)?;
        }
        Ok(())
    }
}

/// Waits until `deadline`; `false` if `stop` was set first.
fn wait_until(deadline: Instant, stop: &AtomicBool) -> bool {
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(POLL_INTERVAL));
    }
}

#[cfg(test)]
mod tests;

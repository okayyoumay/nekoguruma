//! The control channel session library (design 5.2, 5.3, 6.4, 14.1; ADR-466, ADR-467, ADR-468,
//! ADR-470, ADR-471; `agentControl` in `api/asyncapi.yaml`): the agent's connection to the
//! server. It is a library with no command yet; the `connect` command and the job-command work
//! come after it.
//!
//! The API has two steps, and the library never touches the key store (the caller owns the key's
//! creation and the store's retry):
//!
//! 1. [`open`] takes the connection lock `<state>/control/connection.lock`, then the
//!    registration lock `<state>/registration/registration.lock`, then reads the registration
//!    record once with the strict reader. Each lock is tried once, and both are held until the
//!    task ends, waits included, so `ngr-agent register` exits 3 while a session lives.
//! 2. [`Opened::start`] compares the loaded key's fingerprint with the record's, spawns the loop
//!    and returns a [`Handle`] that stops it and shows its [`Status`].
//!
//! The loop is exchange, upgrade, `hello`, keepalive, wait ([`exchange`], [`channel`]). It is
//! driven only by `tokio::time`, with one writer, and stops through a `watch` signal. All socket
//! I/O goes through a private seam ([`dial`]): production opens a TCP stream to
//! `127.0.0.1:<port>` and nothing else. Only local facts end the library (a lock held, lock I/O,
//! the record absent or invalid, a key mismatch, stop); every answer of the server is retried
//! forever, one warning per failure class since the last `hello` and debug after that. The delay
//! is [`backoff`]'s.
//!
//! Never logged: the token, a challenge, a nonce, a proof, a body, a message's text, a
//! `commandId`, the text of a close reason, the record's path or content.

mod backoff;
mod channel;
mod dial;
mod exchange;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shared_crypto::proof::Audience;
use shared_crypto::{PublicKey, RandomError, Signer};
use tokio::sync::watch;
use tokio::time::Instant;
use worker_host::service::OperatingMode;

use crate::identity::AgentKey;
use crate::journal::{JournalError, WriterLock};
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{self, Record, RecordError};
use backoff::Backoff;
use dial::{Dial, Loopback};
use exchange::Fail;

/// The sub-directory of the state directory that holds the connection lock.
pub const DIR_NAME: &str = "control";
/// The connection lock's file name: held for as long as a session lives, and never deleted.
pub const LOCK_FILE: &str = "connection.lock";

/// Why [`open`] or [`Opened::start`] failed. Nothing was sent, and both locks are released. The
/// texts name no path and no content of the record.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// Another session holds the connection lock (in this process or another).
    #[error("a control channel session is already running")]
    AlreadyRunning,
    /// A registration, or the registration lock's other holder, is running.
    #[error("a registration is running")]
    RegistrationRunning,
    /// A lock or its directory cannot be used.
    #[error("cannot take a lock: {0}")]
    Lock(String),
    /// The agent has no registration record.
    #[error("the agent is not registered")]
    NotRegistered,
    /// The record cannot be read or is not valid.
    #[error(transparent)]
    Record(#[from] RecordError),
    /// The key's fingerprint is not the one the record was registered with: register again.
    #[error("the agent key is not the one the registration record names")]
    KeyMismatch,
}

/// Both locks and the record: the first step's result, not yet running. Dropping it releases the
/// locks.
pub struct Opened {
    locks: Locks,
    record: Record,
}

/// Fields drop in declaration order: the registration lock is released first and the connection
/// lock last, so a racing `open` that finds the connection lock free never fails on a
/// registration lock this session still holds, and one that finds it held sees `AlreadyRunning`.
struct Locks {
    _registration: WriterLock,
    _connection: WriterLock,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened").finish_non_exhaustive()
    }
}

/// Step 1 for `mode`: the lock directories are the mode's state sub-directories, created when
/// missing (`crate::state`).
pub fn open(mode: OperatingMode) -> Result<Opened, ControlError> {
    let dir = |name| {
        crate::state::private_subdir(mode, name)
            .map_err(|error| ControlError::Lock(error.to_string()))
    };
    let control = dir(DIR_NAME)?;
    let registration = dir(record::DIR_NAME)?;
    open_in(&control, &registration)
}

/// Step 1 in the existing directories `control_dir` and `registration_dir`.
pub fn open_in(control_dir: &Path, registration_dir: &Path) -> Result<Opened, ControlError> {
    let connection = take(&control_dir.join(LOCK_FILE), ControlError::AlreadyRunning)?;
    let registration = take(
        &registration_dir.join(record::LOCK_FILE),
        ControlError::RegistrationRunning,
    )?;
    let record = match Record::read(&registration_dir.join(record::RECORD_FILE)) {
        Ok(record) => record,
        Err(RecordError::Io(std::io::ErrorKind::NotFound)) => {
            return Err(ControlError::NotRegistered);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Opened {
        locks: Locks {
            _registration: registration,
            _connection: connection,
        },
        record,
    })
}

/// Tries one lock once: `held` when another holder has it.
fn take(path: &Path, held: ControlError) -> Result<WriterLock, ControlError> {
    WriterLock::at(path).map_err(|error| match error {
        JournalError::InUse => held,
        JournalError::Io(error) => ControlError::Lock(format!("{:?}", error.kind())),
        _ => ControlError::Lock("unexpected".to_owned()),
    })
}

/// What the loop is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The challenge and the session token.
    Exchange,
    /// The upgrade handshake.
    Upgrade,
    /// Upgraded, `hello` not yet read.
    Hello,
    /// `hello` was read: heartbeats are exchanged.
    Connected,
    /// The connection is being closed.
    Closing,
    /// Waiting before the next attempt.
    Waiting,
    /// The task has ended.
    Stopped,
}

/// What [`Handle::status`] shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    /// The interval `hello` named, while connected.
    pub heartbeat: Option<Duration>,
    /// The delay the next wait uses; while waiting, the wait in progress (a longer
    /// `Retry-After` included).
    pub next_delay: Duration,
    /// Job commands read and not answered (the answers are the job-command work's).
    pub commands_unanswered: u64,
}

/// The sending side of [`Status`].
struct StatusTx(watch::Sender<Status>);

impl StatusTx {
    fn phase(&self, phase: Phase) {
        self.0.send_if_modified(|status| {
            // The interval is shown while connected only.
            let heartbeat = if phase == Phase::Connected {
                status.heartbeat
            } else {
                None
            };
            let changed = status.phase != phase || status.heartbeat != heartbeat;
            status.phase = phase;
            status.heartbeat = heartbeat;
            changed
        });
    }

    fn connected(&self, secs: u32) {
        self.0.send_modify(|status| {
            status.phase = Phase::Connected;
            status.heartbeat = Some(Duration::from_secs(u64::from(secs)));
        });
    }

    fn command_received(&self) {
        self.0.send_modify(|status| status.commands_unanswered += 1);
    }

    fn delay(&self, delay: Duration) {
        self.0.send_modify(|status| status.next_delay = delay);
    }

    fn waiting(&self, delay: Duration) {
        self.0.send_modify(|status| {
            status.phase = Phase::Waiting;
            status.heartbeat = None;
            status.next_delay = delay;
        });
    }
}

/// The running session. Dropping it stops the session, like [`Handle::stop`].
pub struct Handle {
    stop: watch::Sender<bool>,
    status: watch::Receiver<Status>,
    done: watch::Receiver<bool>,
}

impl Handle {
    /// Asks the session to stop: a connection gets Close 1001, or is dropped when the stop comes
    /// during a stalled write. A second call does nothing more.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves once the task has ended and released both locks.
    pub async fn stopped(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }

    /// The session's phase, heartbeat interval, next delay and unanswered job commands.
    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

/// The stop signal as the loop sees it. A handle that is gone is a stop too.
#[derive(Clone)]
struct Stop(watch::Receiver<bool>);

impl Stop {
    async fn requested(&self) {
        let mut stop = self.0.clone();
        let _ = stop.wait_for(|stop| *stop).await;
    }
}

/// What the loop is wired to: the dial, the key and the record. The seams are what tests replace.
struct Wiring {
    dial: Arc<dyn Dial>,
    signer: Arc<dyn Signer + Send + Sync>,
    random: fn() -> Result<[u8; 32], RandomError>,
    record: Record,
    audience: Audience,
    /// The `Host` header's value.
    host: String,
    port: u16,
}

/// The warnings of one session: one per failure class since the last `hello`, debug after that,
/// and one per refusal until a token is issued again (the refusal flag).
#[derive(Default)]
struct Warnings {
    seen: HashSet<&'static str>,
    refused: bool,
}

impl Warnings {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        if self.seen.insert(class) {
            tracing::warn!(class, status = ?status, "control channel attempt failed");
        } else {
            tracing::debug!(class, status = ?status, "control channel attempt failed");
        }
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        if self.refused {
            tracing::debug!(class, status, "control channel refused again");
        } else {
            self.refused = true;
            tracing::warn!(class, status, "control channel refused");
        }
    }

    /// `hello` came: the classes warn again.
    fn hello(&mut self) {
        self.seen.clear();
    }

    /// A token was issued: the refusal is over.
    fn accepted(&mut self) {
        self.refused = false;
    }
}

/// What one attempt came to.
enum Outcome {
    /// Stop: the task ends.
    Stopped,
    /// A failed attempt: when it was decided, how to back off and whether the connection lived.
    Failed {
        decided_at: Instant,
        how: Fail,
        lived: bool,
    },
}

/// What a connection's loop reads and writes besides the socket.
struct SessionState<'a> {
    stop: &'a Stop,
    status: &'a StatusTx,
    warnings: &'a mut Warnings,
}

/// The test seams of [`Opened::start`].
struct Seams {
    dial: Arc<dyn Dial>,
    signer: Arc<dyn Signer + Send + Sync>,
    random: fn() -> Result<[u8; 32], RandomError>,
}

impl Opened {
    /// The registration record the session runs with.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Step 2: compares `key`'s fingerprint with the record's and starts the loop, whose first
    /// exchange begins at once. Needs a running tokio runtime. On an error nothing was sent and
    /// both locks are released.
    pub fn start(self, key: AgentKey) -> Result<Handle, ControlError> {
        let port = self.origin()?.port();
        let seams = Seams {
            dial: Arc::new(Loopback { port }),
            signer: key.signer(),
            random: shared_crypto::random_32,
        };
        self.start_with(&key.public_key(), seams)
    }

    fn origin(&self) -> Result<ServerOrigin, ControlError> {
        ServerOrigin::parse(&self.record.server_origin)
            .map_err(|_| RecordError::Field("server_origin").into())
    }

    fn start_with(self, key: &PublicKey, seams: Seams) -> Result<Handle, ControlError> {
        if key.fingerprint_hex() != self.record.agent_key_fingerprint {
            return Err(ControlError::KeyMismatch);
        }
        let origin = self.origin()?;
        let audience = Audience::parse_canonical(&self.record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        let wiring = Wiring {
            dial: seams.dial,
            signer: seams.signer,
            random: seams.random,
            record: self.record,
            audience,
            host: origin.host_header(),
            port: origin.port(),
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let (status_tx, status_rx) = watch::channel(Status {
            phase: Phase::Exchange,
            heartbeat: None,
            next_delay: backoff::INITIAL,
            commands_unanswered: 0,
        });
        let (done_tx, done_rx) = watch::channel(false);
        let guard = Guard {
            locks: Some(self.locks),
            done: done_tx,
        };
        tokio::spawn(async move {
            let _guard = guard;
            run(wiring, Stop(stop_rx), StatusTx(status_tx)).await;
        });
        Ok(Handle {
            stop: stop_tx,
            status: status_rx,
            done: done_rx,
        })
    }
}

/// Releases both locks and then says the task has ended, however it ends: it returns, panics, is
/// aborted or is dropped with its runtime. The lock files are never deleted.
struct Guard {
    locks: Option<Locks>,
    done: watch::Sender<bool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.locks.take();
        self.done.send_replace(true);
    }
}

/// The loop: attempts, each followed by a wait, until stop.
async fn run(wiring: Wiring, stop: Stop, status: StatusTx) {
    let mut backoff = Backoff::new();
    let mut warnings = Warnings::default();
    loop {
        match attempt(&wiring, &stop, &status, &mut warnings).await {
            Outcome::Stopped => break,
            Outcome::Failed {
                decided_at,
                how,
                lived,
            } => {
                let mut retry_after = None;
                match how {
                    // The cap overrides the reset (G4, G5, C5, C6).
                    Fail::Cap => backoff.cap(),
                    Fail::RetryAfter(secs) => retry_after = Some(secs),
                    Fail::Ord => {}
                }
                if lived && how != Fail::Cap {
                    backoff.reset();
                }
                let wait = backoff.wait(retry_after);
                // The wait itself, which a longer Retry-After lengthens.
                status.waiting(wait);
                // Never before the close handshake ended: the attempt returns after it.
                tokio::select! {
                    biased;
                    () = stop.requested() => break,
                    () = tokio::time::sleep_until(decided_at + wait) => {}
                }
                backoff.after_wait();
                status.delay(backoff.delay());
            }
        }
    }
    status.phase(Phase::Stopped);
}

/// One attempt: the exchange, the upgrade and the connection. A stop before the 101 drops the
/// call; after it the connection closes with 1001.
async fn attempt(w: &Wiring, stop: &Stop, status: &StatusTx, warnings: &mut Warnings) -> Outcome {
    let failed = |how| Outcome::Failed {
        decided_at: Instant::now(),
        how,
        lived: false,
    };
    status.phase(Phase::Exchange);
    let token = tokio::select! {
        biased;
        () = stop.requested() => return Outcome::Stopped,
        token = exchange::token(w, warnings) => token,
    };
    let token = match token {
        Ok(token) => token,
        Err(how) => return failed(how),
    };
    status.phase(Phase::Upgrade);
    let upgraded = tokio::select! {
        biased;
        () = stop.requested() => return Outcome::Stopped,
        upgraded = channel::upgrade(w, &token) => upgraded,
    };
    // The token served this one request (D9).
    drop(token);
    let socket = match upgraded {
        Ok(socket) => socket,
        Err(error) => {
            warnings.failure(error.class, error.status);
            return failed(Fail::Ord);
        }
    };
    status.phase(Phase::Hello);
    let mut state = SessionState {
        stop,
        status,
        warnings,
    };
    channel::serve(socket, &mut state).await
}

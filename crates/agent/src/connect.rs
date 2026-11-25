//! The flow of `ngr-agent connect` (design 5.3, 6.4; ADR-471; `crates/agent/docs/ngr-agent.md`):
//! it runs the control channel session ([`crate::control`]) until a stop signal.
//!
//! The order, where the first step that fails decides:
//!
//! 0. The installation file ([`crate::installation`], ADR-494), read by the binary before this
//!    flow starts: the origin it names is passed to [`run`].
//! 1. [`crate::control::open`]: the connection lock, the registration lock and the registration
//!    record, whose origin must be the installed one (a record for another server is exit 1,
//!    "register again"). A missing record ends the command before the key store is touched.
//! 2. The agent's outbox ([`crate::outbox::Outbox::open`], ADR-478): the file of the record's
//!    agent ID, created when missing. One that does not read back is corrupt, and the owner
//!    registers again (exit 1); another process holding it, or a directory that cannot be used,
//!    is exit 3.
//! 3. The issue-time mark ([`crate::deadline::IssueMark::open`], ADR-486): the file of the record's
//!    agent ID in the registration directory, read when present and not created. One that does
//!    not read back is corrupt and the owner registers again (exit 1); another process holding it,
//!    an unusable directory or a read that fails is exit 3.
//! 4. The received-jobs ledger ([`crate::received_jobs::ReceivedJobs::open`], ADR-489): the file
//!    of the record's agent ID in the registration directory, read when present and not created,
//!    a torn last frame cut off. One that does not read back is corrupt and the owner registers
//!    again (exit 1); another process holding it, an unusable directory or a read that fails is
//!    exit 3.
//! 5. The upload ledger ([`crate::upload_ledger::UploadLedger::open`], ADR-493 item 11): the
//!    file of the record's agent ID in `<state>/uploads/`, created when missing, an entry a
//!    crash tore cut off. One that does not read back is corrupt and the owner registers again
//!    (exit 1); another process holding it (a `debug enqueue` that has not ended, or a second
//!    agent), an unusable directory or a read that fails is exit 3. It is opened with the other
//!    state, before the key store, so that nothing starts on a file that is refused.
//! 6. The agent key, through [`crate::identity::agent_key`] (the create-once routine). A key
//!    store that is unavailable, or a creation lock held past its wait, is tried again with the
//!    session's delays until the stop signal; any other error (a refused identity directory, a
//!    lock that cannot be opened) ends the command at once. A key this run had to create, or one
//!    that is not the record's, means registering again.
//! 7. The uploader ([`crate::uploader::start`], design 5.2, ADR-493 item 10), then
//!    [`crate::control::Opened::start`]: two tasks, each with its own token exchange, and neither
//!    ends the other. Then a wait for the stop signal or for the session task to end (a task that
//!    ends alone is an error: exit 1 when its outbox or its ledger, poisoned and opened again,
//!    was corrupt, otherwise the same exit with a generic text). An uploader that ends alone
//!    (its ledger did not read back after a reopen) is logged once at error level and the session
//!    goes on: `connect`'s exit status is the session's. Then both tasks are stopped (the
//!    session closes with 1001), the session is awaited, and the uploader is awaited for
//!    [`UPLOADER_STOP_BOUND`] at most.
//!
//! The command prints and logs nothing the server sent and no token; its messages are fixed
//! wording.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use worker_host::service::OperatingMode;

use crate::control::{self, Collaborators, ControlError, LogIntake, SharedOutbox};
use crate::deadline::{IssueMark, MarkError, SystemClock};
use crate::identity::{AgentKey, IdentityError, agent_key};
use crate::log_upload::LogUploader;
use crate::outbox::{Outbox, OutboxError};
use crate::received_jobs::{LedgerError, ReceivedJobs};
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{Record, RecordError};
use crate::upload_ledger::{LedgerError as UploadLedgerError, UploadLedger};
use crate::uploader::{self, SharedLedger};

/// The first wait before the key store is tried again, as the session's own delay starts.
const RETRY_INITIAL: Duration = Duration::from_secs(1);
/// The longest wait between two tries of the key store.
const RETRY_CAP: Duration = Duration::from_secs(60);

/// How long a stop waits for the uploader to end. The uploader drops the request in flight at
/// once and awaits only a ledger write that is running, so the wait is short; the bound keeps a
/// stuck write from holding the process.
const UPLOADER_STOP_BOUND: Duration = Duration::from_secs(10);

/// The exit status of a command that did not end by a stop signal (the usage text lists them; a
/// stop is 0 and a usage error 2, which the caller decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 1: the agent cannot connect as it is: not registered, register again, no key store.
    CannotConnect,
    /// 3: a session or a registration is running, or something local is unusable for now: a lock
    /// or the state directory, the identity directory, the identity creation lock (it cannot be
    /// opened, or stays held past its wait), the key store, or a registration record that cannot
    /// be read for an I/O reason.
    Unavailable,
    /// 4: the key is present and unusable.
    KeyUnusable,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::CannotConnect => 1,
            Exit::Unavailable => 3,
            Exit::KeyUnusable => 4,
        }
    }
}

/// Why the command ended without a stop signal. The text names no path, no content of the record
/// and nothing the server sent.
#[derive(Debug)]
pub struct ConnectError {
    exit: Exit,
    message: String,
}

impl ConnectError {
    pub fn exit(&self) -> Exit {
        self.exit
    }

    pub fn exit_code(&self) -> u8 {
        self.exit.code()
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConnectError {}

fn fail(exit: Exit, message: impl Into<String>) -> ConnectError {
    ConnectError {
        exit,
        message: message.into(),
    }
}

const REGISTER_AGAIN: &str = "register again: run `ngr-agent register --replace` for this key";

fn control_error(error: &ControlError) -> ConnectError {
    match error {
        ControlError::AlreadyRunning => fail(
            Exit::Unavailable,
            "a control channel session is already running",
        ),
        ControlError::RegistrationRunning => fail(
            Exit::Unavailable,
            "a registration is running; try again when it ends",
        ),
        ControlError::Lock(_) => fail(
            Exit::Unavailable,
            "a lock or the state directory is unusable; nothing changed, try again",
        ),
        ControlError::NotRegistered => fail(
            Exit::CannotConnect,
            "not registered: run `ngr-agent register` first",
        ),
        // A record that cannot be read for an I/O reason may read later: registering again is not
        // the advice.
        ControlError::Record(RecordError::Io(kind)) => fail(
            Exit::Unavailable,
            format!("the registration record cannot be read: {kind:?}; nothing changed, try again"),
        ),
        ControlError::Record(_) => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        ControlError::KeyMismatch => fail(
            Exit::CannotConnect,
            "register again: the agent key is not the one the registration record names",
        ),
        ControlError::ServerMismatch => fail(
            Exit::CannotConnect,
            "register again: the registration record names another server than the installation \
             file; run `ngr-agent register --replace`",
        ),
    }
}

/// The exit and the text of an error that ends the open of the outbox. The texts are NGR's own.
fn outbox_error(error: &OutboxError) -> ConnectError {
    match error {
        OutboxError::Corrupt(_) => fail(
            Exit::CannotConnect,
            "the outbox is corrupt and is not repaired; register again with `ngr-agent register --replace`",
        ),
        OutboxError::BadAgentId => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        OutboxError::InUse => fail(
            Exit::Unavailable,
            "the outbox is open in another process; nothing changed, try again",
        ),
        other => fail(
            Exit::Unavailable,
            format!("the outbox cannot be opened: {other}; nothing changed, try again"),
        ),
    }
}

/// The exit and the text of an error that ends the open of the issue-time mark. The texts are
/// NGR's own.
fn mark_error(error: &MarkError) -> ConnectError {
    match error {
        MarkError::Corrupt(_) => fail(
            Exit::CannotConnect,
            "the issue-time mark file is corrupt and is not repaired; register again with `ngr-agent register --replace`",
        ),
        MarkError::BadAgentId => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        MarkError::InUse => fail(
            Exit::Unavailable,
            "the issue-time mark is open in another process; nothing changed, try again",
        ),
        other => fail(
            Exit::Unavailable,
            format!("the issue-time mark cannot be opened: {other}; nothing changed, try again"),
        ),
    }
}

/// The exit and the text of an error that ends the open of the received-jobs ledger. The texts
/// are NGR's own.
fn ledger_error(error: &LedgerError) -> ConnectError {
    match error {
        LedgerError::Corrupt(_) => fail(
            Exit::CannotConnect,
            "the received-jobs ledger is corrupt and is not repaired; register again with `ngr-agent register --replace`",
        ),
        LedgerError::BadAgentId => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        LedgerError::InUse => fail(
            Exit::Unavailable,
            "the received-jobs ledger is open in another process; nothing changed, try again",
        ),
        other => fail(
            Exit::Unavailable,
            format!(
                "the received-jobs ledger cannot be opened: {other}; nothing changed, try again"
            ),
        ),
    }
}

/// The exit and the text of an error that ends the open of the upload ledger. The texts are NGR's
/// own.
fn upload_ledger_error(error: &UploadLedgerError) -> ConnectError {
    match error {
        UploadLedgerError::Corrupt(_) => fail(
            Exit::CannotConnect,
            "the upload ledger is corrupt and is not repaired; register again with `ngr-agent register --replace`",
        ),
        UploadLedgerError::BadAgentId => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        UploadLedgerError::InUse => fail(
            Exit::Unavailable,
            "the upload ledger is open in another process; nothing changed, try again",
        ),
        other => fail(
            Exit::Unavailable,
            format!("the upload ledger cannot be opened: {other}; nothing changed, try again"),
        ),
    }
}

/// The exit and the text of a key error that ends the load, as `register` reports it. The retried
/// errors ([`IdentityError::retry_class`]) never get here.
fn identity_error(error: &IdentityError) -> ConnectError {
    let exit = match error {
        IdentityError::Unavailable(_)
        | IdentityError::CreationLockBusy
        | IdentityError::LocalFault(_) => Exit::Unavailable,
        IdentityError::KeyUnusable(_) => Exit::KeyUnusable,
        IdentityError::NoStore(_) | IdentityError::NotProvisioned | IdentityError::Random(_) => {
            Exit::CannotConnect
        }
    };
    fail(exit, error.to_string())
}

/// Runs the session of `mode` to the server `installed` (the installation file's) until `stop`
/// resolves. `Ok` is a stop by the signal.
pub async fn run(
    mode: OperatingMode,
    installed: &ServerOrigin,
    stop: impl Future<Output = ()>,
) -> Result<(), ConnectError> {
    // Both locks and the record first: no record ends the command with the key store untouched.
    let opened = control::open(mode, installed).map_err(|error| control_error(&error))?;
    // After the record (it names the agent ID) and before the key store, which a corrupt outbox
    // need not wait for. Dropping `opened` on the error releases both locks.
    let outbox =
        Outbox::open(mode, &opened.record().agent_id).map_err(|error| outbox_error(&error))?;
    let outbox = SharedOutbox::new(outbox);
    let marks =
        IssueMark::open(mode, &opened.record().agent_id).map_err(|error| mark_error(&error))?;
    let ledger = ReceivedJobs::open(mode, &opened.record().agent_id)
        .map_err(|error| ledger_error(&error))?;
    let uploads = UploadLedger::open(mode, &opened.record().agent_id)
        .map_err(|error| upload_ledger_error(&error))?;
    let uploads = SharedLedger::new(uploads);
    let log_uploads = LogUploader::new(mode, ledger.clone(), uploads.clone());
    run_opened(
        opened,
        move || agent_key(mode),
        move |record, key| uploader::start(record, key, uploads),
        |opened, key| {
            opened.start(
                key,
                Collaborators {
                    intake: Arc::new(LogIntake),
                    outbox,
                    marks,
                    clock: Arc::new(SystemClock),
                    ledger,
                    log_uploads: Arc::new(log_uploads),
                },
            )
        },
        UPLOADER_STOP_BOUND,
        stop,
    )
    .await
}

/// The steps after `open`, with the key's source and the session's start as seams for tests.
async fn run_opened(
    opened: control::Opened,
    load_key: impl Fn() -> Result<AgentKey, IdentityError> + Clone + Send + 'static,
    start_uploader: impl FnOnce(&Record, &AgentKey) -> Result<uploader::Handle, ControlError>,
    start: impl FnOnce(control::Opened, AgentKey) -> Result<control::Handle, ControlError>,
    uploader_bound: Duration,
    stop: impl Future<Output = ()>,
) -> Result<(), ConnectError> {
    tokio::pin!(stop);
    let Some(key) = load_key_retrying(load_key, stop.as_mut()).await? else {
        tracing::info!("control channel session stopped before it started");
        return Ok(());
    };
    // A key this run created cannot be the record's: the server never saw it.
    if key.created() {
        return Err(fail(Exit::CannotConnect, REGISTER_AGAIN));
    }
    // The record's checks come before either task is spawned (the session makes them again).
    opened
        .check(&key.public_key())
        .map_err(|error| control_error(&error))?;
    // The uploader first, over the key by reference: the session's start takes the key. A start
    // that fails drops the uploader's handle, which stops it.
    let uploader = start_uploader(opened.record(), &key).map_err(|error| control_error(&error))?;
    let handle = start(opened, key).map_err(|error| control_error(&error))?;
    tracing::info!("control channel session started");
    // The task also ends without a stop when it panics (release builds unwind): the locks are
    // then free and no session is left, so the process must not stay up.
    let mut uploader_ended = false;
    let outcome = loop {
        tokio::select! {
            biased;
            () = stop.as_mut() => break Ok(()),
            () = handle.stopped() => {
                break Err(match handle.ended() {
                    Some(control::Ended::OutboxUnusable) => {
                        outbox_error(&OutboxError::Corrupt("reopen"))
                    }
                    Some(control::Ended::LedgerUnusable) => {
                        ledger_error(&LedgerError::Corrupt("reopen"))
                    }
                    None => fail(
                        Exit::CannotConnect,
                        "the control channel session ended unexpectedly",
                    ),
                });
            }
            // Neither channel ends the other: the session goes on without uploads.
            () = uploader.stopped(), if !uploader_ended => {
                uploader_ended = true;
                tracing::error!(
                    reason = ?uploader.ended(),
                    "the uploader ended; nothing is uploaded until the agent restarts, and the control channel session goes on"
                );
            }
        }
    };
    if outcome.is_ok() {
        handle.stop();
        handle.stopped().await;
        tracing::info!("control channel session stopped");
    }
    // The uploader is awaited on the error path too: the process then ends within a short
    // timeout, which must not cut a ledger rewrite that is running.
    uploader.stop();
    if !uploader_ended {
        if tokio::time::timeout(uploader_bound, uploader.stopped())
            .await
            .is_err()
        {
            tracing::warn!("the uploader did not stop in time; the agent exits without it");
        } else {
            tracing::info!("uploader stopped");
        }
    }
    outcome
}

/// Loads the key, trying again after each unavailable store or busy creation lock with the
/// session's delays (1 second, doubling to 60) until it comes or `stop` resolves (`None`). Any
/// other error ends the load.
async fn load_key_retrying(
    load_key: impl Fn() -> Result<AgentKey, IdentityError> + Clone + Send + 'static,
    mut stop: std::pin::Pin<&mut impl Future<Output = ()>>,
) -> Result<Option<AgentKey>, ConnectError> {
    let mut delay = RETRY_INITIAL;
    loop {
        // The store and the creation lock block, so the call runs off the runtime's threads.
        let load = tokio::task::spawn_blocking(load_key.clone());
        let result = tokio::select! {
            biased;
            () = stop.as_mut() => return Ok(None),
            result = load => result,
        };
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                return Err(fail(
                    Exit::CannotConnect,
                    "the key load failed unexpectedly",
                ));
            }
        };
        match result {
            Ok(key) => return Ok(Some(key)),
            Err(error) => {
                let Some(class) = error.retry_class() else {
                    return Err(identity_error(&error));
                };
                tracing::warn!(
                    class,
                    retry_in_secs = delay.as_secs(),
                    "the agent key is unavailable; trying again"
                );
                tokio::select! {
                    biased;
                    () = stop.as_mut() => return Ok(None),
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(RETRY_CAP);
            }
        }
    }
}

/// The stop signal: SIGINT or SIGTERM on Unix, Ctrl-C or the console closing on Windows. The
/// handlers are installed when this returns, and the future resolves on the first signal.
#[cfg(unix)]
pub fn stop_signal() -> std::io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

/// The stop signal (see the Unix variant).
#[cfg(windows)]
pub fn stop_signal() -> std::io::Result<impl Future<Output = ()>> {
    use tokio::signal::windows::{ctrl_c, ctrl_close};
    let mut interrupt = ctrl_c()?;
    let mut close = ctrl_close()?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = close.recv() => {}
        }
    })
}

#[cfg(test)]
mod tests;

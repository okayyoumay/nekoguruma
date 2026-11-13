//! The flow of `ngr-agent connect` (design 5.3, 6.4; ADR-471; `crates/agent/docs/ngr-agent.md`):
//! it runs the control channel session ([`crate::control`]) until a stop signal.
//!
//! The order, where the first step that fails decides:
//!
//! 1. [`crate::control::open`]: the connection lock, the registration lock and the registration
//!    record. A missing record ends the command before the key store is touched.
//! 2. The agent key, through [`crate::identity::agent_key`] (the create-once routine). A key
//!    store that is unavailable is tried again with the session's delays until the stop signal; a
//!    key this run had to create, or one that is not the record's, means registering again.
//! 3. [`crate::control::Opened::start`], then a wait for the stop signal, then
//!    [`crate::control::Handle::stop`] (Close 1001) and a wait for the task to end.
//!
//! The command prints and logs nothing the server sent and no token; its messages are fixed
//! wording.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use worker_host::service::OperatingMode;

use crate::control::{self, ControlError, LogIntake};
use crate::identity::{AgentKey, IdentityError, agent_key};

/// The first wait before the key store is tried again, as the session's own delay starts.
const RETRY_INITIAL: Duration = Duration::from_secs(1);
/// The longest wait between two tries of the key store.
const RETRY_CAP: Duration = Duration::from_secs(60);

/// The exit status of a command that did not end by a stop signal (the usage text lists them; a
/// stop is 0 and a usage error 2, which the caller decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// 1: the agent cannot connect as it is: not registered, register again, no key store.
    CannotConnect,
    /// 3: a session or a registration is running, or a lock or the state directory is unusable.
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

/// Why the command ended without a stop signal. The text is fixed wording: no path, no content of
/// the record and nothing the server sent.
#[derive(Debug)]
pub struct ConnectError {
    exit: Exit,
    message: &'static str,
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
        f.write_str(self.message)
    }
}

impl std::error::Error for ConnectError {}

fn fail(exit: Exit, message: &'static str) -> ConnectError {
    ConnectError { exit, message }
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
        ControlError::Record(_) => fail(
            Exit::CannotConnect,
            "the registration record cannot be used; register again with `ngr-agent register --replace`",
        ),
        ControlError::KeyMismatch => fail(
            Exit::CannotConnect,
            "register again: the agent key is not the one the registration record names",
        ),
    }
}

fn identity_error(error: &IdentityError) -> ConnectError {
    match error {
        IdentityError::Unavailable(_) => fail(Exit::Unavailable, "the key store is unavailable"),
        IdentityError::KeyUnusable(_) => fail(
            Exit::KeyUnusable,
            "the stored agent key cannot be used; it is not replaced",
        ),
        IdentityError::NoStore(_) => fail(
            Exit::CannotConnect,
            "no key store: this build or mode has none, or the debug variables are wrong",
        ),
        IdentityError::NotProvisioned => fail(
            Exit::CannotConnect,
            "the store holds no agent key and cannot create one; an administrator provisions it",
        ),
        IdentityError::Random(_) => fail(Exit::CannotConnect, "cannot create the agent key"),
    }
}

/// Runs the session of `mode` until `stop` resolves. `Ok` is a stop by the signal.
pub async fn run(mode: OperatingMode, stop: impl Future<Output = ()>) -> Result<(), ConnectError> {
    // Both locks and the record first: no record ends the command with the key store untouched.
    let opened = control::open(mode).map_err(|error| control_error(&error))?;
    run_opened(opened, move || agent_key(mode), stop).await
}

/// The steps after `open`, with the key's source as a seam for tests.
async fn run_opened(
    opened: control::Opened,
    load_key: impl Fn() -> Result<AgentKey, IdentityError> + Clone + Send + 'static,
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
    let handle = opened
        .start(key, Arc::new(LogIntake))
        .map_err(|error| control_error(&error))?;
    tracing::info!("control channel session started");
    stop.await;
    handle.stop();
    handle.stopped().await;
    tracing::info!("control channel session stopped");
    Ok(())
}

/// Loads the key, trying again after each unavailable store with the session's delays (1 second,
/// doubling to 60) until it comes or `stop` resolves (`None`). Any other error ends the load.
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
            Err(error) if error.retryable() => {
                tracing::warn!(
                    retry_in_secs = delay.as_secs(),
                    "the agent key store is unavailable; trying again"
                );
                tokio::select! {
                    biased;
                    () = stop.as_mut() => return Ok(None),
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(RETRY_CAP);
            }
            Err(error) => return Err(identity_error(&error)),
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

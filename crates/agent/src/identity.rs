//! The agent's identity: its P-256 key in a named-secret store (design 6.4, ADR-459 Decision item
//! 4, ADR-460).
//!
//! [`load_or_create`] reads the key and, only when the store positively says it holds none,
//! creates it once: it takes an exclusive lock file beside the state, reads again, generates the
//! key, creates it in the store (never replacing), reads it back and hands out what it read. A
//! present key needs neither the lock nor a writable state directory. A store that cannot be
//! read is [`IdentityError::Unavailable`], which the connect loop retries and which is never read
//! as an absent key; a key that cannot be decoded is [`IdentityError::KeyUnusable`], which asks
//! for registration again and is never overwritten.
//!
//! Only the commands that need the key call this (`ngr-agent identity`, later registration and
//! connect): they call [`load_or_create`] or [`agent_key`], never the store's `put_new`.
//! `ngr-agent run` and `identify` never touch the store.

pub mod store;

#[cfg(debug_assertions)]
pub mod file_store;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shared_crypto::{PublicKey, RandomError, Signer, SigningKey};
use worker_host::service::OperatingMode;

use crate::journal::{JournalError, WriterLock};
use crate::state::StateDirError;
use store::{SecretName, SecretStore, StoreError, StoreErrorKind};

/// How long a process that finds the creation lock held waits for it: the holder is creating the
/// key, and the waiter then adopts it. A holder stuck longer makes the waiter report the store
/// unavailable.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often a waiting process tries the lock again (as `guards::LockFile` does).
const LOCK_POLL: Duration = Duration::from_millis(20);

/// The debug-only variable that selects the plain-file test store (value `file`).
pub const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";

/// Why the agent has no key.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// This build, platform or mode has no key store, or the debug variables that select the
    /// test store are wrong. Nothing was written.
    #[error("no key store: {0}")]
    NoStore(&'static str),
    /// The store, the lock or the state directory cannot be used now; nothing changed and a
    /// retry may work.
    #[error("{0}")]
    Unavailable(String),
    /// A key is present and cannot be used; the store is untouched. Registering again needs an
    /// explicit reset.
    #[error("the stored agent key cannot be used ({0}); it is not replaced")]
    KeyUnusable(&'static str),
    /// The store holds no key and cannot create one: an administrator provisions it.
    #[error("the store holds no agent key and cannot create one; an administrator provisions it")]
    NotProvisioned,
    #[error(transparent)]
    Random(#[from] RandomError),
}

impl IdentityError {
    /// Whether a retry may succeed: only for [`IdentityError::Unavailable`].
    pub fn retryable(&self) -> bool {
        matches!(self, IdentityError::Unavailable(_))
    }
}

/// The agent's key, loaded once and held in memory, so a store that locks later does not stop a
/// reconnection.
pub struct AgentKey {
    key: Arc<SigningKey>,
    created: bool,
}

impl AgentKey {
    /// The signer for connection proofs.
    pub fn signer(&self) -> Arc<dyn Signer + Send + Sync> {
        self.key.clone()
    }

    pub fn public_key(&self) -> PublicKey {
        self.key.public_key()
    }

    /// Whether this call created the key.
    pub fn created(&self) -> bool {
        self.created
    }
}

impl std::fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentKey")
            .field("key", &self.key)
            .field("created", &self.created)
            .finish()
    }
}

/// The key of `mode` in the store this build selects, created once if the store has none.
pub fn agent_key(mode: OperatingMode) -> Result<AgentKey, IdentityError> {
    let store = select_store(mode, &|name| std::env::var_os(name))?;
    load_or_create(
        &*store,
        &SecretName::agent_key(mode),
        &|| crate::state::private_subdir(mode, "identity"),
        LOCK_WAIT,
    )
}

/// The store this build uses. A release build has none until the platform stores exist; a debug
/// build has the plain-file test store when `NGR_AGENT_KEY_STORE=file` and an absolute
/// `NGR_AGENT_STATE_DIR` are both set (ADR-460 Decision item 6).
#[cfg(debug_assertions)]
fn select_store(
    _mode: OperatingMode,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    match var(KEY_STORE_VAR) {
        None => Err(IdentityError::NoStore(
            "this build has no key store for this platform or mode",
        )),
        Some(value) if value == "file" => {
            let root = var(crate::state::STATE_DIR_VAR)
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .ok_or(IdentityError::NoStore(
                    "the file store needs an absolute NGR_AGENT_STATE_DIR",
                ))?;
            Ok(Box::new(file_store::DebugFileStore::new(
                root.join("identity"),
            )))
        }
        Some(_) => Err(IdentityError::NoStore(
            "NGR_AGENT_KEY_STORE must be \"file\"",
        )),
    }
}

/// The store this build uses: none yet (the platform stores come later, ADR-459 Decision item 4).
#[cfg(not(debug_assertions))]
fn select_store(
    _mode: OperatingMode,
    _var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    Err(IdentityError::NoStore(
        "this build has no key store for this platform or mode",
    ))
}

/// Reads `name`: `Ok(None)` when the store positively holds nothing.
fn read(store: &dyn SecretStore, name: &SecretName) -> Result<Option<SigningKey>, IdentityError> {
    match store.get(name) {
        Ok(bytes) => SigningKey::from_pkcs8_der(&bytes)
            .map(Some)
            .map_err(|_| IdentityError::KeyUnusable("not a P-256 private key")),
        Err(error) => match error.kind() {
            StoreErrorKind::Absent => Ok(None),
            StoreErrorKind::Undecryptable => {
                Err(IdentityError::KeyUnusable("the store cannot decrypt it"))
            }
            _ => Err(unavailable("the key store", &error)),
        },
    }
}

fn unavailable(what: &str, error: &dyn std::error::Error) -> IdentityError {
    IdentityError::Unavailable(format!("{what} is unavailable: {error}"))
}

/// Loads the key stored under `name`, creating it once when the store has none (the steps are
/// in the module's documentation).
///
/// `lock_dir` makes and returns the directory of the lock file; it is called only when the key
/// is absent. `wait` bounds the wait for another creator.
pub fn load_or_create(
    store: &dyn SecretStore,
    name: &SecretName,
    lock_dir: &dyn Fn() -> Result<PathBuf, StateDirError>,
    wait: Duration,
) -> Result<AgentKey, IdentityError> {
    let adopt = |key: SigningKey, created: bool| AgentKey {
        key: Arc::new(key),
        created,
    };
    if let Some(key) = read(store, name)? {
        return Ok(adopt(key, false));
    }
    let dir = lock_dir().map_err(|error| unavailable("the state directory", &error))?;
    let _lock = take_lock(&dir.join(format!("{name}.lock")), wait)?;
    // Another process may have created the key while this one waited.
    if let Some(key) = read(store, name)? {
        return Ok(adopt(key, false));
    }
    let generated = SigningKey::generate()?;
    match store.put_new(name, &generated.to_pkcs8_der()) {
        Ok(()) => {}
        // A writer outside the lock got there first: take its key.
        Err(error) if error.kind() == StoreErrorKind::AlreadyExists => {
            return read(store, name)?
                .map(|key| adopt(key, false))
                .ok_or_else(|| unavailable("the key store", &error));
        }
        Err(error) if error.kind() == StoreErrorKind::NotCreatable => {
            return Err(IdentityError::NotProvisioned);
        }
        Err(error) => return Err(unavailable("the key store", &error)),
    }
    // Hand out what the store holds, not what was generated.
    let stored = read(store, name)?
        .ok_or_else(|| unavailable("the key store", &StoreError::new(StoreErrorKind::Absent)))?;
    if stored.public_key() != generated.public_key() {
        tracing::warn!(
            "the stored agent key differs from the one just created; using the stored one"
        );
        return Ok(adopt(stored, false));
    }
    Ok(adopt(stored, true))
}

/// Takes the exclusive lock on the file `path`, waiting up to `wait`.
fn take_lock(path: &std::path::Path, wait: Duration) -> Result<WriterLock, IdentityError> {
    let deadline = Instant::now() + wait;
    loop {
        match WriterLock::at(path) {
            Ok(lock) => return Ok(lock),
            Err(JournalError::InUse) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(JournalError::InUse) => {
                return Err(IdentityError::Unavailable(
                    "another process is creating the agent key and did not finish in time"
                        .to_owned(),
                ));
            }
            Err(error) => return Err(unavailable("the creation lock", &error)),
        }
    }
}

#[cfg(test)]
mod tests;

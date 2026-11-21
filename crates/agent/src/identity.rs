//! The agent's identity: its P-256 key in a named-secret store (design 6.4, ADR-459 Decision item
//! 4, ADR-460).
//!
//! [`load_or_create`] reads the key and, only when the store positively says it holds none,
//! creates it once: it takes an exclusive lock file beside the state, reads again, generates the
//! key, creates it in the store (never replacing), reads it back and hands out what it read. A
//! present key needs neither the lock nor a writable state directory. A store that cannot be
//! read is [`IdentityError::Unavailable`] and a creation lock held past its wait is
//! [`IdentityError::CreationLockBusy`]: the connect loop retries both, and neither is read as an
//! absent key. A refused identity directory or a creation lock that cannot be opened is
//! [`IdentityError::LocalFault`], which no retry fixes; a key that cannot be decoded is [`IdentityError::KeyUnusable`], which asks
//! for registration again and is never overwritten.
//!
//! The store is chosen by build, platform and mode ([`select_store`]): on Linux in the user mode
//! it is the Secret Service store in [`secret_service`] (ADR-496), which a release build selects
//! without a variable; in the device mode it is the systemd credential in [`systemd_credential`]
//! (ADR-498), which a release build selects from `CREDENTIALS_DIRECTORY` alone. A debug build
//! selects either, or the plain-file test store, by `NGR_AGENT_KEY_STORE`. Windows has none yet.
//! A store that cannot create ([`SecretStore::can_create`], the credential store) turns an absent
//! key into [`IdentityError::NotProvisioned`] at once: `ngr-agent provision-key` ([`provision`])
//! creates the device key instead.
//!
//! Only the commands that need the key call this (`ngr-agent identity`, later registration and
//! connect): they call [`load_or_create`] or [`agent_key`], never the store's `put_new`.
//! `ngr-agent run` and `identify` never touch the store.

pub mod store;

#[cfg(debug_assertions)]
pub mod file_store;

#[cfg(target_os = "linux")]
pub mod secret_service;

#[cfg(target_os = "linux")]
pub mod systemd_credential;

#[cfg(target_os = "linux")]
pub mod provision;

use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shared_crypto::{PublicKey, RandomError, Signer, SigningKey};
use worker_host::service::OperatingMode;

use crate::journal::{JournalError, WriterLock};
use crate::state::StateDirError;
use store::{SecretName, SecretStore, StoreError, StoreErrorKind};

/// How long a process that finds the creation lock held waits for it: the holder is creating the
/// key, and the waiter then adopts it. A holder stuck longer makes the waiter report the creation lock
/// busy.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often a waiting process tries the lock again (as `guards::LockFile` does).
const LOCK_POLL: Duration = Duration::from_millis(20);

/// The debug-only variable that selects a store by value: `file` (the plain-file test store) or
/// `secret-service` (the Linux user-mode store, which a release build selects without it) or
/// `systemd-credential` (the Linux device-mode store, which a release build selects from
/// `CREDENTIALS_DIRECTORY`).
pub const KEY_STORE_VAR: &str = "NGR_AGENT_KEY_STORE";

/// The device key's credential name (`SecretName::agent_key` for the device mode): the unit's
/// `LoadCredentialEncrypted=` ID, the file name in the credential directory and in the credential
/// store, and the name `systemd-creds` binds the ciphertext to (ADR-498).
pub const DEVICE_CREDENTIAL_NAME: &str = "nekoguruma-agent-key.device.v1";

/// The directory the administrator's encrypted device key lives in (ADR-498 item 2); the
/// packaging checks the unit against these constants on every platform.
pub const DEVICE_CREDSTORE_DIR: &str = "/etc/credstore.encrypted";

/// The encrypted device key: [`DEVICE_CREDSTORE_DIR`] and [`DEVICE_CREDENTIAL_NAME`].
pub const DEVICE_CREDENTIAL_PATH: &str = "/etc/credstore.encrypted/nekoguruma-agent-key.device.v1";

/// The variable systemd sets to the directory of a service's credentials.
pub const CREDENTIALS_DIRECTORY_VAR: &str = "CREDENTIALS_DIRECTORY";

/// What the credential is sealed to (`--with-key` of `systemd-creds`): a closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithKey {
    /// The tool picks the TPM when there is one and the host key otherwise.
    Auto,
    /// The host key under `/var/lib/systemd`.
    Host,
    Tpm2,
    HostTpm2,
}

impl WithKey {
    /// The value the tool's `--with-key` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            WithKey::Auto => "auto",
            WithKey::Host => "host",
            WithKey::Tpm2 => "tpm2",
            WithKey::HostTpm2 => "host+tpm2",
        }
    }
}

impl FromStr for WithKey {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "auto" => Ok(WithKey::Auto),
            "host" => Ok(WithKey::Host),
            "tpm2" => Ok(WithKey::Tpm2),
            "host+tpm2" => Ok(WithKey::HostTpm2),
            _ => Err("must be auto, host, tpm2 or host+tpm2"),
        }
    }
}

/// Why the agent has no key.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// This build, platform or mode has no key store, or the debug variables that select the
    /// test store are wrong. Nothing was written.
    #[error("no key store: {0}")]
    NoStore(&'static str),
    /// The key store cannot be used now; nothing changed and a retry may work.
    #[error("{0}")]
    Unavailable(String),
    /// Another process held the creation lock past the wait; nothing changed and a retry may
    /// work.
    #[error("another process is creating the agent key and did not finish in time")]
    CreationLockBusy,
    /// The identity directory or the creation lock cannot be used (the directory is refused, the
    /// lock file cannot be opened); nothing changed and a retry does not help.
    #[error("{0}")]
    LocalFault(String),
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
    /// Whether a retry may succeed: a key store that is unavailable
    /// ([`IdentityError::Unavailable`]) and a creation lock held past its wait
    /// ([`IdentityError::CreationLockBusy`]).
    pub fn retryable(&self) -> bool {
        self.retry_class().is_some()
    }

    /// The fixed class a retry's warning names, or `None` for an error no retry fixes. The one
    /// exhaustive match on the variants: a new variant must choose here.
    pub fn retry_class(&self) -> Option<&'static str> {
        match self {
            IdentityError::Unavailable(_) => Some("key store"),
            IdentityError::CreationLockBusy => Some("creation lock"),
            IdentityError::NoStore(_)
            | IdentityError::LocalFault(_)
            | IdentityError::KeyUnusable(_)
            | IdentityError::NotProvisioned
            | IdentityError::Random(_) => None,
        }
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

/// The Linux user-mode store: the freedesktop Secret Service (ADR-459 Decision item 4). Other
/// platforms and the device mode have none yet.
#[cfg(target_os = "linux")]
fn user_mode_store(mode: OperatingMode) -> Option<Box<dyn SecretStore>> {
    (mode == OperatingMode::User).then(|| Box::new(secret_service::SecretService::new()) as _)
}

#[cfg(not(target_os = "linux"))]
fn user_mode_store(_mode: OperatingMode) -> Option<Box<dyn SecretStore>> {
    None
}

const NO_STORE: &str = "this build has no key store for this platform or mode";

/// The Linux device-mode store: the systemd credential in the directory `CREDENTIALS_DIRECTORY`
/// names (ADR-498 item 5). The variable must be an absolute path; unset, empty or relative is no
/// store, with the way to run the command. Other modes and platforms have none.
#[cfg(target_os = "linux")]
fn credential_store(
    mode: OperatingMode,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    if mode != OperatingMode::Device {
        return Err(IdentityError::NoStore(
            "the systemd-credential store is for the device mode",
        ));
    }
    let dir = var(CREDENTIALS_DIRECTORY_VAR)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .ok_or(IdentityError::NoStore(
            "CREDENTIALS_DIRECTORY is not an absolute directory: start the command from its \
             systemd unit, or with systemd-run and LoadCredentialEncrypted= (ngr-agent.md)",
        ))?;
    Ok(Box::new(systemd_credential::SystemdCredential::new(dir)))
}

#[cfg(not(target_os = "linux"))]
fn credential_store(
    _mode: OperatingMode,
    _var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    Err(IdentityError::NoStore(NO_STORE))
}

/// The store this build uses. A debug build has the plain-file test store when
/// `NGR_AGENT_KEY_STORE=file` and an absolute `NGR_AGENT_STATE_DIR` are both set (ADR-460
/// Decision item 6), and the store of the platform when `NGR_AGENT_KEY_STORE` is
/// `secret-service` (user mode) or `systemd-credential` (device mode); with the variable unset
/// it has none, as a release build has none for a platform or mode without a store.
#[cfg(debug_assertions)]
fn select_store(
    mode: OperatingMode,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    match var(KEY_STORE_VAR) {
        None => Err(IdentityError::NoStore(NO_STORE)),
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
        Some(value) if value == "secret-service" => user_mode_store(mode).ok_or(
            IdentityError::NoStore("the secret-service store is for the user mode on Linux"),
        ),
        Some(value) if value == "systemd-credential" => credential_store(mode, var),
        Some(_) => Err(IdentityError::NoStore(
            "NGR_AGENT_KEY_STORE must be \"file\", \"secret-service\" or \"systemd-credential\"",
        )),
    }
}

/// The store this build uses: on Linux the Secret Service store in user mode and the systemd
/// credential in device mode (which reads `CREDENTIALS_DIRECTORY` and no `NGR_` variable), or
/// none (ADR-459 Decision item 4, ADR-496 item 7, ADR-498 item 5).
#[cfg(not(debug_assertions))]
fn select_store(
    mode: OperatingMode,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<Box<dyn SecretStore>, IdentityError> {
    match mode {
        OperatingMode::Device => credential_store(mode, var),
        _ => user_mode_store(mode).ok_or(IdentityError::NoStore(NO_STORE)),
    }
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

fn local_fault(what: &str, error: &dyn std::error::Error) -> IdentityError {
    IdentityError::LocalFault(format!("{what} cannot be used: {error}"))
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
    // A store that never creates has nothing to race for: no lock, no state directory, no key.
    if !store.can_create() {
        return Err(IdentityError::NotProvisioned);
    }
    let dir = lock_dir().map_err(|error| local_fault("the state directory", &error))?;
    let _lock = take_lock(&dir.join(format!("{name}.lock")), wait)?;
    // Another process may have created the key while this one waited.
    if let Some(key) = read(store, name)? {
        return Ok(adopt(key, false));
    }
    let generated = SigningKey::generate()?;
    match store.put_new(name, &generated.to_pkcs8_der()) {
        Ok(()) => {}
        Err(error) if error.kind() == StoreErrorKind::NotCreatable => {
            return Err(IdentityError::NotProvisioned);
        }
        // A writer outside the lock got there first: take its key.
        Err(error) if error.kind() == StoreErrorKind::AlreadyExists => {
            return read(store, name)?
                .map(|key| adopt(key, false))
                .ok_or_else(|| unavailable("the key store", &error));
        }
        // A create that failed after the key was in place (a sync that failed) still left a key:
        // use it, as another creator's, since this call cannot say it made it. Without a
        // readable key the failure stands and nothing is handed out.
        Err(error) => {
            return match read(store, name) {
                Ok(Some(key)) => Ok(adopt(key, false)),
                _ => Err(unavailable("the key store", &error)),
            };
        }
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
    // A wait too long to add to the clock never ends.
    let deadline = Instant::now().checked_add(wait);
    loop {
        match WriterLock::at(path) {
            Ok(lock) => return Ok(lock),
            Err(JournalError::InUse) if deadline.is_none_or(|end| Instant::now() < end) => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(JournalError::InUse) => return Err(IdentityError::CreationLockBusy),
            Err(error) => return Err(local_fault("the creation lock", &error)),
        }
    }
}

#[cfg(test)]
mod tests;

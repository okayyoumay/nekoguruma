//! The named-secret store the agent key lives in (design 6.4, ADR-459 Decision item 4,
//! ADR-460 Decision item 3).
//!
//! A store holds secrets by name and can only create them: it has no replace and no delete, so
//! no code path of the agent can overwrite or lose a key (ADR-387). The platform stores
//! (DPAPI blob, Secret Service, systemd credential) implement [`SecretStore`] later.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use shared_crypto::Zeroizing;
use worker_host::service::OperatingMode;

/// What a store's name for a secret looks like: lowercase letters, digits, `.` and `-`, 1 to 64
/// bytes. The agent key's name holds the mode and a format version and no host or account name,
/// so it means the same on every device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretName(String);

impl SecretName {
    /// A name from `text`, or `None` when it breaks the rule above.
    pub fn new(text: &str) -> Option<Self> {
        let ok = (1..=64).contains(&text.len())
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        ok.then(|| Self(text.to_owned()))
    }

    /// The agent key's name for `mode`: `nekoguruma-agent-key.<mode>.v1`. A later format reads
    /// the earlier names before it creates one (ADR-460 Decision item 2).
    pub fn agent_key(mode: OperatingMode) -> Self {
        Self(format!("nekoguruma-agent-key.{mode}.v1"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a store call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    /// The store answered that the name holds nothing. Only a positive answer is this.
    Absent,
    /// The store cannot be reached or read right now (locked collection, no daemon, an I/O or
    /// permission error); a retry may work, and it is never read as [`Absent`](Self::Absent).
    Unavailable,
    /// The store holds the name but cannot decrypt it.
    Undecryptable,
    /// `put_new` found the name already present.
    AlreadyExists,
    /// The store cannot create secrets (an administrator provisions them).
    NotCreatable,
}

/// A store call failed. The text names the kind only; the optional source is for logs, and a
/// store must put no secret material in it.
#[derive(Debug)]
pub struct StoreError {
    kind: StoreErrorKind,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl StoreError {
    pub fn new(kind: StoreErrorKind) -> Self {
        Self { kind, source: None }
    }

    pub fn with_source(
        kind: StoreErrorKind,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            source: Some(source.into()),
        }
    }

    pub fn kind(&self) -> StoreErrorKind {
        self.kind
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.kind {
            StoreErrorKind::Absent => "the secret is not in the store",
            StoreErrorKind::Unavailable => "the secret store is unavailable",
            StoreErrorKind::Undecryptable => "the store cannot decrypt the secret",
            StoreErrorKind::AlreadyExists => "the secret already exists",
            StoreErrorKind::NotCreatable => "the store does not create secrets",
        })?;
        if let Some(source) = &self.source {
            write!(f, " ({source})")?;
        }
        Ok(())
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

/// A store of named secrets that can read and create, never replace or delete.
pub trait SecretStore: Send + Sync {
    /// The secret under `name`. [`StoreErrorKind::Absent`] only when the store positively
    /// answers that the name holds nothing; any other failure to read is
    /// [`Unavailable`](StoreErrorKind::Unavailable) or [`Undecryptable`](StoreErrorKind::Undecryptable).
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError>;

    /// Creates `name` holding `secret`: all or nothing, and it never replaces
    /// ([`AlreadyExists`](StoreErrorKind::AlreadyExists) when the name is present).
    fn put_new(&self, name: &SecretName, secret: &[u8]) -> Result<(), StoreError>;
}

type GetHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct MemoryInner {
    secrets: HashMap<String, Zeroizing<Vec<u8>>>,
    /// Outcomes `get` returns before it answers from `secrets`, in order: bytes, or an error
    /// kind (`Absent` answers that the name holds nothing).
    scripted_gets: VecDeque<Result<Zeroizing<Vec<u8>>, StoreErrorKind>>,
    put_fault: Option<StoreErrorKind>,
    /// Whether a faulting `put_new` has stored the secret before it fails.
    put_fault_after_write: bool,
    read_only: bool,
    absent_hook: Option<GetHook>,
}

/// A store in memory, for tests, with fault injection. The agent never selects it itself.
#[derive(Default)]
pub struct MemoryStore(Mutex<MemoryInner>);

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that answers `get` but cannot create (`put_new` gives `NotCreatable`), like the
    /// device-mode credential store the administrator provisions.
    pub fn read_only() -> Self {
        let store = Self::new();
        store.inner().read_only = true;
        store
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Puts `bytes` under `name` as they are, whatever they hold, bypassing every fault.
    pub fn insert_raw(&self, name: &SecretName, bytes: &[u8]) {
        self.inner()
            .secrets
            .insert(name.0.clone(), Zeroizing::new(bytes.to_vec()));
    }

    /// The bytes under `name`, bypassing every fault.
    pub fn peek(&self, name: &SecretName) -> Option<Zeroizing<Vec<u8>>> {
        self.inner().secrets.get(&name.0).cloned()
    }

    /// How many names the store holds.
    pub fn len(&self) -> usize {
        self.inner().secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Queues the outcome of one `get`, used before any answer from the stored secrets.
    pub fn queue_get(&self, outcome: Result<Vec<u8>, StoreErrorKind>) {
        self.inner()
            .scripted_gets
            .push_back(outcome.map(Zeroizing::new));
    }

    /// Makes every `put_new` fail with `kind` (nothing is stored), or stop failing.
    pub fn set_put_fault(&self, kind: Option<StoreErrorKind>) {
        let mut inner = self.inner();
        inner.put_fault = kind;
        inner.put_fault_after_write = false;
    }

    /// Makes every `put_new` store the secret and then fail with `kind`, like a file store whose
    /// last sync fails after the file is in place.
    pub fn set_put_fault_after_write(&self, kind: StoreErrorKind) {
        let mut inner = self.inner();
        inner.put_fault = Some(kind);
        inner.put_fault_after_write = true;
    }

    /// Calls `hook` after every `get` that answers `Absent`, before it returns, so a test can
    /// hold racing creators at the moment they all see the name absent.
    pub fn set_absent_hook(&self, hook: impl Fn() + Send + Sync + 'static) {
        self.inner().absent_hook = Some(Arc::new(hook));
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, name: &SecretName) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let (outcome, hook) = {
            let mut inner = self.inner();
            let outcome = match inner.scripted_gets.pop_front() {
                Some(scripted) => scripted,
                None => inner
                    .secrets
                    .get(&name.0)
                    .cloned()
                    .ok_or(StoreErrorKind::Absent),
            };
            (outcome, inner.absent_hook.clone())
        };
        match outcome {
            Ok(bytes) => Ok(bytes),
            Err(kind) => {
                if let (StoreErrorKind::Absent, Some(hook)) = (kind, hook) {
                    hook();
                }
                Err(StoreError::new(kind))
            }
        }
    }

    fn put_new(&self, name: &SecretName, secret: &[u8]) -> Result<(), StoreError> {
        let mut inner = self.inner();
        if inner.read_only {
            return Err(StoreError::new(StoreErrorKind::NotCreatable));
        }
        if let (Some(kind), false) = (inner.put_fault, inner.put_fault_after_write) {
            return Err(StoreError::new(kind));
        }
        if inner.secrets.contains_key(&name.0) {
            return Err(StoreError::new(StoreErrorKind::AlreadyExists));
        }
        inner
            .secrets
            .insert(name.0.clone(), Zeroizing::new(secret.to_vec()));
        match inner.put_fault {
            Some(kind) => Err(StoreError::new(kind)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        assert!(SecretName::new("a").is_some());
        assert!(SecretName::new(&"a".repeat(64)).is_some());
        assert!(SecretName::new("").is_none());
        assert!(SecretName::new(&"a".repeat(65)).is_none());
        for bad in ["A", "a b", "a_b", "a/b", "é"] {
            assert!(SecretName::new(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_agent_key_name_has_the_mode_and_a_version_and_nothing_of_the_host() {
        assert_eq!(
            SecretName::agent_key(OperatingMode::Device).as_str(),
            "nekoguruma-agent-key.device.v1"
        );
        assert_eq!(
            SecretName::agent_key(OperatingMode::User).as_str(),
            "nekoguruma-agent-key.user.v1"
        );
        let name = SecretName::agent_key(OperatingMode::User);
        assert_eq!(SecretName::new(name.as_str()), Some(name));
    }

    #[test]
    fn a_memory_store_creates_once_and_never_replaces() {
        let store = MemoryStore::new();
        let name = SecretName::new("n").unwrap();
        assert_eq!(store.get(&name).unwrap_err().kind(), StoreErrorKind::Absent);
        store.put_new(&name, b"one").unwrap();
        assert_eq!(
            store.put_new(&name, b"two").unwrap_err().kind(),
            StoreErrorKind::AlreadyExists
        );
        assert_eq!(&**store.get(&name).unwrap(), b"one");
    }

    #[test]
    fn a_read_only_store_cannot_create() {
        let store = MemoryStore::read_only();
        let name = SecretName::new("n").unwrap();
        assert_eq!(
            store.put_new(&name, b"x").unwrap_err().kind(),
            StoreErrorKind::NotCreatable
        );
        assert!(store.is_empty());
    }

    #[test]
    fn an_error_names_its_kind_and_a_source_when_given() {
        let plain = StoreError::new(StoreErrorKind::Unavailable);
        assert_eq!(plain.to_string(), "the secret store is unavailable");
        let with = StoreError::with_source(StoreErrorKind::Unavailable, "no daemon");
        assert!(with.to_string().ends_with("(no daemon)"));
        assert!(std::error::Error::source(&with).is_some());
    }
}

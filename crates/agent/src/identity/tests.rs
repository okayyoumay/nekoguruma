//! The create-once rule's cases (ADR-460): S for the store's states, L for the lock, E for the
//! build's selection and the proof. Names follow the case table of the change that wrote them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use shared_crypto::proof::{ProofTranscript, sign_proof, verify_proof};

use super::store::MemoryStore;
use super::*;

const WAIT: Duration = Duration::from_millis(200);

fn name() -> SecretName {
    SecretName::agent_key(OperatingMode::User)
}

fn dir_of(dir: &Path) -> impl Fn() -> Result<PathBuf, StateDirError> + '_ {
    move || Ok(dir.to_owned())
}

/// A lock directory that must not be asked for: the key is present or the store is unusable.
fn no_lock_dir() -> Result<PathBuf, StateDirError> {
    panic!("the lock directory is not needed");
}

/// A transcript with a challenge and a nonce drawn at run time.
fn transcript() -> ProofTranscript {
    let (challenge, nonce) = (
        shared_crypto::random_32().unwrap(),
        shared_crypto::random_32().unwrap(),
    );
    let audience = shared_crypto::proof::Audience::canonicalise("https://ngr.example").unwrap();
    ProofTranscript::new(&audience, "agent-1", challenge, nonce).unwrap()
}

fn key_bytes() -> (SigningKey, Vec<u8>) {
    let key = SigningKey::generate().unwrap();
    let der = key.to_pkcs8_der().to_vec();
    (key, der)
}

/// An Ed25519 key as PKCS#8 (a valid document of another curve), built by hand: the version, the
/// algorithm identifier with the Ed25519 OID, and an octet string holding a 32-byte seed.
fn ed25519_pkcs8() -> Vec<u8> {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[7u8; 32]);
    der
}

type LockDir = Box<dyn Fn() -> Result<PathBuf, StateDirError>>;

fn kind(error: &IdentityError) -> &'static str {
    match error {
        IdentityError::NoStore(_) => "NoStore",
        IdentityError::Unavailable(_) => "Unavailable",
        IdentityError::CreationLockBusy => "CreationLockBusy",
        IdentityError::LocalFault(_) => "LocalFault",
        IdentityError::KeyUnusable(_) => "KeyUnusable",
        IdentityError::NotProvisioned => "NotProvisioned",
        IdentityError::Random(_) => "Random",
    }
}

fn load(store: &MemoryStore, dir: &Path) -> Result<AgentKey, IdentityError> {
    load_or_create(store, &name(), &dir_of(dir), WAIT)
}

/// S1, S2, L6, L11: a first run creates, a rerun finds the same key without the lock, a stale
/// lock file does not block, and the lock is free once the call returns.
#[test]
fn a_first_run_creates_and_a_rerun_finds_the_same_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    // A lock file left behind, unlocked, is normal after any create.
    std::fs::write(temp.path().join(format!("{}.lock", name())), b"").unwrap();
    let first = load(&store, temp.path()).unwrap();
    assert!(first.created());
    assert_eq!(store.len(), 1);
    assert!(WriterLock::at(&temp.path().join(format!("{}.lock", name()))).is_ok());

    let second = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap();
    assert!(!second.created());
    assert_eq!(second.public_key(), first.public_key());
    assert_eq!(store.len(), 1);
}

/// E4: a proof made with the loaded key verifies against its public key, and not against another.
#[test]
fn a_proof_made_with_the_loaded_key_verifies_against_its_public_key() {
    let temp = tempfile::tempdir().unwrap();
    let key = load(&MemoryStore::new(), temp.path()).unwrap();
    let transcript = transcript();
    let signature = sign_proof(&*key.signer(), &transcript).unwrap();
    verify_proof(&key.public_key(), &transcript, &signature).unwrap();
    let other = SigningKey::generate().unwrap();
    assert!(verify_proof(&other.public_key(), &transcript, &signature).is_err());
    assert!(format!("{key:?}").contains("redacted"));
}

/// S3: a store that cannot be read is never read as absent: nothing is created, the lock
/// directory is not asked for, and the error is retryable.
#[test]
fn an_unavailable_store_is_not_absent() {
    let store = MemoryStore::new();
    store.queue_get(Err(StoreErrorKind::Unavailable));
    let error = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap_err();
    assert_eq!(kind(&error), "Unavailable");
    assert!(error.retryable());
    assert!(store.is_empty());
}

/// S4: a key the store cannot decrypt is unusable, asks for registration again, and is untouched.
#[test]
fn an_undecryptable_key_is_unusable_and_not_replaced() {
    let store = MemoryStore::new();
    store.insert_raw(&name(), b"sealed");
    store.queue_get(Err(StoreErrorKind::Undecryptable));
    let error = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap_err();
    assert_eq!(kind(&error), "KeyUnusable");
    assert!(!error.retryable());
    assert_eq!(store.peek(&name()).unwrap().as_slice(), b"sealed");
}

/// S5: a present value that is not a P-256 PKCS#8 key is unusable and byte-identical afterwards.
#[test]
fn a_present_value_that_is_not_a_p256_key_is_unusable_and_not_replaced() {
    let (_, good) = key_bytes();
    let mut truncated = good.clone();
    truncated.truncate(good.len() - 10);
    let mut tail_flipped = good.clone();
    *tail_flipped.last_mut().unwrap() ^= 0xFF;
    let cases: [(&str, Vec<u8>); 5] = [
        ("empty", Vec::new()),
        ("truncated", truncated),
        ("other curve", ed25519_pkcs8()),
        ("garbage", b"not a key at all".to_vec()),
        ("public part changed", tail_flipped),
    ];
    for (label, bytes) in cases {
        let store = MemoryStore::new();
        store.insert_raw(&name(), &bytes);
        let error = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap_err();
        assert_eq!(kind(&error), "KeyUnusable", "{label}");
        assert_eq!(store.peek(&name()).unwrap().as_slice(), bytes, "{label}");
    }
}

/// S9: an absent key in a store that cannot create is "not provisioned", nothing is written, and
/// the lock is released.
#[test]
fn an_absent_key_in_a_read_only_store_is_not_provisioned() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::read_only();
    let error = load(&store, temp.path()).unwrap_err();
    assert_eq!(kind(&error), "NotProvisioned");
    assert!(!error.retryable());
    assert!(store.is_empty());
    assert!(WriterLock::at(&temp.path().join(format!("{}.lock", name()))).is_ok());
}

/// S10: the re-check under the lock finds a key another process just created.
#[test]
fn a_key_that_appears_while_waiting_is_adopted() {
    let temp = tempfile::tempdir().unwrap();
    let (key, der) = key_bytes();
    let store = MemoryStore::new();
    store.insert_raw(&name(), &der);
    store.queue_get(Err(StoreErrorKind::Absent)); // the first read, before the key "appeared"
    let loaded = load(&store, temp.path()).unwrap();
    assert!(!loaded.created());
    assert_eq!(loaded.public_key(), key.public_key());
    assert_eq!(store.peek(&name()).unwrap().as_slice(), der);
}

/// S11: the re-check finds the store unavailable or the key unusable: no create.
#[test]
fn a_failing_re_check_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    for (second, expected) in [
        (Err(StoreErrorKind::Unavailable), "Unavailable"),
        (Err(StoreErrorKind::Undecryptable), "KeyUnusable"),
        (Ok(b"junk".to_vec()), "KeyUnusable"),
    ] {
        let store = MemoryStore::new();
        store.queue_get(Err(StoreErrorKind::Absent));
        store.queue_get(second);
        let error = load(&store, temp.path()).unwrap_err();
        assert_eq!(kind(&error), expected);
        assert!(store.is_empty());
    }
}

/// S12: a create that fails midway is unavailable, nothing is stored and nothing is handed out.
#[test]
fn a_failed_create_is_unavailable_and_stores_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    store.set_put_fault(Some(StoreErrorKind::Unavailable));
    let error = load(&store, temp.path()).unwrap_err();
    assert_eq!(kind(&error), "Unavailable");
    assert!(error.retryable());
    assert!(store.is_empty());
    // The next try, with the fault gone, creates.
    store.set_put_fault(None);
    assert!(load(&store, temp.path()).unwrap().created());
}

/// A create that failed after the key was in place (a sync failed) leaves a key: it is adopted,
/// and this call does not claim to have created it.
#[test]
fn a_create_that_fails_after_the_key_is_in_place_adopts_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    store.set_put_fault_after_write(StoreErrorKind::Unavailable);
    let loaded = load(&store, temp.path()).unwrap();
    assert!(!loaded.created());
    assert_eq!(store.len(), 1);
    assert_eq!(
        store.peek(&name()).unwrap().as_slice(),
        &*SigningKey::from_pkcs8_der(&store.peek(&name()).unwrap())
            .unwrap()
            .to_pkcs8_der()
    );
}

/// A wait too long to add to the clock does not panic.
#[test]
fn a_wait_beyond_the_clock_is_no_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let key = load_or_create(
        &MemoryStore::new(),
        &name(),
        &dir_of(temp.path()),
        Duration::MAX,
    )
    .unwrap();
    assert!(key.created());
}

/// E4 across processes: a second store instance on the same directory (as a later process has)
/// loads the key the first created, and its proof verifies against the PEM the first printed.
#[cfg(debug_assertions)]
#[test]
fn a_proof_from_a_later_load_verifies_against_the_first_loads_pem() {
    use file_store::DebugFileStore;
    let temp = tempfile::tempdir().unwrap();
    let keys = temp.path().to_owned();
    let first = load_or_create(
        &DebugFileStore::new(keys.clone()),
        &name(),
        &dir_of(&keys),
        WAIT,
    )
    .unwrap();
    let pem = first.public_key().to_spki_pem();
    drop(first);
    let later = load_or_create(
        &DebugFileStore::new(keys.clone()),
        &name(),
        &no_lock_dir,
        WAIT,
    )
    .unwrap();
    assert!(!later.created());
    let transcript = transcript();
    let signature = sign_proof(&*later.signer(), &transcript).unwrap();
    let printed = shared_crypto::PublicKey::from_spki_pem(&pem).unwrap();
    verify_proof(&printed, &transcript, &signature).unwrap();
}

/// S13: a writer outside the lock created the key first: adopt it.
#[test]
fn a_create_that_finds_the_name_present_adopts_that_key() {
    let temp = tempfile::tempdir().unwrap();
    let (key, der) = key_bytes();
    let store = MemoryStore::new();
    store.insert_raw(&name(), &der);
    store.queue_get(Err(StoreErrorKind::Absent));
    store.queue_get(Err(StoreErrorKind::Absent));
    store.set_put_fault(Some(StoreErrorKind::AlreadyExists));
    let loaded = load(&store, temp.path()).unwrap();
    assert!(!loaded.created());
    assert_eq!(loaded.public_key(), key.public_key());
}

/// S14: a read-back that fails or finds nothing hands out nothing.
#[test]
fn a_failed_read_back_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    for readback in [StoreErrorKind::Unavailable, StoreErrorKind::Absent] {
        let store = MemoryStore::new();
        store.queue_get(Err(StoreErrorKind::Absent));
        store.queue_get(Err(StoreErrorKind::Absent));
        store.queue_get(Err(readback));
        let error = load(&store, temp.path()).unwrap_err();
        assert_eq!(kind(&error), "Unavailable", "{readback:?}");
    }
}

/// S15: a read-back that decodes to another key is the key that is used.
#[test]
fn a_read_back_of_another_key_is_the_key_used() {
    let temp = tempfile::tempdir().unwrap();
    let (other, other_der) = key_bytes();
    let store = MemoryStore::new();
    store.queue_get(Err(StoreErrorKind::Absent));
    store.queue_get(Err(StoreErrorKind::Absent));
    store.queue_get(Ok(other_der));
    let loaded = load(&store, temp.path()).unwrap();
    assert!(!loaded.created());
    assert_eq!(loaded.public_key(), other.public_key());
}

/// L1: two creators held at Absent by the store's hook both go on to the lock: one creates, the
/// other adopts its key, and the store holds one key. No timing decides it.
#[test]
fn two_creators_held_at_absent_create_one_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let barrier = Arc::new(Barrier::new(2));
    let seen = Arc::new(AtomicUsize::new(0));
    {
        let (barrier, seen) = (barrier.clone(), seen.clone());
        store.set_absent_hook(move || {
            // Only the two first reads wait: the winner's re-check under the lock must not.
            if seen.fetch_add(1, Ordering::SeqCst) < 2 {
                barrier.wait();
            }
        });
    }
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            let dir = temp.path().to_owned();
            std::thread::spawn(move || {
                load_or_create(&*store, &name(), &dir_of(&dir), Duration::from_secs(5)).unwrap()
            })
        })
        .collect();
    let keys: Vec<AgentKey> = racers
        .into_iter()
        .map(|racer| racer.join().unwrap())
        .collect();
    assert_eq!(keys[0].public_key(), keys[1].public_key());
    assert_eq!(keys.iter().filter(|key| key.created()).count(), 1);
    assert_eq!(store.len(), 1);
}

/// L5: a holder stuck past the wait makes the waiter report the lock busy (retryable), and nothing
/// is written.
#[test]
fn a_creator_that_stays_locked_out_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let _holder = WriterLock::at(&temp.path().join(format!("{}.lock", name()))).unwrap();
    let store = MemoryStore::new();
    let started = Instant::now();
    let error = load_or_create(
        &store,
        &name(),
        &dir_of(temp.path()),
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert_eq!(kind(&error), "CreationLockBusy");
    assert!(error.retryable());
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(store.is_empty());
}

/// L7: a lock that cannot be opened, with the key absent, is a local fault (no retry helps) and
/// creates nothing. The
/// parent being a regular file fails for every account, root included.
#[test]
fn a_lock_that_cannot_be_opened_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"x").unwrap();
    let lock_as_dir = temp.path().join("as-dir");
    std::fs::create_dir_all(as_lock_path(&lock_as_dir)).unwrap();
    let cases: [(&str, LockDir); 3] = [
        ("parent is a file", Box::new(move || Ok(file.join("sub")))),
        ("directory is missing", {
            let missing = temp.path().join("missing");
            Box::new(move || Ok(missing.clone()))
        }),
        (
            "lock path is a directory",
            Box::new(move || Ok(lock_as_dir.clone())),
        ),
    ];
    for (label, lock_dir) in cases {
        let store = MemoryStore::new();
        let error = load_or_create(&store, &name(), &*lock_dir, WAIT).unwrap_err();
        assert_eq!(kind(&error), "LocalFault", "{label}");
        assert!(!error.retryable(), "{label}");
        assert!(store.is_empty(), "{label}");
    }
    let store = MemoryStore::new();
    let failing = || {
        Err::<PathBuf, _>(
            crate::state::prepare_subdir(Path::new("/nonexistent-root"), false, "identity")
                .unwrap_err(),
        )
    };
    let error = load_or_create(&store, &name(), &failing, WAIT).unwrap_err();
    assert_eq!(kind(&error), "LocalFault");
    assert!(!error.retryable());
}

/// The directory that holds the lock file at `dir/<name>.lock` as a directory.
fn as_lock_path(dir: &Path) -> PathBuf {
    dir.join(format!("{}.lock", name()))
}

/// L8 (with S2): the key present and the lock unreachable is still the key.
#[test]
fn a_present_key_needs_no_lock() {
    let (key, der) = key_bytes();
    let store = MemoryStore::new();
    store.insert_raw(&name(), &der);
    let loaded = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap();
    assert_eq!(loaded.public_key(), key.public_key());
}

/// L10: the two modes have different names, so their keys are independent.
#[test]
fn the_modes_keep_separate_keys() {
    let temp = tempfile::tempdir().unwrap();
    let store = MemoryStore::new();
    let create = |mode| {
        load_or_create(
            &store,
            &SecretName::agent_key(mode),
            &dir_of(temp.path()),
            WAIT,
        )
        .unwrap()
    };
    let device = create(OperatingMode::Device);
    let user = create(OperatingMode::User);
    assert!(device.created() && user.created());
    assert_ne!(device.public_key(), user.public_key());
    assert_eq!(store.len(), 2);
}

/// Only an unavailable store and a busy creation lock are worth a retry.
#[test]
fn only_unavailable_and_a_busy_lock_are_retryable() {
    assert!(IdentityError::Unavailable("x".into()).retryable());
    assert!(IdentityError::CreationLockBusy.retryable());
    for error in [
        IdentityError::LocalFault("x".into()),
        IdentityError::NoStore("x"),
        IdentityError::KeyUnusable("x"),
        IdentityError::NotProvisioned,
        IdentityError::Random(RandomError),
    ] {
        assert!(!error.retryable(), "{error}");
    }
}

fn vars(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<OsString> {
    move |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
    }
}

fn abs_dir() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

/// S8: without the debug variables no store is selected, in either build.
#[test]
fn no_variable_selects_no_store() {
    for mode in [OperatingMode::Device, OperatingMode::User] {
        let error = select_store(mode, &vars(vec![])).err().unwrap();
        assert_eq!(kind(&error), "NoStore");
        let only_dir = vars(vec![(crate::state::STATE_DIR_VAR, abs_dir())]);
        assert_eq!(
            kind(&select_store(mode, &only_dir).err().unwrap()),
            "NoStore"
        );
    }
}

/// E3: the file store needs both variables, an absolute directory and the value `file`.
#[cfg(debug_assertions)]
#[test]
fn the_file_store_needs_both_variables() {
    let bad: Vec<Vec<(&'static str, String)>> = vec![
        vec![(KEY_STORE_VAR, "file".into())],
        vec![
            (KEY_STORE_VAR, "file".into()),
            (crate::state::STATE_DIR_VAR, "relative".into()),
        ],
        vec![
            (KEY_STORE_VAR, "plain".into()),
            (crate::state::STATE_DIR_VAR, abs_dir()),
        ],
        vec![
            (KEY_STORE_VAR, "".into()),
            (crate::state::STATE_DIR_VAR, abs_dir()),
        ],
    ];
    for pairs in bad {
        let error = select_store(OperatingMode::User, &vars(pairs.clone()))
            .err()
            .unwrap();
        assert_eq!(kind(&error), "NoStore", "{pairs:?}");
    }
    let good = vars(vec![
        (KEY_STORE_VAR, "file".into()),
        (crate::state::STATE_DIR_VAR, abs_dir()),
    ]);
    assert!(select_store(OperatingMode::User, &good).is_ok());
}

/// E2: a release build selects no store whatever the variables say.
#[cfg(not(debug_assertions))]
#[test]
fn a_release_build_ignores_both_variables() {
    let both = vars(vec![
        (KEY_STORE_VAR, "file".into()),
        (crate::state::STATE_DIR_VAR, abs_dir()),
    ]);
    for mode in [OperatingMode::Device, OperatingMode::User] {
        assert_eq!(kind(&select_store(mode, &both).err().unwrap()), "NoStore");
    }
}

/// S6 through the file store: a crash left a temporary file; the create succeeds and removes it.
#[cfg(debug_assertions)]
#[test]
fn the_file_store_creates_once_across_calls_and_cleans_a_stale_temp() {
    use file_store::DebugFileStore;
    let temp = tempfile::tempdir().unwrap();
    let keys = temp.path().join("identity");
    std::fs::create_dir(&keys).unwrap();
    std::fs::write(keys.join(format!("{}.pk8.tmp", name())), b"half").unwrap();
    let store = DebugFileStore::new(keys.clone());
    let first = load_or_create(&store, &name(), &dir_of(&keys), WAIT).unwrap();
    assert!(first.created());
    assert!(!keys.join(format!("{}.pk8.tmp", name())).exists());
    let again = load_or_create(&store, &name(), &no_lock_dir, WAIT).unwrap();
    assert!(!again.created());
    assert_eq!(again.public_key(), first.public_key());
}

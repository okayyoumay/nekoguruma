//! The blob store's policy against a fake protector and a fake profile probe, over real files in
//! a temporary directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use worker_host::service::OperatingMode;

use super::*;
use crate::identity::{IdentityError, load_or_create};

/// A protector that XORs the secret and appends the entropy and a sum of everything before it:
/// a blob opens only for the entropy it was made with and only while unmodified, as DPAPI's.
#[derive(Default)]
struct FakeProtect {
    protect_fault: Option<u32>,
    unprotect_fault: Option<u32>,
    protects: AtomicUsize,
}

fn sum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |total, byte| total.wrapping_add(*byte))
}

impl Protect for FakeProtect {
    fn protect(&self, plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, OsCode> {
        self.protects.fetch_add(1, Ordering::SeqCst);
        if let Some(code) = self.protect_fault {
            return Err(OsCode(code));
        }
        let mut blob: Vec<u8> = plain.iter().map(|byte| byte ^ 0x5a).collect();
        blob.extend_from_slice(entropy);
        blob.push(sum(&blob));
        Ok(blob)
    }

    fn unprotect(&self, blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, OsCode> {
        if let Some(code) = self.unprotect_fault {
            return Err(OsCode(code));
        }
        let invalid = OsCode(ERROR_INVALID_DATA);
        let (body, check) = blob.split_at(blob.len().checked_sub(1).ok_or(invalid)?);
        if sum(body) != check[0] {
            return Err(invalid);
        }
        let plain_len = body.len().checked_sub(entropy.len()).ok_or(invalid)?;
        let (xored, bound) = body.split_at(plain_len);
        if bound != entropy {
            return Err(invalid);
        }
        Ok(Zeroizing::new(
            xored.iter().map(|byte| byte ^ 0x5a).collect(),
        ))
    }
}

struct FakeProfile(Result<u32, u32>);

impl ProfileProbe for FakeProfile {
    fn profile_flags(&self) -> Result<u32, OsCode> {
        self.0.map_err(OsCode)
    }
}

const LOCAL: u32 = 0;
const ROAMING: u32 = 0x2;

type Store = BlobStore<FakeProtect, FakeProfile>;

fn name() -> SecretName {
    SecretName::agent_key(OperatingMode::User)
}

fn other_name() -> SecretName {
    SecretName::agent_key(OperatingMode::Device)
}

fn store_with(root: &Path, protect: FakeProtect, profile: Result<u32, u32>) -> Store {
    BlobStore::new(root.to_owned(), protect, FakeProfile(profile))
}

/// A store whose root does not exist yet, in a temporary directory that does.
fn fresh() -> (tempfile::TempDir, PathBuf, Store) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = store_with(&root, FakeProtect::default(), Ok(LOCAL));
    (temp, root, store)
}

/// The identity directory, made as `state::private_subdir` makes it before a create.
fn identity_dir(root: &Path) -> PathBuf {
    let dir = root.join("identity");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn kind<T: std::fmt::Debug>(result: Result<T, StoreError>) -> StoreErrorKind {
    result.unwrap_err().kind()
}

fn file_of(dir: &Path, name: &SecretName) -> PathBuf {
    dir.join(format!("{name}.dpapi"))
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A first run finds nothing, a create is found again by a store made afterwards, and a second
/// create finds the name present and changes nothing. The file starts with the magic and the
/// format, holds no plain bytes and leaves no temporary file.
#[test]
fn a_secret_is_created_once_and_read_back() {
    let (_temp, root, store) = fresh();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Absent);
    let dir = identity_dir(&root);
    let secret = b"a secret that is long enough to find".to_vec();
    store.put_new(&name(), &secret).unwrap();
    let again = store_with(&root, FakeProtect::default(), Ok(LOCAL));
    assert_eq!(&**again.get(&name()).unwrap(), &secret[..]);
    let file = file_of(&dir, &name());
    let before = fs::read(&file).unwrap();
    assert_eq!(&before[..9], b"NGRDPAPI\x01");
    assert!(!before.windows(secret.len()).any(|window| window == secret));
    assert_eq!(
        kind(again.put_new(&name(), b"other")),
        StoreErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&file).unwrap(), before);
    assert_eq!(names_in(&dir), [format!("{}.dpapi", name())]);
}

/// `load_or_create` over the store: created once, then found as the same key.
#[test]
fn load_or_create_creates_once_and_then_finds_the_same_key() {
    let (_temp, root, store) = fresh();
    let lock = || crate::state::prepare_subdir(&root, true, "identity");
    let wait = std::time::Duration::from_secs(5);
    let first = load_or_create(&store, &name(), &lock, wait).unwrap();
    assert!(first.created());
    let second = load_or_create(&store, &name(), &lock, wait).unwrap();
    assert!(!second.created());
    assert_eq!(first.public_key(), second.public_key());
}

/// Files that are no key file are undecryptable and are left as they are: a plain PKCS#8
/// document, an empty file, a file over the cap, a wrong magic, an unknown format byte, a header
/// with no blob and a file too short for the header.
#[test]
fn a_file_that_is_no_key_file_is_undecryptable_and_unchanged() {
    let (_temp, root, store) = fresh();
    let dir = identity_dir(&root);
    let file = file_of(&dir, &name());
    let der = shared_crypto::SigningKey::generate()
        .unwrap()
        .to_pkcs8_der();
    let mut wrong_magic = b"NGRDPAPX\x01".to_vec();
    wrong_magic.extend_from_slice(b"blob");
    let mut unknown_format = b"NGRDPAPI\x02".to_vec();
    unknown_format.extend_from_slice(b"blob");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("plain PKCS#8", der.to_vec()),
        ("empty", Vec::new()),
        ("oversized", vec![b'N'; MAX_LEN + 1]),
        ("wrong magic", wrong_magic),
        ("unknown format", unknown_format),
        ("header only", b"NGRDPAPI\x01".to_vec()),
        ("too short", b"NGRD".to_vec()),
    ];
    for (label, bytes) in cases {
        fs::write(&file, &bytes).unwrap();
        assert_eq!(
            kind(store.get(&name())),
            StoreErrorKind::Undecryptable,
            "{label}"
        );
        assert_eq!(fs::read(&file).unwrap(), bytes, "{label}");
    }
}

/// A file of exactly the cap is read as far as the protector, which decides; one byte more is
/// undecryptable without asking it.
#[test]
fn the_cap_is_inclusive() {
    let (_temp, root, _store) = fresh();
    let dir = identity_dir(&root);
    let file = file_of(&dir, &name());
    let protect = FakeProtect {
        unprotect_fault: Some(0x1234),
        ..FakeProtect::default()
    };
    let store = store_with(&root, protect, Ok(LOCAL));
    let mut bytes = b"NGRDPAPI\x01".to_vec();
    bytes.resize(MAX_LEN, 0);
    fs::write(&file, &bytes).unwrap();
    // The protector's own refusal (an unlisted code) shows the file got that far.
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Unavailable);
    bytes.push(0);
    fs::write(&file, &bytes).unwrap();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Undecryptable);
}

/// A blob with a changed byte, and a blob copied to another name, do not open, and the file is
/// not touched.
#[test]
fn a_tampered_blob_and_a_copied_blob_are_undecryptable() {
    let (_temp, root, store) = fresh();
    let dir = identity_dir(&root);
    store.put_new(&name(), b"secret bytes").unwrap();
    let file = file_of(&dir, &name());
    let original = fs::read(&file).unwrap();

    let copy = file_of(&dir, &other_name());
    fs::write(&copy, &original).unwrap();
    assert_eq!(
        kind(store.get(&other_name())),
        StoreErrorKind::Undecryptable
    );
    assert_eq!(fs::read(&copy).unwrap(), original);

    let mut tampered = original;
    tampered[HEADER_LEN] ^= 1;
    fs::write(&file, &tampered).unwrap();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Undecryptable);
    assert_eq!(fs::read(&file).unwrap(), tampered);
}

/// The operating system's refusal is undecryptable only for the listed codes; any other is
/// unavailable, so an unknown failure is retried and never reported as a lost key.
#[test]
fn only_the_listed_codes_make_a_blob_undecryptable() {
    let (_temp, root, store) = fresh();
    identity_dir(&root);
    store.put_new(&name(), b"secret").unwrap();
    for code in UNDECRYPTABLE_CODES {
        let protect = FakeProtect {
            unprotect_fault: Some(code),
            ..FakeProtect::default()
        };
        let store = store_with(&root, protect, Ok(LOCAL));
        assert_eq!(
            kind(store.get(&name())),
            StoreErrorKind::Undecryptable,
            "{code:#x}"
        );
    }
    for code in [5, 1168, 0x8009_0005] {
        let protect = FakeProtect {
            unprotect_fault: Some(code),
            ..FakeProtect::default()
        };
        let store = store_with(&root, protect, Ok(LOCAL));
        let error = store.get(&name()).unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::Unavailable, "{code:#x}");
        assert!(
            error.to_string().contains(&format!("{code:#010x}")),
            "{error}"
        );
    }
}

/// A crash left the temporary file and no final name: absent, and the next create removes it.
#[test]
fn a_stale_temporary_file_is_absent_and_the_next_create_removes_it() {
    let (_temp, root, store) = fresh();
    let dir = identity_dir(&root);
    let tmp = dir.join(format!("{}.dpapi.tmp", name()));
    fs::write(&tmp, b"half a file").unwrap();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Absent);
    store.put_new(&name(), b"whole").unwrap();
    assert_eq!(&**store.get(&name()).unwrap(), b"whole");
    assert_eq!(names_in(&dir), [format!("{}.dpapi", name())]);
}

/// With the state's parent directory missing (an unmounted drive or profile), nothing is absent
/// and nothing is created.
#[test]
fn a_missing_parent_of_the_root_is_unavailable_and_creates_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-mounted").join("state");
    let store = store_with(&root, FakeProtect::default(), Ok(LOCAL));
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Unavailable);
    assert_eq!(
        kind(store.put_new(&name(), b"x")),
        StoreErrorKind::Unavailable
    );
    assert!(names_in(temp.path()).is_empty());
}

/// With the root there and its `identity` directory not, a create fails and makes nothing.
#[test]
fn a_create_without_the_identity_directory_is_unavailable_and_creates_nothing() {
    let (_temp, root, store) = fresh();
    fs::create_dir(&root).unwrap();
    assert_eq!(
        kind(store.put_new(&name(), b"x")),
        StoreErrorKind::Unavailable
    );
    assert!(names_in(&root).is_empty());
}

/// A profile that is temporary or mandatory, or whose type cannot be read, makes a missing file
/// unavailable and refuses a create before the secret is protected; a local or roaming profile is
/// the normal case.
#[test]
fn the_profile_gates_absent_and_creation() {
    for (label, profile) in [
        ("temporary", Ok(PT_TEMPORARY)),
        ("mandatory", Ok(PT_MANDATORY)),
        ("roaming and mandatory", Ok(ROAMING | PT_MANDATORY)),
        ("unreadable", Err(1008)),
    ] {
        let (_temp, root, _store) = fresh();
        let dir = identity_dir(&root);
        let store = store_with(&root, FakeProtect::default(), profile);
        assert_eq!(
            kind(store.get(&name())),
            StoreErrorKind::Unavailable,
            "{label}"
        );
        assert_eq!(
            kind(store.put_new(&name(), b"x")),
            StoreErrorKind::Unavailable,
            "{label}"
        );
        assert_eq!(store.protect.protects.load(Ordering::SeqCst), 0, "{label}");
        assert!(names_in(&dir).is_empty(), "{label}");
    }
    for profile in [LOCAL, ROAMING] {
        let (_temp, root, _store) = fresh();
        identity_dir(&root);
        let store = store_with(&root, FakeProtect::default(), Ok(profile));
        assert_eq!(kind(store.get(&name())), StoreErrorKind::Absent);
        store.put_new(&name(), b"x").unwrap();
    }
}

/// A key that is present is read whatever the profile says: the profile gates only the absent
/// answer and the creation.
#[test]
fn a_present_key_is_read_without_the_profile_check() {
    let (_temp, root, store) = fresh();
    identity_dir(&root);
    store.put_new(&name(), b"kept").unwrap();
    for profile in [Ok(PT_TEMPORARY), Ok(PT_MANDATORY), Err(5)] {
        let store = store_with(&root, FakeProtect::default(), profile);
        assert_eq!(&**store.get(&name()).unwrap(), b"kept");
        assert_eq!(
            kind(store.put_new(&name(), b"x")),
            StoreErrorKind::Unavailable
        );
    }
}

/// A failure of the protector on create is unavailable and leaves no file.
#[test]
fn a_protect_failure_is_unavailable_and_creates_nothing() {
    let (_temp, root, _store) = fresh();
    let dir = identity_dir(&root);
    let protect = FakeProtect {
        protect_fault: Some(5),
        ..FakeProtect::default()
    };
    let store = store_with(&root, protect, Ok(LOCAL));
    let error = store.put_new(&name(), b"x").unwrap_err();
    assert_eq!(error.kind(), StoreErrorKind::Unavailable);
    assert!(error.to_string().contains("0x00000005"), "{error}");
    assert!(names_in(&dir).is_empty());
}

/// A directory at the key's name cannot be read as a file.
#[test]
fn a_directory_at_the_name_is_unavailable() {
    let (_temp, root, store) = fresh();
    let dir = identity_dir(&root);
    fs::create_dir(file_of(&dir, &name())).unwrap();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Unavailable);
}

/// A link at the name is not followed.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_name_is_unavailable() {
    let (temp, root, store) = fresh();
    let dir = identity_dir(&root);
    let target = temp.path().join("target");
    fs::write(&target, b"NGRDPAPI\x01whatever").unwrap();
    std::os::unix::fs::symlink(&target, file_of(&dir, &name())).unwrap();
    assert_eq!(kind(store.get(&name())), StoreErrorKind::Unavailable);
}

/// Creators that do not take the lock: at most one create succeeds, each of the others finds
/// the name present or is unavailable (its temporary file was taken from under it), and what is
/// in place afterwards opens. `load_or_create` takes the lock, and adopts what is stored.
#[test]
fn concurrent_creates_leave_one_blob_that_opens() {
    const CREATORS: usize = 4;
    let (_temp, root, store) = fresh();
    identity_dir(&root);
    let barrier = std::sync::Barrier::new(CREATORS);
    let results: Vec<(usize, Result<(), StoreError>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..CREATORS)
            .map(|index| {
                let (store, barrier) = (&store, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    (
                        index,
                        store.put_new(&name(), format!("secret {index}").as_bytes()),
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let winners: Vec<usize> = results
        .iter()
        .filter(|(_, result)| result.is_ok())
        .map(|(index, _)| *index)
        .collect();
    assert!(winners.len() <= 1, "{winners:?}");
    for (_, result) in &results {
        if let Err(error) = result {
            assert!(
                matches!(
                    error.kind(),
                    StoreErrorKind::AlreadyExists | StoreErrorKind::Unavailable
                ),
                "{error}"
            );
        }
    }
    // Creators share the temporary name, so a creator may link a file another is still filling:
    // what is in place is whole and is one creator's secret, not necessarily the winner's own.
    // `load_or_create` hands out what the store holds, so it never matters there.
    let secrets: Vec<Vec<u8>> = (0..CREATORS)
        .map(|index| format!("secret {index}").into_bytes())
        .collect();
    match store.get(&name()) {
        Ok(stored) => assert!(secrets.contains(&stored), "{:?}", &**stored),
        // Every creator lost its temporary file to another: nothing is in place and a retry
        // creates it.
        Err(error) => {
            assert!(winners.is_empty(), "{winners:?}");
            assert_eq!(error.kind(), StoreErrorKind::Absent);
            store.put_new(&name(), b"retry").unwrap();
        }
    }
}

/// A key that cannot be decoded as a P-256 key, in a file the store opens, is the identity
/// code's `KeyUnusable`, not an absent key.
#[test]
fn a_blob_that_holds_no_key_is_unusable_and_not_replaced() {
    let (_temp, root, store) = fresh();
    let dir = identity_dir(&root);
    store.put_new(&name(), b"not pkcs8").unwrap();
    let before = fs::read(file_of(&dir, &name())).unwrap();
    let lock = || Ok(dir.clone());
    let error =
        load_or_create(&store, &name(), &lock, std::time::Duration::from_secs(5)).unwrap_err();
    assert!(matches!(error, IdentityError::KeyUnusable(..)), "{error}");
    assert_eq!(fs::read(file_of(&dir, &name())).unwrap(), before);
}

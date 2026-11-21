//! The cache's rows: A1, A2, A15, A17, A19 and the layout (ADR-497 item 7).

use std::fs;
use std::io::Write;

use sha2::{Digest, Sha256};

use super::*;

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn objects() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("objects");
    fs::create_dir(&root).unwrap();
    (temp, root)
}

/// Downloads `bytes` into the cache as a fetch does.
fn put(store: &ObjectStore, kind: Kind, bytes: &[u8]) -> PathBuf {
    let (tmp, mut file) = store.begin().unwrap();
    file.write_all(bytes).unwrap();
    store.commit(tmp, file, kind, &digest(bytes)).unwrap()
}

#[test]
fn the_layout_is_one_directory_per_kind_and_a_tmp_area_all_owner_only() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    for name in ["documents", "artifacts", "tmp"] {
        assert!(root.join(name).is_dir(), "{name}");
    }
    let path = put(&store, Kind::Document, b"doc");
    assert_eq!(path, root.join("documents").join(hex(&digest(b"doc"))));
    let path = put(&store, Kind::Artifact, b"art");
    assert_eq!(path, root.join("artifacts").join(hex(&digest(b"art"))));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for name in ["documents", "artifacts", "tmp"] {
            assert_eq!(mode(&root.join(name)), 0o700, "{name}");
        }
        assert_eq!(mode(&path), 0o600);
    }
}

/// A15: the file is renamed to its digest and no temporary file remains.
#[test]
fn a_committed_download_is_in_place_and_tmp_is_empty() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let path = put(&store, Kind::Document, b"hello");
    assert_eq!(fs::read(&path).unwrap(), b"hello");
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
}

/// A1: bytes that hash to the name are a hit, for `load` and for `read`.
#[test]
fn a_file_that_hashes_to_its_name_is_a_hit() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let path = put(&store, Kind::Document, b"hello");
    let d = digest(b"hello");
    assert_eq!(
        store.load(Kind::Document, &d, 100).unwrap(),
        Load::Hit { path, length: 5 }
    );
    assert_eq!(
        store.read(Kind::Document, &d, 100).unwrap().as_deref(),
        Some(&b"hello"[..])
    );
    // The other kind's directory does not have it.
    assert_eq!(store.load(Kind::Artifact, &d, 100).unwrap(), Load::Miss);
    assert_eq!(store.read(Kind::Artifact, &d, 100).unwrap(), None);
}

/// A2: a file that no longer matches, or is over the bound, is deleted and is a miss.
#[test]
fn a_file_that_changed_or_is_over_the_bound_is_deleted() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let d = digest(b"hello");
    let path = put(&store, Kind::Document, b"hello");
    fs::write(&path, b"hellO").unwrap();
    assert_eq!(store.load(Kind::Document, &d, 100).unwrap(), Load::Deleted);
    assert!(!path.exists());
    assert_eq!(store.load(Kind::Document, &d, 100).unwrap(), Load::Miss);

    let path = put(&store, Kind::Document, b"hello");
    fs::write(&path, b"hellO").unwrap();
    assert_eq!(store.read(Kind::Document, &d, 100).unwrap(), None);
    assert!(!path.exists());

    // Intact, but over the bound given.
    let path = put(&store, Kind::Document, b"hello");
    assert_eq!(store.load(Kind::Document, &d, 4).unwrap(), Load::Deleted);
    assert!(!path.exists());
    let path = put(&store, Kind::Document, b"hello");
    assert_eq!(store.read(Kind::Document, &d, 4).unwrap(), None);
    assert!(!path.exists());
}

/// A2: a name that is not a regular file is a local failure and nothing is deleted.
#[test]
fn a_name_that_is_not_a_regular_file_is_refused_and_kept() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let d = digest(b"hello");
    let path = root.join("documents").join(hex(&d));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("inner"), b"x").unwrap();
    assert!(matches!(
        store.load(Kind::Document, &d, 100),
        Err(StoreError::NotRegular)
    ));
    assert!(matches!(
        store.read(Kind::Document, &d, 100),
        Err(StoreError::NotRegular)
    ));
    assert!(path.join("inner").exists());
}

#[cfg(unix)]
#[test]
fn a_link_is_not_followed_and_is_kept() {
    let (temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let d = digest(b"hello");
    let target = temp.path().join("elsewhere");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, root.join("documents").join(hex(&d))).unwrap();
    assert!(matches!(
        store.load(Kind::Document, &d, 100),
        Err(StoreError::NotRegular)
    ));
    assert!(target.exists());
}

/// A2: a file that cannot be read is a local failure and nothing is deleted.
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_a_local_failure_and_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let d = digest(b"hello");
    let path = put(&store, Kind::Document, b"hello");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    // A superuser reads it anyway; the check is only meaningful for an ordinary account.
    if fs::File::open(&path).is_ok() {
        return;
    }
    assert!(matches!(
        store.load(Kind::Document, &d, 100),
        Err(StoreError::Io(_))
    ));
    assert!(path.exists());
}

/// A17: what a dead process left in `tmp` is swept at the next open, and nothing else is.
#[test]
fn the_tmp_directory_is_swept_at_open() {
    let (_temp, root) = objects();
    let kept = {
        let store = ObjectStore::open_in(&root).unwrap();
        let kept = put(&store, Kind::Document, b"kept");
        fs::write(root.join("tmp").join("half"), b"partial").unwrap();
        fs::create_dir(root.join("tmp").join("dir")).unwrap();
        fs::write(root.join("tmp").join("dir").join("x"), b"y").unwrap();
        kept
    };
    let _store = ObjectStore::open_in(&root).unwrap();
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
    assert!(kept.exists());
}

/// A19: one directory lock per state root; it is free again when the store is dropped.
#[test]
fn a_second_open_of_the_same_root_is_refused() {
    let (_temp, root) = objects();
    let first = ObjectStore::open_in(&root).unwrap();
    assert!(matches!(
        ObjectStore::open_in(&root),
        Err(StoreError::InUse)
    ));
    // The refused open swept nothing the first one is writing.
    let (tmp, mut file) = first.begin().unwrap();
    file.write_all(b"live").unwrap();
    assert!(matches!(
        ObjectStore::open_in(&root),
        Err(StoreError::InUse)
    ));
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 1);
    drop((tmp, file));
    drop(first);
    ObjectStore::open_in(&root).unwrap();
}

/// A dropped download leaves no file (the guard of a stop, A18).
#[test]
fn a_dropped_download_deletes_its_file() {
    let (_temp, root) = objects();
    let store = ObjectStore::open_in(&root).unwrap();
    let (tmp, file) = store.begin().unwrap();
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 1);
    drop(file);
    drop(tmp);
    assert_eq!(fs::read_dir(root.join("tmp")).unwrap().count(), 0);
}

#[test]
fn the_hashing_writer_counts_and_hashes_what_it_writes() {
    let mut writer = HashingWriter::new(Vec::new());
    writer.write_all(b"he").unwrap();
    writer.write_all(b"llo").unwrap();
    let (inner, hash, length) = writer.finish();
    assert_eq!((inner.as_slice(), length), (&b"hello"[..], 5));
    assert_eq!(hash, digest(b"hello"));
}

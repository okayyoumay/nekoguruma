//! The local store's tests (ADR-474). They need no database, so every platform's CI runs them.
//! Races run on the multi-thread runtime. A refusal by directory mode is provoked by mode bits
//! (Unix only), never by an unreadable mode, because the tests may run as root.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::hooks::Parker;
use super::*;

fn config(dir: &Path, max: u64) -> StorageConfig {
    StorageConfig {
        dir: dir.to_owned(),
        max_object_bytes: max,
    }
}

/// The store's directory inside a fresh temporary directory (so that the store creates it).
fn store_dir(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("store")
}

fn open_at(root: &tempfile::TempDir) -> LocalStore {
    LocalStore::open(&config(&store_dir(root), 1 << 30)).unwrap()
}

fn open_max(root: &tempfile::TempDir, max: u64) -> LocalStore {
    LocalStore::open(&config(&store_dir(root), max)).unwrap()
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

fn never_issued() -> ObjectKey {
    ObjectKey::parse("00000000-0000-4000-8000-000000000000").unwrap()
}

fn staging_file(root: &tempfile::TempDir, key: &ObjectKey) -> PathBuf {
    store_dir(root).join("staging").join(key.as_str())
}

fn object_file(root: &tempfile::TempDir, key: &ObjectKey) -> PathBuf {
    store_dir(root).join("objects").join(key.as_str())
}

async fn read_all(mut body: ObjectBody) -> Vec<u8> {
    let mut out = Vec::new();
    body.reader.read_to_end(&mut out).await.unwrap();
    assert_eq!(out.len() as u64, body.length);
    out
}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

/// A finished object of `content`.
async fn finished(store: &LocalStore, content: &[u8]) -> ObjectKey {
    let key = store.begin().await.unwrap();
    store.append(&key, 0, content.to_vec()).await.unwrap();
    store
        .finish(&key, sha(content), content.len() as u64)
        .await
        .unwrap();
    key
}

fn expect<T: std::fmt::Debug>(result: Result<T, StoreError>) -> StoreError {
    result.expect_err("the call must be refused")
}

// ---------------------------------------------------------------------------------------------
// T10: keys.

#[test]
fn only_a_canonical_version_4_uuid_is_a_key() {
    for good in [
        "00000000-0000-4000-8000-000000000000",
        "0190f5a0-1b2c-4d3e-9f40-516273849a0b",
        "ffffffff-ffff-4fff-bfff-ffffffffffff",
        "12345678-9abc-4def-a012-3456789abcde",
    ] {
        assert_eq!(ObjectKey::parse(good).unwrap().as_str(), good);
    }
    for bad in [
        "",
        "0190f5a0-1b2c-4d3e-9f40-516273849a0",
        "0190f5a0-1b2c-4d3e-9f40-516273849a0bb",
        "0190F5A0-1B2C-4D3E-9F40-516273849A0B",
        "0190f5a01b2c4d3e9f40516273849a0b",
        "0190f5a0-1b2c-1d3e-9f40-516273849a0b",
        "0190f5a0-1b2c-5d3e-9f40-516273849a0b",
        "0190f5a0-1b2c-4d3e-cf40-516273849a0b",
        "0190f5a0-1b2c-4d3e-0f40-516273849a0b",
        "0190f5a0-1b2c-4d3e-9f40-516273849a0g",
        "0190f5a0_1b2c_4d3e_9f40_516273849a0b",
        "../0190f5a0-1b2c-4d3e-9f40-516273849a0",
        "..",
        "staging/0190f5a0-1b2c-4d3e-9f40-516273849a",
        " 0190f5a0-1b2c-4d3e-9f40-516273849a0b",
        "0190f5a0-1b2c-4d3e-9f40-516273849a0b\n",
        "0190f5a0-1b2c-4d3e-9f40-51627384\u{e9}a0b",
        "{0190f5a0-1b2c-4d3e-9f40-516273849a0}",
    ] {
        assert!(
            matches!(ObjectKey::parse(bad), Err(StoreError::InvalidKey)),
            "{bad:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn begin_issues_distinct_valid_keys() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let mut seen = HashSet::new();
    for _ in 0..50 {
        let key = store.begin().await.unwrap();
        assert!(ObjectKey::parse(key.as_str()).is_ok(), "{key}");
        assert!(seen.insert(key));
    }
}

// ---------------------------------------------------------------------------------------------
// T1: every operation in every state.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_nobody_issued_is_not_found_by_every_operation() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = never_issued();
    assert!(matches!(
        expect(store.append(&key, 0, b"x".to_vec()).await),
        StoreError::NotFound
    ));
    assert!(matches!(
        expect(store.status(&key).await),
        StoreError::NotFound
    ));
    assert!(matches!(
        expect(store.finish(&key, sha(b""), 0).await),
        StoreError::NotFound
    ));
    assert!(matches!(
        expect(store.get(&key).await),
        StoreError::NotFound
    ));
    assert!(matches!(
        expect(store.delete(&key).await),
        StoreError::NotFound
    ));
    // An append never creates.
    assert!(!staging_file(&root, &key).exists());
    assert!(store.list_unfinished().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unfinished_object_takes_appends_only_at_its_stored_length() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 0 }
    );
    assert_eq!(store.append(&key, 0, b"abc".to_vec()).await.unwrap(), 3);
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 3 }
    );
    // Behind, ahead: refused with the stored length, nothing written.
    for offset in [0, 1, 2, 4, 5, u64::MAX] {
        assert!(matches!(
            expect(store.append(&key, offset, b"zz".to_vec()).await),
            StoreError::OffsetMismatch { stored: 3 }
        ));
    }
    assert_eq!(std::fs::read(staging_file(&root, &key)).unwrap(), b"abc");
    // Nothing appended at the stored length is the stored length.
    assert_eq!(store.append(&key, 3, Vec::new()).await.unwrap(), 3);
    // Reading an unfinished object is refused.
    assert!(matches!(
        expect(store.get(&key).await),
        StoreError::NotFinished
    ));
    assert_eq!(store.append(&key, 3, b"de".to_vec()).await.unwrap(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finish_that_does_not_match_leaves_the_object_unfinished() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    store.append(&key, 0, b"abcdef".to_vec()).await.unwrap();
    for length in [0, 5, 7] {
        assert!(matches!(
            expect(store.finish(&key, sha(b"abcdef"), length).await),
            StoreError::LengthMismatch { stored: 6 }
        ));
    }
    assert!(matches!(
        expect(store.finish(&key, sha(b"abcdeg"), 6).await),
        StoreError::HashMismatch
    ));
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 6 }
    );
    assert!(!object_file(&root, &key).exists());
    // It is still writable, and the right finish works.
    assert_eq!(store.append(&key, 6, b"g".to_vec()).await.unwrap(), 7);
    store.finish(&key, sha(b"abcdefg"), 7).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_object_is_immutable_and_readable() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let content = bytes(200_000, 3);
    let key = finished(&store, &content).await;
    assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
    assert!(matches!(
        expect(
            store
                .append(&key, content.len() as u64, b"x".to_vec())
                .await
        ),
        StoreError::AlreadyFinished
    ));
    assert!(matches!(
        expect(store.append(&key, 0, Vec::new()).await),
        StoreError::AlreadyFinished
    ));
    assert!(matches!(
        expect(
            store
                .finish(&key, sha(&content), content.len() as u64)
                .await
        ),
        StoreError::AlreadyFinished
    ));
    let body = store.get(&key).await.unwrap();
    assert_eq!(body.length, content.len() as u64);
    assert_eq!(read_all(body).await, content);
    assert!(!staging_file(&root, &key).exists());
    assert!(store.list_unfinished().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_object_can_be_finished() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    store.finish(&key, sha(b""), 0).await.unwrap();
    let body = store.get(&key).await.unwrap();
    assert_eq!(body.length, 0);
    assert!(read_all(body).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_removes_a_finished_and_an_unfinished_object_once() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let done = finished(&store, b"done").await;
    let open = store.begin().await.unwrap();
    store.append(&open, 0, b"part".to_vec()).await.unwrap();
    for key in [&done, &open] {
        store.delete(key).await.unwrap();
        assert!(matches!(
            expect(store.status(key).await),
            StoreError::NotFound
        ));
        assert!(matches!(expect(store.get(key).await), StoreError::NotFound));
        assert!(matches!(
            expect(store.append(key, 0, b"x".to_vec()).await),
            StoreError::NotFound
        ));
        assert!(matches!(
            expect(store.delete(key).await),
            StoreError::NotFound
        ));
    }
    assert!(!object_file(&root, &done).exists());
    assert!(!staging_file(&root, &open).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_store_is_usable_as_a_trait_object() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let dynamic: &dyn ObjectStore = &store;
    let key = dynamic.begin().await.unwrap();
    assert_eq!(dynamic.append(&key, 0, b"a".to_vec()).await.unwrap(), 1);
    dynamic.finish(&key, sha(b"a"), 1).await.unwrap();
    assert_eq!(dynamic.status(&key).await.unwrap(), ObjectStatus::Finished);
}

// ---------------------------------------------------------------------------------------------
// T2, T14: the size limit.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_append_past_the_limit_is_refused_and_exactly_the_limit_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    let store = open_max(&root, 16);
    let key = store.begin().await.unwrap();
    assert!(matches!(
        expect(store.append(&key, 0, bytes(17, 0)).await),
        StoreError::TooLarge { stored: 0, max: 16 }
    ));
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 0 }
    );
    assert_eq!(store.append(&key, 0, bytes(10, 0)).await.unwrap(), 10);
    assert!(matches!(
        expect(store.append(&key, 10, bytes(7, 1)).await),
        StoreError::TooLarge {
            stored: 10,
            max: 16
        }
    ));
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 10 }
    );
    assert_eq!(store.append(&key, 10, bytes(6, 1)).await.unwrap(), 16);
    // An append to a full object that adds nothing is not an excess; one that adds a byte is.
    assert_eq!(store.append(&key, 16, Vec::new()).await.unwrap(), 16);
    assert!(matches!(
        expect(store.append(&key, 16, bytes(1, 2)).await),
        StoreError::TooLarge {
            stored: 16,
            max: 16
        }
    ));
    // A wrong offset is judged before the limit.
    assert!(matches!(
        expect(store.append(&key, 3, bytes(100, 2)).await),
        StoreError::OffsetMismatch { stored: 16 }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_limit_lowered_across_a_restart_applies_to_appends_not_to_finish() {
    let root = tempfile::tempdir().unwrap();
    let key = {
        let store = open_max(&root, 100);
        let key = store.begin().await.unwrap();
        store.append(&key, 0, bytes(10, 0)).await.unwrap();
        key
    };
    let store = open_max(&root, 8);
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 10 }
    );
    assert!(matches!(
        expect(store.append(&key, 10, bytes(1, 0)).await),
        StoreError::TooLarge { stored: 10, max: 8 }
    ));
    assert_eq!(store.append(&key, 10, Vec::new()).await.unwrap(), 10);
    store.finish(&key, sha(&bytes(10, 0)), 10).await.unwrap();
    assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
}

// ---------------------------------------------------------------------------------------------
// T3: a chunked write resumed after a cut.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chunked_write_cut_in_the_middle_of_a_chunk_resumes_from_what_reached_the_disk() {
    let chunk1 = bytes(1000, 1);
    let chunk2 = bytes(777, 2);
    for k in [0, 1, 300, chunk2.len()] {
        let root = tempfile::tempdir().unwrap();
        let store = open_at(&root);
        let key = store.begin().await.unwrap();
        let old = store.append(&key, 0, chunk1.clone()).await.unwrap();
        assert_eq!(old, 1000);
        // The process dies: its store goes away and the second chunk's prefix is on the disk.
        drop(store);
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(staging_file(&root, &key))
                .unwrap();
            file.write_all(&chunk2[..k]).unwrap();
            file.sync_all().unwrap();
        }
        let store = open_at(&root);
        let stored = old + k as u64;
        assert_eq!(
            store.status(&key).await.unwrap(),
            ObjectStatus::Unfinished { stored }
        );
        // The writer resends the whole second chunk from where it thinks it is: when nothing of
        // the chunk reached the disk that is the stored length, and otherwise it is refused with
        // the length that did.
        let total = if k == 0 {
            store.append(&key, old, chunk2.clone()).await.unwrap()
        } else {
            assert!(matches!(
                expect(store.append(&key, old, chunk2.clone()).await),
                StoreError::OffsetMismatch { stored: s } if s == stored
            ));
            store
                .append(&key, stored, chunk2[k..].to_vec())
                .await
                .unwrap()
        };
        assert_eq!(total, (chunk1.len() + chunk2.len()) as u64);
        let whole: Vec<u8> = chunk1.iter().chain(&chunk2).copied().collect();
        store.finish(&key, sha(&whole), total).await.unwrap();
        assert_eq!(read_all(store.get(&key).await.unwrap()).await, whole);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_answer_is_recovered_from_the_refusal() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    // The append is stored; its answer never reaches the writer, which sends it again.
    assert_eq!(store.append(&key, 0, bytes(10, 0)).await.unwrap(), 10);
    assert!(matches!(
        expect(store.append(&key, 0, bytes(10, 0)).await),
        StoreError::OffsetMismatch { stored: 10 }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_begin_cut_before_its_answer_leaves_an_empty_object_that_is_listed() {
    let root = tempfile::tempdir().unwrap();
    let key = {
        let store = open_at(&root);
        store.begin().await.unwrap()
    };
    let store = open_at(&root);
    let listed = store.list_unfinished().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);
    assert_eq!(listed[0].stored, 0);
}

// ---------------------------------------------------------------------------------------------
// T4: failures at the points a test can break.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_that_fails_part_way_leaves_its_prefix_and_the_key_usable() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    *store.inner.hooks.write_fails_after.lock().unwrap() = Some(2);
    assert!(matches!(
        expect(store.append(&key, 0, b"abcdef".to_vec()).await),
        StoreError::Io { .. }
    ));
    *store.inner.hooks.write_fails_after.lock().unwrap() = None;
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 2 }
    );
    assert_eq!(store.append(&key, 2, b"cdef".to_vec()).await.unwrap(), 6);
    store.finish(&key, sha(b"abcdef"), 6).await.unwrap();
    assert_eq!(read_all(store.get(&key).await.unwrap()).await, b"abcdef");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_sync_answers_io_to_everything_but_delete_until_a_restart() {
    let root = tempfile::tempdir().unwrap();
    let other;
    let key;
    {
        let store = open_at(&root);
        other = store.begin().await.unwrap();
        key = store.begin().await.unwrap();
        // The sync of the length passes, the one after the write fails.
        store.inner.hooks.file_sync_skips.store(1, Ordering::SeqCst);
        store
            .inner
            .hooks
            .file_sync_fails
            .store(true, Ordering::SeqCst);
        assert!(matches!(
            expect(store.append(&key, 0, b"abc".to_vec()).await),
            StoreError::Io { .. }
        ));
        store
            .inner
            .hooks
            .file_sync_fails
            .store(false, Ordering::SeqCst);
        // The sync works again, and the key still answers Io.
        assert!(matches!(
            expect(store.status(&key).await),
            StoreError::Io { .. }
        ));
        assert!(matches!(
            expect(store.append(&key, 0, b"abc".to_vec()).await),
            StoreError::Io { .. }
        ));
        assert!(matches!(
            expect(store.finish(&key, sha(b"abc"), 3).await),
            StoreError::Io { .. }
        ));
        assert!(matches!(
            expect(store.get(&key).await),
            StoreError::Io { .. }
        ));
        // Another key is not affected, and the listing leaves the failed one out.
        assert_eq!(store.append(&other, 0, b"x".to_vec()).await.unwrap(), 1);
        let listed = store.list_unfinished().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, other);
    }
    // After a restart the length is what the file holds.
    let store = open_at(&root);
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 3 }
    );
    // Delete answers a failed key, and clears it.
    store
        .inner
        .hooks
        .file_sync_fails
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        expect(store.append(&key, 3, b"d".to_vec()).await),
        StoreError::Io { .. }
    ));
    store.delete(&key).await.unwrap();
    store
        .inner
        .hooks
        .file_sync_fails
        .store(false, Ordering::SeqCst);
    assert!(matches!(
        expect(store.status(&key).await),
        StoreError::NotFound
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_sync_that_fails_after_the_rename_reports_io_and_the_object_is_finished() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    store.append(&key, 0, b"abc".to_vec()).await.unwrap();
    store
        .inner
        .hooks
        .directory_sync_after_rename_fails
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        expect(store.finish(&key, sha(b"abc"), 3).await),
        StoreError::Io { .. }
    ));
    store
        .inner
        .hooks
        .directory_sync_after_rename_fails
        .store(false, Ordering::SeqCst);
    assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
    assert_eq!(read_all(store.get(&key).await.unwrap()).await, b"abc");
    assert!(matches!(
        expect(store.finish(&key, sha(b"abc"), 3).await),
        StoreError::AlreadyFinished
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rename_that_fails_leaves_the_object_unfinished_and_copies_nothing() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    store.append(&key, 0, b"abc".to_vec()).await.unwrap();
    store.inner.hooks.rename_fails.store(true, Ordering::SeqCst);
    assert!(matches!(
        expect(store.finish(&key, sha(b"abc"), 3).await),
        StoreError::Io { .. }
    ));
    assert!(!object_file(&root, &key).exists());
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 3 }
    );
    store
        .inner
        .hooks
        .rename_fails
        .store(false, Ordering::SeqCst);
    store.finish(&key, sha(b"abc"), 3).await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// T5: a dropped future does not free the key.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_append_still_runs_and_holds_the_key_until_it_ends() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(open_at(&root));
    let key = store.begin().await.unwrap();
    assert_eq!(store.append(&key, 0, b"aaaa".to_vec()).await.unwrap(), 4);
    let parker = Parker::new();
    *store.inner.hooks.park.lock().unwrap() = Some(Arc::clone(&parker));
    let first = {
        let (store, key) = (Arc::clone(&store), key.clone());
        tokio::spawn(async move { store.append(&key, 4, b"bb".to_vec()).await })
    };
    let arrived = Arc::clone(&parker);
    tokio::task::spawn_blocking(move || arrived.wait_entered())
        .await
        .unwrap();
    // The caller goes away while its write is in flight.
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let second = {
        let (store, key) = (Arc::clone(&store), key.clone());
        tokio::spawn(async move { store.append(&key, 4, b"cc".to_vec()).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !second.is_finished(),
        "the key was free while its write ran"
    );
    parker.release();
    assert!(matches!(
        expect(second.await.unwrap()),
        StoreError::OffsetMismatch { stored: 6 }
    ));
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 6 }
    );
    assert_eq!(std::fs::read(staging_file(&root, &key)).unwrap(), b"aaaabb");
}

// ---------------------------------------------------------------------------------------------
// T6: directories built by hand.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_existing_directory_with_one_area_or_none_gets_the_rest() {
    for areas in [
        &[][..],
        &["staging"][..],
        &["objects"][..],
        &["staging", "objects"][..],
    ] {
        let root = tempfile::tempdir().unwrap();
        let dir = store_dir(&root);
        std::fs::create_dir(&dir).unwrap();
        for area in areas {
            std::fs::create_dir(dir.join(area)).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for area in areas {
                std::fs::set_permissions(dir.join(area), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = open_at(&root);
        assert!(dir.join("staging").is_dir() && dir.join("objects").is_dir());
        assert!(dir.join(".lock").is_file());
        let key = finished(&store, b"x").await;
        assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_in_both_areas_is_finished_and_not_listed_and_delete_removes_both() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    store.append(&key, 0, b"abc".to_vec()).await.unwrap();
    std::fs::copy(staging_file(&root, &key), object_file(&root, &key)).unwrap();
    assert_eq!(store.status(&key).await.unwrap(), ObjectStatus::Finished);
    assert!(store.list_unfinished().await.unwrap().is_empty());
    assert!(matches!(
        expect(store.append(&key, 3, b"d".to_vec()).await),
        StoreError::AlreadyFinished
    ));
    assert_eq!(read_all(store.get(&key).await.unwrap()).await, b"abc");
    store.delete(&key).await.unwrap();
    assert!(!staging_file(&root, &key).exists());
    assert!(!object_file(&root, &key).exists());
    assert!(matches!(
        expect(store.status(&key).await),
        StoreError::NotFound
    ));
}

#[test]
fn open_refuses_a_relative_path_a_missing_parent_and_a_file() {
    let root = tempfile::tempdir().unwrap();
    assert!(matches!(
        LocalStore::open(&config(Path::new("relative/store"), 10)),
        Err(StoreError::DirectoryNotAbsolute)
    ));
    // No recursive create.
    let deep = root.path().join("absent").join("store");
    assert!(matches!(
        LocalStore::open(&config(&deep, 10)),
        Err(StoreError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        })
    ));
    assert!(!root.path().join("absent").exists());
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    assert!(matches!(
        LocalStore::open(&config(&file, 10)),
        Err(StoreError::NotADirectory)
    ));
}

#[cfg(unix)]
#[test]
fn a_new_directory_and_its_files_belong_to_the_server_account_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = store_dir(&root);
    assert_eq!(mode(dir.clone()), 0o700);
    assert_eq!(mode(dir.join("staging")), 0o700);
    assert_eq!(mode(dir.join("objects")), 0o700);
    assert_eq!(mode(dir.join(".lock")), 0o600);
    let key = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.begin())
        .unwrap();
    assert_eq!(mode(staging_file(&root, &key)), 0o600);
}

// ---------------------------------------------------------------------------------------------
// T7: races.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_appends_at_one_offset_give_one_success() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(open_at(&root));
    let key = store.begin().await.unwrap();
    store.append(&key, 0, bytes(5, 0)).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let (store, key) = (Arc::clone(&store), key.clone());
        tasks.push(tokio::spawn(async move {
            store.append(&key, 5, vec![i; 4]).await
        }));
    }
    let mut ok = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(9) => ok += 1,
            Err(StoreError::OffsetMismatch { stored: 9 }) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(
        store.status(&key).await.unwrap(),
        ObjectStatus::Unfinished { stored: 9 }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_finishes_give_one_success_and_one_already_finished() {
    for _ in 0..20 {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(open_at(&root));
        let key = store.begin().await.unwrap();
        store.append(&key, 0, b"abc".to_vec()).await.unwrap();
        let run = || {
            let (store, key) = (Arc::clone(&store), key.clone());
            tokio::spawn(async move { store.finish(&key, sha(b"abc"), 3).await })
        };
        let (a, b) = (run(), run());
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        let ok = [&a, &b].iter().filter(|r| r.is_ok()).count();
        let already = [&a, &b]
            .iter()
            .filter(|r| matches!(r, Err(StoreError::AlreadyFinished)))
            .count();
        assert_eq!((ok, already), (1, 1), "{a:?} {b:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_append_and_a_finish_are_serialised() {
    for _ in 0..20 {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(open_at(&root));
        let key = store.begin().await.unwrap();
        store.append(&key, 0, b"abc".to_vec()).await.unwrap();
        let append = {
            let (store, key) = (Arc::clone(&store), key.clone());
            tokio::spawn(async move { store.append(&key, 3, b"d".to_vec()).await })
        };
        let finish = {
            let (store, key) = (Arc::clone(&store), key.clone());
            tokio::spawn(async move { store.finish(&key, sha(b"abc"), 3).await })
        };
        let (append, finish) = (append.await.unwrap(), finish.await.unwrap());
        match (&append, &finish) {
            // The append went first: the finish saw four bytes.
            (Ok(4), Err(StoreError::LengthMismatch { stored: 4 })) => {}
            // The finish went first: the append found a finished object.
            (Err(StoreError::AlreadyFinished), Ok(())) => {}
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_is_serialised_with_every_other_call() {
    for _ in 0..20 {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(open_at(&root));
        let key = store.begin().await.unwrap();
        store.append(&key, 0, b"abc".to_vec()).await.unwrap();
        let delete = |store: &Arc<LocalStore>, key: &ObjectKey| {
            let (store, key) = (Arc::clone(store), key.clone());
            tokio::spawn(async move { store.delete(&key).await })
        };
        let append = {
            let (store, key) = (Arc::clone(&store), key.clone());
            tokio::spawn(async move { store.append(&key, 3, b"d".to_vec()).await })
        };
        let finish = {
            let (store, key) = (Arc::clone(&store), key.clone());
            tokio::spawn(async move { store.finish(&key, sha(b"abc"), 3).await })
        };
        let (d1, d2) = (delete(&store, &key), delete(&store, &key));
        let (append, finish) = (append.await.unwrap(), finish.await.unwrap());
        let (d1, d2) = (d1.await.unwrap(), d2.await.unwrap());
        // Of two deletes one removes the object and the other finds nothing; the first delete
        // is never refused.
        assert_eq!(
            [&d1, &d2].iter().filter(|r| r.is_ok()).count(),
            1,
            "{d1:?} {d2:?}"
        );
        assert!(
            [&d1, &d2]
                .iter()
                .any(|r| matches!(r, Err(StoreError::NotFound))),
            "{d1:?} {d2:?}"
        );
        // Whatever ran after the removal found no object, and whatever ran before it worked.
        assert!(
            append.is_ok()
                || matches!(
                    append,
                    Err(StoreError::NotFound | StoreError::AlreadyFinished)
                ),
            "{append:?}"
        );
        assert!(
            finish.is_ok()
                || matches!(
                    finish,
                    Err(StoreError::NotFound | StoreError::LengthMismatch { .. })
                ),
            "{finish:?}"
        );
        assert!(matches!(
            expect(store.status(&key).await),
            StoreError::NotFound
        ));
        assert!(!staging_file(&root, &key).exists() && !object_file(&root, &key).exists());
    }
}

// ---------------------------------------------------------------------------------------------
// T8: a read survives a delete (Unix: Windows keeps a deleted file's name until it is closed).

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_opened_before_a_delete_stays_complete() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let content = bytes(1_000_000, 9);
    let key = finished(&store, &content).await;
    let mut body = store.get(&key).await.unwrap();
    let mut first = vec![0u8; 1000];
    body.reader.read_exact(&mut first).await.unwrap();
    store.delete(&key).await.unwrap();
    let mut rest = Vec::new();
    body.reader.read_to_end(&mut rest).await.unwrap();
    first.extend(rest);
    assert_eq!(first, content);
    assert!(matches!(
        expect(store.get(&key).await),
        StoreError::NotFound
    ));
}

// ---------------------------------------------------------------------------------------------
// T9: one process per directory.

#[test]
fn a_second_store_on_one_directory_is_refused_until_the_first_is_gone() {
    let root = tempfile::tempdir().unwrap();
    let first = open_at(&root);
    assert!(matches!(
        LocalStore::open(&config(&store_dir(&root), 10)),
        Err(StoreError::InUse)
    ));
    drop(first);
    let again = open_at(&root);
    drop(again);
    // The lock file is kept.
    assert!(store_dir(&root).join(".lock").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_call_keeps_the_directory_locked_after_its_store_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let key = store.begin().await.unwrap();
    let parker = Parker::new();
    *store.inner.hooks.park.lock().unwrap() = Some(Arc::clone(&parker));
    let call = {
        let inner = Arc::clone(&store.inner);
        let key = key.clone();
        let guard = inner.lock_key(&key).await;
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            inner.append(&key, 0, b"x")
        })
    };
    let arrived = Arc::clone(&parker);
    tokio::task::spawn_blocking(move || arrived.wait_entered())
        .await
        .unwrap();
    drop(store);
    assert!(matches!(
        LocalStore::open(&config(&store_dir(&root), 10)),
        Err(StoreError::InUse)
    ));
    parker.release();
    assert_eq!(call.await.unwrap().unwrap(), 1);
    drop(open_at(&root));
}

// ---------------------------------------------------------------------------------------------
// T11: the directory checks (Unix).

#[cfg(unix)]
mod directory_checks {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_directory_that_group_or_others_may_write_is_refused() {
        for mode in [0o770, 0o707, 0o777, 0o720, 0o702] {
            let root = tempfile::tempdir().unwrap();
            let dir = store_dir(&root);
            std::fs::create_dir(&dir).unwrap();
            chmod(&dir, mode);
            assert!(
                matches!(
                    LocalStore::open(&config(&dir, 10)),
                    Err(StoreError::DirectoryWritable)
                ),
                "{mode:o}"
            );
            // Nothing was created in it.
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_directory_that_others_may_read_but_not_write_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let dir = store_dir(&root);
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o755);
        drop(LocalStore::open(&config(&dir, 10)).unwrap());
    }

    #[test]
    fn an_area_that_others_may_write_is_refused() {
        for area in ["staging", "objects"] {
            let root = tempfile::tempdir().unwrap();
            drop(open_at(&root));
            chmod(&store_dir(&root).join(area), 0o777);
            assert!(matches!(
                LocalStore::open(&config(&store_dir(&root), 10)),
                Err(StoreError::DirectoryWritable)
            ));
        }
    }

    #[test]
    fn a_symbolic_link_to_a_good_directory_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        chmod(&real, 0o700);
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();
        drop(LocalStore::open(&config(&link, 10)).unwrap());
        assert!(real.join("staging").is_dir());
    }

    #[test]
    fn a_directory_of_another_account_is_refused_by_its_owner() {
        let other = fs_guard::euid() + 1;
        assert_eq!(
            fs_guard::check_directory_meta(true, other, 0o700),
            Err(DirectoryFault::NotOwned)
        );
        assert_eq!(fs_guard::check_directory_meta(true, 0, 0o755), Ok(()));
        assert_eq!(
            fs_guard::check_directory_meta(true, fs_guard::euid(), 0o700),
            Ok(())
        );
        assert_eq!(
            fs_guard::check_directory_meta(false, 0, 0o700),
            Err(DirectoryFault::NotADirectory)
        );
        assert_eq!(
            fs_guard::check_directory_meta(true, 0, 0o720),
            Err(DirectoryFault::Writable)
        );
    }

    #[test]
    fn a_lock_file_that_is_a_symbolic_link_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = store_dir(&root);
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o700);
        let target = root.path().join("elsewhere");
        std::fs::write(&target, b"x").unwrap();
        symlink(&target, dir.join(".lock")).unwrap();
        assert!(matches!(
            LocalStore::open(&config(&dir, 10)),
            Err(StoreError::NotRegularFile)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_staging_file_that_is_a_symbolic_link_is_refused_and_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let store = open_at(&root);
        let key = store.begin().await.unwrap();
        let target = root.path().join("elsewhere");
        std::fs::write(&target, b"secret").unwrap();
        std::fs::remove_file(staging_file(&root, &key)).unwrap();
        symlink(&target, staging_file(&root, &key)).unwrap();
        assert!(matches!(
            expect(store.append(&key, 0, b"x".to_vec()).await),
            StoreError::NotRegularFile
        ));
        assert!(matches!(
            expect(store.status(&key).await),
            StoreError::NotRegularFile
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
        // The listing skips it with a warning.
        assert!(store.list_unfinished().await.unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// T12: a removed directory (Unix).

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_removed_under_a_live_store_is_unavailable_and_never_recreated() {
    for area in ["staging", "objects", ""] {
        let root = tempfile::tempdir().unwrap();
        let store = open_at(&root);
        let done = finished(&store, b"done").await;
        let open = store.begin().await.unwrap();
        store.append(&open, 0, b"part".to_vec()).await.unwrap();
        let removed = if area.is_empty() {
            store_dir(&root)
        } else {
            store_dir(&root).join(area)
        };
        std::fs::remove_dir_all(&removed).unwrap();
        for key in [&done, &open, &never_issued()] {
            assert!(
                matches!(expect(store.status(key).await), StoreError::Unavailable),
                "{area}"
            );
            assert!(matches!(
                expect(store.append(key, 0, b"x".to_vec()).await),
                StoreError::Unavailable
            ));
            assert!(matches!(
                expect(store.finish(key, sha(b""), 0).await),
                StoreError::Unavailable
            ));
            assert!(matches!(
                expect(store.get(key).await),
                StoreError::Unavailable
            ));
            assert!(matches!(
                expect(store.delete(key).await),
                StoreError::Unavailable
            ));
        }
        assert!(matches!(
            expect(store.begin().await),
            StoreError::Unavailable
        ));
        assert!(matches!(
            expect(store.list_unfinished().await),
            StoreError::Unavailable
        ));
        assert!(!removed.exists(), "the store recreated {area}");
    }
}

// ---------------------------------------------------------------------------------------------
// T13: the listing.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_listing_has_each_unfinished_object_with_its_length_and_time() {
    let root = tempfile::tempdir().unwrap();
    let store = open_at(&root);
    let _done = finished(&store, b"finished").await;
    let a = store.begin().await.unwrap();
    let b = store.begin().await.unwrap();
    let c = store.begin().await.unwrap();
    store.append(&b, 0, bytes(5, 0)).await.unwrap();
    store.append(&c, 0, bytes(1234, 0)).await.unwrap();
    // The modification time is the file's; set one whole second, which every file system keeps.
    let when = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    OpenOptions::new()
        .write(true)
        .open(staging_file(&root, &b))
        .unwrap()
        .set_modified(when)
        .unwrap();
    // Names that are no key are ignored; a directory with a key's name is skipped.
    let staging = store_dir(&root).join("staging");
    std::fs::write(staging.join("README"), b"x").unwrap();
    std::fs::write(staging.join("0190F5A0-1B2C-4D3E-9F40-516273849A0B"), b"x").unwrap();
    let odd = never_issued();
    std::fs::create_dir(staging.join(odd.as_str())).unwrap();

    let listed = store.list_unfinished().await.unwrap();
    let find = |key: &ObjectKey| listed.iter().find(|o| &o.key == key).unwrap();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(find(&a).stored, 0);
    assert_eq!(find(&b).stored, 5);
    assert_eq!(find(&b).modified, when);
    assert_eq!(find(&c).stored, 1234);
    let now = std::time::SystemTime::now();
    let age = now.duration_since(find(&c).modified).unwrap_or_default();
    assert!(age < Duration::from_secs(60), "{age:?}");
}

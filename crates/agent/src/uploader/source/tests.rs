use std::fs;
use std::io::Read;

use tokio::sync::watch;

use super::*;

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn flag() -> AtomicBool {
    AtomicBool::new(false)
}

fn stop() -> (watch::Sender<bool>, Stop) {
    let (tx, rx) = watch::channel(false);
    (tx, Stop(rx))
}

// V1, V7
#[test]
fn the_first_length_bytes_are_hashed_whatever_the_file_has_grown_to() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"0123456789 and then some more").unwrap();
    assert_eq!(
        check(&path, 10, &sha(b"0123456789"), &flag()),
        Verdict::Verified
    );
    // The same length, another digest.
    assert_eq!(
        check(&path, 10, &sha(b"0123456780"), &flag()),
        Verdict::Broken("source-changed")
    );
}

// V3
#[test]
fn a_file_shorter_than_the_entry_is_broken() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"0123").unwrap();
    assert_eq!(
        check(&path, 10, &sha(b"0123456789"), &flag()),
        Verdict::Broken("source-short")
    );
}

// V8
#[test]
fn a_length_of_zero_is_checked_against_the_empty_digest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"anything").unwrap();
    assert_eq!(check(&path, 0, &sha(b""), &flag()), Verdict::Verified);
    assert_eq!(
        check(&path, 0, &sha(b"x"), &flag()),
        Verdict::Broken("source-changed")
    );
}

// V2, V4
#[test]
fn a_missing_file_is_broken_but_a_missing_directory_or_an_unreadable_file_is_not() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        check(&dir.path().join("gone"), 1, &sha(b"x"), &flag()),
        Verdict::Broken("source-missing")
    );
    assert_eq!(
        check(
            &dir.path().join("no-dir").join("log"),
            1,
            &sha(b"x"),
            &flag()
        ),
        Verdict::Transient
    );
    // A directory is no file: opening it fails (Windows) or reading it does (Unix).
    assert_eq!(
        check(dir.path(), 1, &sha(b"x"), &flag()),
        Verdict::Transient
    );
}

#[tokio::test]
async fn a_stop_ends_the_check_and_the_hash_is_told_to_stop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"abc").unwrap();
    let (tx, stop) = stop();
    assert_eq!(
        verify(path.clone(), 3, sha(b"abc"), &stop).await,
        Verdict::Verified
    );
    tx.send_replace(true);
    assert_eq!(verify(path, 3, sha(b"abc"), &stop).await, Verdict::Stopped);
}

#[test]
fn a_cancelled_hash_stops() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"abc").unwrap();
    let cancel = AtomicBool::new(true);
    assert_eq!(check(&path, 3, &sha(b"abc"), &cancel), Verdict::Stopped);
}

// V5, V7
#[test]
fn the_reader_gives_its_part_only_and_flags_a_shortfall() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log");
    fs::write(&path, b"0123456789 grown").unwrap();
    let unsound = Arc::new(AtomicBool::new(false));
    let mut part = Vec::new();
    PartReader::new(path.clone(), 4, 4, unsound.clone())
        .read_to_end(&mut part)
        .unwrap();
    assert_eq!(part, b"4567");
    assert!(!unsound.load(Ordering::Relaxed));

    // The file is shorter than the part wants: the reader ends early and says so.
    fs::write(&path, b"0123456").unwrap();
    let mut part = Vec::new();
    PartReader::new(path.clone(), 4, 6, unsound.clone())
        .read_to_end(&mut part)
        .unwrap();
    assert_eq!(part, b"456");
    assert!(unsound.load(Ordering::Relaxed));

    // A file that is gone is an error, and flagged too.
    unsound.store(false, Ordering::Relaxed);
    fs::remove_file(&path).unwrap();
    assert!(
        PartReader::new(path, 0, 3, unsound.clone())
            .read(&mut [0u8; 3])
            .is_err()
    );
    assert!(unsound.load(Ordering::Relaxed));
}

// V8
#[test]
fn an_empty_part_reads_nothing_and_opens_nothing() {
    let unsound = Arc::new(AtomicBool::new(false));
    let mut reader = PartReader::new(PathBuf::from("/does/not/exist"), 0, 0, unsound.clone());
    assert_eq!(reader.read(&mut [0u8; 8]).unwrap(), 0);
    assert!(!unsound.load(Ordering::Relaxed));
}

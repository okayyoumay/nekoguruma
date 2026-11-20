//! The upload ledger across a crash (design 5.2, ADR-493): a child process enqueues and finishes
//! uploads up to a crash point and aborts, as a crashing agent would, and the parent opens what
//! it left and checks that every entry is either there with its bytes or gone with its object
//! swept, and that the counter never gives a number twice.
//!
//! Each case runs in a child process: the test executable starts itself again with [`CASE_ENV`]
//! set to the case's name.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use agent::journal::JobKey;
use agent::upload_ledger::{Probe, Step, UploadLedger};
use shared_proto::JobId;

/// Set in a child process to the case it runs.
const CASE_ENV: &str = "NGR_TEST_UPLOAD_LEDGER_CRASH_CASE";
/// Set in a child process to the ledger directory.
const DIR_ENV: &str = "NGR_TEST_UPLOAD_LEDGER_DIR";
/// This test's name, which a child process runs alone.
const TEST_NAME: &str = "the_upload_ledger_reads_back_after_a_crash_in_any_write";
/// Printed by a child right before it aborts, so a failed assertion is not taken for the crash.
const CRASHING: &str = "upload-ledger-crash-point-reached";
const ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
/// Where a child keeps a copy of the file as it was before the crashing write.
const OLD_COPY: &str = "old-copy";

fn key(n: u8) -> JobKey {
    JobKey {
        job_id: JobId(format!("job-{n}")),
        generation: 1,
    }
}

fn bytes_of(n: u8) -> Vec<u8> {
    format!("result {n}").into_bytes()
}

/// A probe that aborts at `step` once `armed` is set.
fn aborting_at(step: Step, armed: &Arc<AtomicBool>) -> Probe {
    let armed = armed.clone();
    Box::new(move |at| {
        if at == step && armed.load(Ordering::SeqCst) {
            println!("{CRASHING}");
            std::process::abort();
        }
        Ok(())
    })
}

fn objects(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.join(format!("{ID}.objects")))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn numbers(ledger: &UploadLedger) -> Vec<u64> {
    ledger.pending().map(|entry| entry.seq).collect()
}

/// The child's work: the crash point of `case` is the last thing it does.
fn run(case: &str, dir: &Path) {
    let armed = Arc::new(AtomicBool::new(false));
    let abort_at = |step| aborting_at(step, &armed);
    match case {
        "object-after-write"
        | "append-before-write"
        | "append-after-write"
        | "append-after-sync" => {
            let step = match case {
                "object-after-write" => Step::AfterObjectWrite,
                "append-before-write" => Step::BeforeWrite,
                "append-after-write" => Step::AfterWrite,
                _ => Step::AfterSync,
            };
            let mut ledger = UploadLedger::open_with(dir, ID, abort_at(step)).unwrap();
            armed.store(true, Ordering::SeqCst);
            ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap();
        }
        "finished-before-rename" | "finished-after-rename" | "finished-before-delete" => {
            let step = match case {
                "finished-before-rename" => Step::BeforeRename,
                "finished-after-rename" => Step::AfterRename,
                _ => Step::BeforeObjectDelete,
            };
            let mut ledger = UploadLedger::open_with(dir, ID, abort_at(step)).unwrap();
            ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap();
            ledger.enqueue_result(&key(2), &bytes_of(2)).unwrap();
            fs::copy(dir.join(ID), dir.join(OLD_COPY)).unwrap();
            armed.store(true, Ordering::SeqCst);
            ledger.finished(1).unwrap();
        }
        "create-before-rename" | "create-after-rename" => {
            let step = if case == "create-before-rename" {
                Step::BeforeRename
            } else {
                Step::AfterRename
            };
            armed.store(true, Ordering::SeqCst);
            let _ = UploadLedger::open_with(dir, ID, abort_at(step));
        }
        "open-after-cut" | "open-before-sweep" => {
            let mut ledger = UploadLedger::open_in(dir, ID).unwrap();
            ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap();
            ledger.enqueue_result(&key(2), &bytes_of(2)).unwrap();
            drop(ledger);
            let step = if case == "open-after-cut" {
                // Tear the last frame, as a crash in its append would.
                let path = dir.join(ID);
                let len = fs::metadata(&path).unwrap().len();
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(len - 3)
                    .unwrap();
                Step::AfterCut
            } else {
                fs::write(dir.join(format!("{ID}.objects")).join("9"), b"stray").unwrap();
                Step::BeforeObjectDelete
            };
            armed.store(true, Ordering::SeqCst);
            let _ = UploadLedger::open_with(dir, ID, abort_at(step));
        }
        other => panic!("unknown case {other}"),
    }
    panic!("the crash point was not reached");
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(case: &str) -> TempDir {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ngr-upload-crash-{case}-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn crash(case: &str) -> TempDir {
    let dir = temp_dir(case);
    let output = Command::new(std::env::current_exe().expect("test executable path"))
        .args([TEST_NAME, "--exact", "--nocapture"])
        .env(CASE_ENV, case)
        .env(DIR_ENV, &dir.0)
        .output()
        .expect("the child process should start");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(CRASHING),
        "{case}: the child failed before its crash point: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{case}: the child should abort");
    dir
}

fn stale_tmp(dir: &Path) -> bool {
    fs::read_dir(dir)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
}

#[test]
fn the_upload_ledger_reads_back_after_a_crash_in_any_write() {
    if let Ok(case) = std::env::var(CASE_ENV) {
        run(
            &case,
            Path::new(&std::env::var_os(DIR_ENV).expect("the directory")),
        );
        return;
    }

    // R1: the process dies after the object is written, or before the append: no entry, the
    // object is swept at the next open and the number is used again.
    for case in ["object-after-write", "append-before-write"] {
        let dir = crash(case);
        let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
        assert!(ledger.next().is_none(), "{case}");
        assert!(objects(&dir.0).is_empty(), "{case}");
        assert_eq!(
            ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap(),
            1,
            "{case}"
        );
    }

    // R2: the process dies after the frame is written. A killed process leaves the OS's cache
    // intact, so the frame is whole and the entry pending; with the frame cut short, as a power
    // loss can leave it, the entry is absent, its object swept and its number used again.
    let dir = crash("append-after-write");
    let ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1]);
    let dir = crash("append-after-write");
    let path = dir.0.join(ID);
    let len = fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 5)
        .unwrap();
    let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert!(ledger.next().is_none());
    assert!(objects(&dir.0).is_empty());
    assert_eq!(ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap(), 1);

    // R3: the process dies after the append is synced, before the call returns: the entry is
    // pending with its object, and the caller's retry finds it.
    let dir = crash("append-after-sync");
    let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1]);
    let entry = ledger.next().unwrap().clone();
    assert_eq!(fs::read(ledger.object_path(&entry)).unwrap(), bytes_of(1));
    assert_eq!(ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap(), 1);
    assert_eq!(ledger.highest(), 1);

    // R4: the process dies in the rewrite before the rename: the old file stands, the entry is
    // pending, its object is kept and the temporary file goes at the open.
    let dir = crash("finished-before-rename");
    assert!(stale_tmp(&dir.0));
    let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1, 2]);
    assert_eq!(objects(&dir.0), ["1", "2"]);
    assert!(!stale_tmp(&dir.0));
    ledger.finished(1).unwrap();
    assert_eq!(objects(&dir.0), ["2"]);

    // R5: the process dies after the rename, before it is synced. Either file may be the one
    // that survives a power loss: the new one has no entry and its object is swept; the old one
    // has the entry pending with its object.
    let dir = crash("finished-after-rename");
    let ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [2]);
    assert_eq!(objects(&dir.0), ["2"]);
    drop(ledger);
    let dir = crash("finished-after-rename");
    fs::copy(dir.0.join(OLD_COPY), dir.0.join(ID)).unwrap();
    let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1, 2]);
    assert_eq!(objects(&dir.0), ["1", "2"]);
    assert_eq!(ledger.enqueue_result(&key(3), &bytes_of(3)).unwrap(), 3);

    // R6: the process dies after the removal is durable, before the object is deleted: the
    // entry is gone and the open sweeps the object.
    let dir = crash("finished-before-delete");
    assert_eq!(objects(&dir.0), ["1", "2"]);
    let ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [2]);
    assert_eq!(objects(&dir.0), ["2"]);

    // R7: the process dies while the file is created: the next open creates it.
    for case in ["create-before-rename", "create-after-rename"] {
        let dir = crash(case);
        let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
        assert!(ledger.next().is_none(), "{case}");
        assert!(!stale_tmp(&dir.0), "{case}");
        assert_eq!(
            ledger.enqueue_result(&key(1), &bytes_of(1)).unwrap(),
            1,
            "{case}"
        );
    }

    // R8: the process dies in the open's cut or sweep: the next open repeats it.
    let dir = crash("open-after-cut");
    let mut ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1]);
    assert_eq!(objects(&dir.0), ["1"]);
    assert_eq!(ledger.enqueue_result(&key(2), &bytes_of(2)).unwrap(), 2);
    let dir = crash("open-before-sweep");
    let ledger = UploadLedger::open_in(&dir.0, ID).unwrap();
    assert_eq!(numbers(&ledger), [1, 2]);
    assert_eq!(objects(&dir.0), ["1", "2"]);
}

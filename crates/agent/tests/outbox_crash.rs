//! The outbox across a crash (ADR-478): a child process appends and acknowledges events up to a
//! crash point and aborts, as a crashing agent would, and the parent opens the file it left and
//! checks that every unacknowledged event is there and the counter continues without a gap.
//!
//! Each case runs in a child process: the test executable starts itself again with [`CASE_ENV`]
//! set to the case's name.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent::outbox::{Kind, Outbox, Probe, Step};

/// Set in a child process to the case it runs.
const CASE_ENV: &str = "NGR_TEST_OUTBOX_CRASH_CASE";
/// Set in a child process to the outbox directory.
const DIR_ENV: &str = "NGR_TEST_OUTBOX_DIR";
/// This test's name, which a child process runs alone.
const TEST_NAME: &str = "the_outbox_reads_back_after_a_crash_in_an_append_or_a_rewrite";
/// Printed by a child right before it aborts, so a failed assertion is not taken for the crash.
const CRASHING: &str = "outbox-crash-point-reached";
const ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";

fn text(seq: u64) -> String {
    format!(r#"{{"type":"agentEvent","seq":{seq}}}"#)
}

/// A text of about 40 KiB, so that two acknowledged events pass the rewrite threshold.
fn big(seq: u64) -> String {
    format!("{seq}:{}", "x".repeat(40 << 10))
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

/// The child's work: the first append of `case` is the one that crashes (S4), or the
/// acknowledgement that triggers the rewrite (S10).
fn run(case: &str, dir: &Path) {
    let (step, rewrite) = match case {
        "append-before-write" => (Step::BeforeWrite, false),
        "append-after-write" => (Step::AfterWrite, false),
        "append-after-sync" => (Step::AfterSync, false),
        "rewrite-before-rename" => (Step::BeforeRename, true),
        "rewrite-after-rename" => (Step::AfterRename, true),
        other => panic!("unknown case {other}"),
    };
    let armed = Arc::new(AtomicBool::new(false));
    let mut outbox = Outbox::open_with(dir, ID, aborting_at(step, &armed)).unwrap();
    if rewrite {
        for _ in 0..4 {
            outbox.append(Kind::Other, big).unwrap();
        }
        armed.store(true, Ordering::SeqCst);
        // Two acknowledged events of 40 KiB pass the rewrite threshold.
        outbox.ack(2).unwrap();
    } else {
        for _ in 0..2 {
            outbox.append(Kind::Other, text).unwrap();
        }
        armed.store(true, Ordering::SeqCst);
        outbox.append(Kind::Other, text).unwrap();
    }
    panic!("the crash point was not reached");
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(case: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("ngr-outbox-crash-{case}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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

fn numbers(outbox: &Outbox) -> Vec<u64> {
    outbox.pending().map(|(seq, _)| seq).collect()
}

#[test]
fn the_outbox_reads_back_after_a_crash_in_an_append_or_a_rewrite() {
    if let Ok(case) = std::env::var(CASE_ENV) {
        run(
            &case,
            Path::new(&std::env::var_os(DIR_ENV).expect("the directory")),
        );
        return;
    }

    // S4: the process dies around the third append. A killed process leaves the OS's cache
    // intact, so a frame written before the abort is whole; one never written is absent. A tear
    // inside a frame is the unit test's. Either way the numbers are contiguous and the next one
    // follows the last.
    for (case, held) in [
        ("append-before-write", 2),
        ("append-after-write", 3),
        ("append-after-sync", 3),
    ] {
        let dir = crash(case);
        let mut outbox = Outbox::open_in(&dir.0, ID).unwrap();
        assert_eq!(numbers(&outbox), (1..=held).collect::<Vec<_>>(), "{case}");
        assert_eq!(
            outbox.append(Kind::Other, text).unwrap(),
            held + 1,
            "{case}"
        );
        let texts: Vec<_> = outbox.pending().map(|(_, t)| t.to_owned()).collect();
        assert_eq!(texts[0], text(1), "{case}: byte-identical");
    }

    // S10: the process dies in the rewrite that drops the acknowledged prefix. Before the
    // rename the old file stands and the stale temporary file goes at the next open; after it the
    // new file stands. The counter is the same and every unacknowledged event is there.
    for (case, first) in [("rewrite-before-rename", 1), ("rewrite-after-rename", 3)] {
        let dir = crash(case);
        let mut outbox = Outbox::open_in(&dir.0, ID).unwrap();
        assert_eq!(numbers(&outbox), (first..=4).collect::<Vec<_>>(), "{case}");
        assert_eq!(outbox.append(Kind::Other, text).unwrap(), 5, "{case}");
        let stale = std::fs::read_dir(&dir.0)
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stale, "{case}: a temporary file is left");
    }
}

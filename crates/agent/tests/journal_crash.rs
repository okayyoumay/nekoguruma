//! The write-job journal across a crash (ADR-244): a child process commits a write job's
//! records up to a crash point and aborts, as a crashing agent would, and the parent opens the
//! journal it left and checks what reads back.
//!
//! The test runs each crash in a child process: the test executable starts itself again with
//! [`CRASH_ENV`] set to the number of commits to make before aborting.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agent::journal::{
    JobKey, Journal, JournalError, JournalState, RecoveryFacts, StageId, StepRef, TransferAttempt,
    TransferExit, Vin,
};
use shared_proto::JobId;

/// Set in a child process to the number of commits it makes before it aborts.
const CRASH_ENV: &str = "NGR_TEST_JOURNAL_CRASH_AFTER";
/// Set in a child process to the journal directory.
const DIR_ENV: &str = "NGR_TEST_JOURNAL_DIR";
/// This test's name, which a child process runs alone.
const TEST_NAME: &str = "the_journal_reads_back_after_a_crash_at_each_commit";
/// Printed by a child right before it aborts, so a failed assertion is not taken for the crash.
const CRASHING: &str = "journal-crash-point-reached";

const STAGE: StageId = StageId(1);

fn key() -> JobKey {
    JobKey {
        job_id: JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
        generation: 3,
    }
}

fn at(pc: u32, steps: u64) -> StepRef {
    StepRef { pc, steps }
}

type Commit = fn(&mut Journal) -> Result<(), JournalError>;

/// A write job's commits in order: the run start (ADR-272), identity, a resume, a transfer through RequestTransferExit
/// and its post-transfer steps, then a second resume whose new transfer start clears them.
const SCRIPT: &[(&str, Commit)] = &[
    ("run start", |j| j.commit_run_start(at(0, 0), b"initial")),
    ("hardware part number", |j| {
        j.commit_ecu_hardware_part_number(b"NGR-SIM-ECU")
    }),
    ("pre-erase version", |j| {
        j.commit_pre_erase_software_version(b"1.0.0")
    }),
    ("resume", |j| j.commit_resume(STAGE, None).map(drop)),
    ("programming session", |j| {
        j.commit_step(at(2, 10), Some(b"vm"))
    }),
    ("transfer-start marker", |j| {
        j.commit_transfer_start(STAGE, at(3, 11))
    }),
    ("RequestDownload", |j| j.commit_step(at(3, 11), None)),
    ("block 0", |j| j.commit_block(0)),
    ("block 1", |j| j.commit_block(1)),
    ("block 2", |j| j.commit_block(2)),
    ("RequestTransferExit marker", |j| {
        j.commit_transfer_exit_intent(at(5, 20))
    }),
    ("RequestTransferExit", |j| j.commit_step(at(5, 20), None)),
    ("CheckMemory", |j| j.commit_step(at(6, 21), None)),
    ("post-transfer complete", |j| {
        j.commit_post_transfer_complete()
    }),
    ("second resume", |j| {
        j.commit_resume(STAGE, Some(b"attempt-2")).map(drop)
    }),
    ("second transfer-start marker", |j| {
        j.commit_transfer_start(STAGE, at(3, 30))
    }),
    ("second RequestDownload", |j| j.commit_step(at(3, 30), None)),
    ("second block 0", |j| j.commit_block(0)),
];

/// Creates the journal in `dir` and makes the first `commits` commits.
fn run_script(dir: &Path, commits: usize) -> Journal {
    let mut journal =
        Journal::create(dir, &key(), None, None).expect("the journal should be created");
    for (name, commit) in &SCRIPT[..commits] {
        commit(&mut journal).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
    journal
}

/// Removes the temporary directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ngr-journal-crash-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temporary directory should be created");
    TempDir(dir)
}

/// The points [`check_named_point`] spells out.
const NAMED_POINTS: [&str; 14] = [
    "created",
    "run start",
    "pre-erase version",
    "resume",
    "programming session",
    "transfer-start marker",
    "block 1",
    "block 2",
    "RequestTransferExit marker",
    "CheckMemory",
    "post-transfer complete",
    "second resume",
    "second transfer-start marker",
    "second block 0",
];

/// What the journal must hold after `commits` commits, spelled out for the points the restart
/// order depends on. Returns the point's name if it is one of them.
fn check_named_point(commits: usize, state: &JournalState) -> Option<&'static str> {
    let facts = &state.facts;
    let first_transfer = |last_block, exit| {
        Some(TransferAttempt {
            stage: STAGE,
            started_at: at(3, 11),
            last_block,
            exit,
            interrupted: false,
        })
    };
    let exit = |last_post_step, complete| {
        Some(TransferExit {
            intent_at: at(5, 20),
            last_post_step,
            complete,
        })
    };
    let name = if commits == 0 {
        "created"
    } else {
        SCRIPT[commits - 1].0
    };
    match name {
        "created" => assert_eq!(
            *facts,
            RecoveryFacts {
                key: key(),
                last_step: None,
                ecu_hardware_part_number: None,
                pre_erase_software_version: None,
                resume_counts: Vec::new(),
                attempt_key: None,
                transfer: None,
                last_intent: None,
                target_vin: None,
                intended_software_version: None,
            }
        ),
        // The newest VM state is the run start's, and it names no request.
        "run start" => {
            assert_eq!(state.last_vm_state, Some((at(0, 0), b"initial".to_vec())));
            assert_eq!(facts.last_step, None);
            assert_eq!(facts.transfer, None);
        }
        "pre-erase version" => {
            assert_eq!(state.last_vm_state, Some((at(0, 0), b"initial".to_vec())));
            assert_eq!(
                facts.ecu_hardware_part_number.as_deref(),
                Some(&b"NGR-SIM-ECU"[..])
            );
            assert_eq!(
                facts.pre_erase_software_version.as_deref(),
                Some(&b"1.0.0"[..])
            );
        }
        "resume" => assert_eq!(facts.resume_counts, [(STAGE, 1)]),
        // Before the transfer-start marker.
        "programming session" => {
            assert_eq!(facts.transfer, None);
            assert_eq!(facts.last_step, Some(at(2, 10)));
            // A later state replaces the run start's.
            assert_eq!(state.last_vm_state, Some((at(2, 10), b"vm".to_vec())));
        }
        // After it: the marker names the RequestDownload, which has not completed.
        "transfer-start marker" => {
            assert_eq!(facts.transfer, first_transfer(None, None));
            assert_eq!(facts.last_step, Some(at(2, 10)));
        }
        // Mid-transfer.
        "block 1" => assert_eq!(facts.transfer, first_transfer(Some(1), None)),
        // Before the RequestTransferExit marker.
        "block 2" => assert_eq!(facts.transfer, first_transfer(Some(2), None)),
        // After it, before the request completed.
        "RequestTransferExit marker" => {
            assert_eq!(facts.transfer, first_transfer(Some(2), exit(None, false)));
        }
        // Post-transfer progress.
        "CheckMemory" => assert_eq!(
            facts.transfer,
            first_transfer(Some(2), exit(Some(at(6, 21)), false))
        ),
        "post-transfer complete" => assert_eq!(
            facts.transfer,
            first_transfer(Some(2), exit(Some(at(6, 21)), true))
        ),
        // The resume closes the first attempt.
        "second resume" => {
            assert_eq!(
                facts.transfer,
                Some(TransferAttempt {
                    interrupted: true,
                    ..first_transfer(Some(2), exit(Some(at(6, 21)), true)).expect("some")
                })
            );
            assert_eq!(facts.resume_counts, [(STAGE, 2)]);
            assert_eq!(facts.attempt_key.as_deref(), Some(&b"attempt-2"[..]));
        }
        // The new marker cleared the first attempt's exit marker and post-transfer progress.
        "second transfer-start marker" => {
            assert_eq!(
                facts.transfer,
                Some(TransferAttempt {
                    stage: STAGE,
                    started_at: at(3, 30),
                    last_block: None,
                    exit: None,
                    interrupted: false,
                })
            );
            assert_eq!(facts.resume_counts, [(STAGE, 2)]);
            assert_eq!(facts.last_step, Some(at(6, 21)));
        }
        "second block 0" => assert_eq!(
            facts
                .transfer
                .as_ref()
                .and_then(|transfer| transfer.last_block),
            Some(0)
        ),
        _ => return None,
    }
    Some(name)
}

#[test]
fn the_journal_reads_back_after_a_crash_at_each_commit() {
    if let Ok(commits) = std::env::var(CRASH_ENV) {
        let commits = commits.parse().expect("a commit count");
        let dir = PathBuf::from(std::env::var_os(DIR_ENV).expect("the journal directory"));
        let journal = run_script(&dir, commits);
        println!("{CRASHING}");
        // Nothing is closed or flushed, as in a crash.
        std::mem::forget(journal);
        std::process::abort();
    }

    let mut checked = Vec::new();
    for commits in 0..=SCRIPT.len() {
        let crashed = temp_dir("crashed");
        let output = Command::new(std::env::current_exe().expect("test executable path"))
            .args([TEST_NAME, "--exact", "--nocapture"])
            .env(CRASH_ENV, commits.to_string())
            .env(DIR_ENV, &crashed.0)
            .output()
            .expect("the child process should start");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(CRASHING),
            "the child failed before its crash point after {commits} commits: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "the child should abort");

        let journal = Journal::open(&crashed.0, &key())
            .unwrap_or_else(|error| panic!("after {commits} commits: {error}"));
        // The same commits in this process, without a crash.
        let reference_dir = temp_dir("reference");
        let reference = run_script(&reference_dir.0, commits);
        assert_eq!(
            journal.state(),
            reference.state(),
            "after {commits} commits"
        );
        assert_eq!(journal.state().records, commits as u64);
        checked.extend(check_named_point(commits, journal.state()));
    }
    assert_eq!(checked, NAMED_POINTS, "every named point is reached");
}

/// A journal created with a target VIN and an intended software version holds both in its
/// creating write: they read back from the file, and a writer that opens it goes on after them
/// (ADR-268).
#[test]
fn the_creating_write_holds_the_vin_and_the_intended_version() {
    for (vin, version) in [
        (Some("WDB12345678901234"), Some(&b"2.0.0"[..])),
        (None, Some(&b"2.0.0"[..])),
        (Some("WDB12345678901234"), None),
    ] {
        let dir = temp_dir("creation-prefix");
        let vin = vin.map(|vin| Vin::new(vin.to_owned()));
        drop(Journal::create(&dir.0, &key(), vin.as_ref(), version).expect("create"));
        let state = Journal::read(&dir.0, &key()).expect("read");
        assert_eq!(state.facts.target_vin, vin);
        assert_eq!(state.facts.intended_software_version.as_deref(), version);
        assert_eq!(
            state.records,
            u64::from(vin.is_some()) + u64::from(version.is_some())
        );
        let mut journal = Journal::open(&dir.0, &key()).expect("open");
        journal
            .commit_ecu_hardware_part_number(b"NGR-SIM-ECU")
            .expect("commit after the prefix");
    }
}

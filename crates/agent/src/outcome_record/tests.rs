//! The outcome record's cases (ADR-511 item 7).

use std::fs;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use shared_proto::FailureCode;

use super::*;
use crate::outbox::Step;

const AGENT: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const OTHER_AGENT: &str = "0194f0a2-7c3e-7d11-8a52-000000000001";

fn key(n: u32, generation: u64) -> JobKey {
    JobKey {
        job_id: JobId(format!("0194f0a2-7c3e-7d11-8a52-{n:012x}")),
        generation,
    }
}

fn failed(detail: &str) -> JobState {
    JobState::Failed {
        code: FailureCode::InstructionRejected,
        detail: detail.to_owned(),
        procedure_code: None,
    }
}

fn file_of(dir: &Path) -> std::path::PathBuf {
    dir.join(format!("{AGENT}{FILE_SUFFIX}"))
}

/// A probe that fails at `step` while `armed` is set.
fn failing_at(step: Step, armed: &Arc<AtomicBool>) -> Probe {
    let armed = armed.clone();
    Box::new(move |at| {
        if at == step && armed.load(Ordering::SeqCst) {
            return Err(io::Error::other("a scripted failure"));
        }
        Ok(())
    })
}

/// What an end leaves behind survives a restart: a job that ended and was reported is not owed,
/// one that only ended is owed with the state it ended in, in the order the jobs ended.
#[test]
fn an_end_without_its_report_is_owed_after_a_reopen_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    assert!(record.owed().is_empty());
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    record.ended(&key(2, 1), &failed("two")).unwrap();
    record.ended(&key(3, 4), &JobState::Expired).unwrap();
    record.reported(&key(1, 1)).unwrap();
    drop(record);

    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    let owed = record.owed();
    assert_eq!(
        owed,
        vec![(key(2, 1), failed("two")), (key(3, 4), JobState::Expired)]
    );
    assert_eq!(
        record.standing(&key(1, 1)),
        Some((JobState::Completed, true))
    );
    record.reported(&key(2, 1)).unwrap();
    assert_eq!(record.owed(), vec![(key(3, 4), JobState::Expired)]);
}

/// A generation is a job of its own: ending generation 1 says nothing of generation 2.
#[test]
fn a_generation_is_its_own_job() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    assert_eq!(record.standing(&key(1, 2)), None);
}

/// The first end stands: a second `ended` for the job changes nothing, and neither does a second
/// `reported` or one for a job that never ended.
#[test]
fn the_first_end_stands_and_repeats_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    record.ended(&key(1, 1), &failed("late")).unwrap();
    record.reported(&key(1, 1)).unwrap();
    let len = fs::metadata(file_of(dir.path())).unwrap().len();
    record.reported(&key(1, 1)).unwrap();
    record.reported(&key(9, 1)).unwrap();
    assert_eq!(fs::metadata(file_of(dir.path())).unwrap().len(), len);
    assert_eq!(
        record.standing(&key(1, 1)),
        Some((JobState::Completed, true))
    );
    assert_eq!(record.standing(&key(9, 1)), None);
}

/// A write that fails poisons the record, and the end it carried is not counted; once the
/// failure is over a reopen reads what reached the file, so an `ended` whose sync failed after
/// the write counts and is owed.
#[test]
fn a_failed_write_poisons_and_a_reopen_counts_what_reached_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let record =
        OutcomeRecord::open_with(dir.path(), AGENT, failing_at(Step::AfterWrite, &armed)).unwrap();
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    armed.store(true, Ordering::SeqCst);
    assert!(record.ended(&key(2, 1), &failed("two")).is_err());
    assert!(record.is_poisoned());
    assert!(matches!(
        record.reported(&key(1, 1)),
        Err(RecordError::Poisoned)
    ));
    assert_eq!(record.standing(&key(2, 1)), None, "not counted in memory");

    armed.store(false, Ordering::SeqCst);
    assert!(record.reopen_if_poisoned().unwrap());
    assert!(!record.is_poisoned());
    // The frame reached the file before the failure, so it counts at the reopen.
    assert_eq!(record.standing(&key(2, 1)), Some((failed("two"), false)));
    assert_eq!(record.owed().len(), 2);
    assert!(!record.reopen_if_poisoned().unwrap());
}

/// A last frame a crash tore is cut off: the end it carried is not in the record, and the
/// records before it are.
#[test]
fn a_torn_last_frame_is_cut_off() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    let whole = fs::metadata(file_of(dir.path())).unwrap().len();
    record.ended(&key(2, 1), &failed("two")).unwrap();
    drop(record);
    let torn = fs::metadata(file_of(dir.path())).unwrap().len() - 3;
    fs::OpenOptions::new()
        .write(true)
        .open(file_of(dir.path()))
        .unwrap()
        .set_len(torn)
        .unwrap();

    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    assert_eq!(record.owed(), vec![(key(1, 1), JobState::Completed)]);
    assert_eq!(fs::metadata(file_of(dir.path())).unwrap().len(), whole);
    record.ended(&key(2, 1), &failed("again")).unwrap();
    assert_eq!(record.standing(&key(2, 1)), Some((failed("again"), false)));
}

/// A file that is not the writer's is refused and left as it is: another agent's file, a report
/// before its end, and a second holder of the lock.
#[test]
fn a_file_the_writer_did_not_make_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    record.ended(&key(1, 1), &JobState::Completed).unwrap();
    assert!(matches!(
        OutcomeRecord::open_in(dir.path(), AGENT),
        Err(RecordError::InUse)
    ));
    drop(record);
    let moved = dir.path().join(format!("{OTHER_AGENT}{FILE_SUFFIX}"));
    fs::copy(file_of(dir.path()), &moved).unwrap();
    assert!(matches!(
        OutcomeRecord::open_in(dir.path(), OTHER_AGENT),
        Err(RecordError::Corrupt(..))
    ));

    // A report whose end is not there.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    crate::frame_file::header_frame(AGENT, &mut bytes);
    let entry = Entry {
        seq: 0,
        job_id: key(5, 1).job_id.0,
        generation: 1,
        what: What::Reported,
    };
    crate::journal::push_frame(&mut bytes, &postcard::to_allocvec(&entry).unwrap()).unwrap();
    fs::write(file_of(dir.path()), bytes).unwrap();
    assert!(matches!(
        OutcomeRecord::open_in(dir.path(), AGENT),
        Err(RecordError::Corrupt(..))
    ));
}

/// A job ID that is not a canonical UUID names no file and is refused before anything is written.
#[test]
fn a_job_id_that_is_not_a_uuid_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let record = OutcomeRecord::open_in(dir.path(), AGENT).unwrap();
    let bad = JobKey {
        job_id: JobId("../x".to_owned()),
        generation: 1,
    };
    assert!(matches!(
        record.ended(&bad, &JobState::Completed),
        Err(RecordError::Refused)
    ));
    assert!(!file_of(dir.path()).exists());
}

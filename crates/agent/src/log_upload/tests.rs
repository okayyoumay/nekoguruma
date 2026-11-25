use sha2::{Digest as _, Sha256};
use shared_proto::JobId;

use std::io::Write;
use std::sync::atomic::AtomicBool;

use super::*;
use crate::comm_log::{CommLog, CommLogSetup};
use crate::upload_ledger::{Kind, Probe, Source, Step, UploadLedger};

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";
const GENERATION: u64 = 2;

struct Rig {
    dir: tempfile::TempDir,
    received: ReceivedJobs,
    uploads: SharedLedger,
    uploader: LogUploader,
}

impl Rig {
    fn new() -> Rig {
        Rig::with_probe(Box::new(|_| Ok(())))
    }

    fn with_probe(probe: Probe) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("journal");
        for sub in ["journal", "received", "uploads"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let received = ReceivedJobs::open_in(&dir.path().join("received"), AGENT_ID).unwrap();
        let uploads = SharedLedger::new(
            UploadLedger::open_with(&dir.path().join("uploads"), AGENT_ID, probe).unwrap(),
        );
        let uploader = LogUploader::in_dir(logs, received.clone(), uploads.clone());
        Rig {
            dir,
            received,
            uploads,
            uploader,
        }
    }

    fn take(&self, job: &str, generation: u64) {
        self.received.record(job, generation, b"envelope").unwrap();
    }

    fn key(&self) -> JobKey {
        JobKey {
            job_id: JobId(JOB.to_owned()),
            generation: GENERATION,
        }
    }

    fn log_path(&self) -> PathBuf {
        comm_log_path(&self.dir.path().join("journal"), &self.key()).unwrap()
    }

    /// A job the agent holds at the test's generation, with `bytes` as its log.
    fn job_with_log(&self, bytes: &[u8]) {
        self.take(JOB, GENERATION);
        std::fs::write(self.log_path(), bytes).unwrap();
    }

    async fn ask(&self) -> Result<LogUploadOutcome, LogUploadError> {
        self.uploader
            .handle(JOB.to_owned(), GENERATION, not_cancelled())
            .await
    }

    /// The log's writer, as a running job holds it.
    fn writer(&self) -> CommLog {
        let setup = CommLogSetup::for_run_key(self.dir.path().join("journal"), self.key()).unwrap();
        CommLog::open(&setup).unwrap()
    }
}

fn not_cancelled() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[tokio::test]
async fn a_job_the_agent_does_not_hold_is_unknown_whatever_its_log() {
    let rig = Rig::new();
    std::fs::write(rig.log_path(), b"a\n").unwrap();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::UnknownJob);
    // Not a canonical UUID: never looked up as a file name.
    for job in ["../x", "not-a-uuid", ""] {
        let outcome = rig
            .uploader
            .handle(job.to_owned(), 1, not_cancelled())
            .await
            .unwrap();
        assert_eq!(outcome, LogUploadOutcome::UnknownJob, "{job:?}");
    }
    assert!(rig.uploads.entries_for_test().await.is_empty());
}

#[tokio::test]
async fn an_earlier_or_later_generation_than_the_held_one_is_unknown() {
    let rig = Rig::new();
    rig.job_with_log(b"a\n");
    for generation in [GENERATION - 1, GENERATION + 1] {
        let outcome = rig
            .uploader
            .handle(JOB.to_owned(), generation, not_cancelled())
            .await
            .unwrap();
        assert_eq!(outcome, LogUploadOutcome::UnknownJob, "{generation}");
    }
    // A job taken again at a higher generation is held at that one only.
    rig.take(JOB, GENERATION + 1);
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::UnknownJob);
    assert!(rig.uploads.entries_for_test().await.is_empty());
}

#[tokio::test]
async fn a_log_that_is_missing_empty_or_without_a_complete_line_is_no_log() {
    let rig = Rig::new();
    rig.take(JOB, GENERATION);
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::NoLog, "missing");
    // No lock file was made for a job that has no log.
    assert_eq!(
        rig.dir.path().join("journal").read_dir().unwrap().count(),
        0
    );
    std::fs::write(rig.log_path(), b"").unwrap();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::NoLog, "empty");
    std::fs::write(rig.log_path(), b"{\"torn\":").unwrap();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::NoLog, "torn");
    assert!(rig.uploads.entries_for_test().await.is_empty());
}

#[tokio::test]
async fn a_log_is_queued_up_to_its_last_line_end_with_its_own_length_and_digest() {
    let rig = Rig::new();
    // The last line is torn: it is left out of the entry.
    rig.job_with_log(b"{\"seq\":1}\n{\"seq\":2}\n{\"seq\":3");
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    let entries = rig.uploads.entries_for_test().await;
    assert_eq!(entries.len(), 1);
    let whole = b"{\"seq\":1}\n{\"seq\":2}\n";
    assert_eq!(entries[0].key, rig.key());
    assert_eq!(entries[0].kind, Kind::CommLog);
    assert_eq!(entries[0].length, whole.len() as u64);
    assert_eq!(entries[0].sha256, sha(whole));
    assert_eq!(
        entries[0].source,
        Source::File(rig.log_path().to_str().unwrap().to_owned())
    );
    // The writer lock was released: a run can take the log now.
    drop(rig.writer());
}

#[tokio::test]
async fn a_log_that_ends_on_a_line_end_is_queued_whole() {
    let rig = Rig::new();
    // Longer than the read chunk, so the last line end is found by reading back more than once.
    let log: Vec<u8> = (0..20_000)
        .flat_map(|n| format!("{{\"seq\":{n}}}\n").into_bytes())
        .collect();
    rig.job_with_log(&log);
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    let entries = rig.uploads.entries_for_test().await;
    assert_eq!(
        (entries[0].length, entries[0].sha256),
        (log.len() as u64, sha(&log))
    );
    // A single line longer than the chunk, then a torn tail longer than the chunk.
    let rig = Rig::new();
    let mut log = vec![b'x'; 200_000];
    log.push(b'\n');
    let whole = log.len();
    log.extend(vec![b'y'; 200_000]);
    rig.job_with_log(&log);
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    let entries = rig.uploads.entries_for_test().await;
    assert_eq!(entries[0].length, whole as u64);
    assert_eq!(entries[0].sha256, sha(&log[..whole]));
}

#[tokio::test]
async fn a_log_a_run_holds_is_busy_and_is_queued_once_the_run_lets_go() {
    let rig = Rig::new();
    rig.take(JOB, GENERATION);
    let writer = rig.writer();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Busy);
    assert!(rig.uploads.entries_for_test().await.is_empty());
    // The run writes a line and ends.
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.log_path())
        .unwrap()
        .write_all(b"a\n")
        .unwrap();
    drop(writer);
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    assert_eq!(rig.uploads.entries_for_test().await.len(), 1);
}

#[tokio::test]
async fn a_queued_log_is_answered_queued_without_reading_it_again_or_trying_the_lock() {
    let rig = Rig::new();
    rig.job_with_log(b"a\nb\n");
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    let first = rig.uploads.entries_for_test().await;
    // A run takes the log and appends: the entry keeps the bytes it covered, the repeated
    // command is answered from the ledger and is not `busy`, and nothing is hashed again.
    let writer = rig.writer();
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.log_path())
        .unwrap()
        .write_all(b"c\n")
        .unwrap();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    assert_eq!(rig.uploads.entries_for_test().await, first);
    drop(writer);
    // Not even a log that is gone changes the answer.
    std::fs::remove_file(rig.log_path()).unwrap();
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
    assert_eq!(rig.uploads.entries_for_test().await, first);
}

#[tokio::test]
async fn a_log_over_the_maximum_is_too_large_and_one_at_it_is_queued() {
    let rig = Rig::new();
    let uploader = LogUploader::in_dir(
        rig.dir.path().join("journal"),
        rig.received.clone(),
        rig.uploads.clone(),
    )
    .with_max_bytes(8);
    rig.take(JOB, GENERATION);
    // Ten bytes of complete lines against a maximum of eight; the torn tail does not count.
    std::fs::write(rig.log_path(), b"aaaa\nbbbb\ncc").unwrap();
    let ask = || uploader.handle(JOB.to_owned(), GENERATION, not_cancelled());
    assert_eq!(ask().await.unwrap(), LogUploadOutcome::TooLarge);
    assert!(rig.uploads.entries_for_test().await.is_empty());
    std::fs::write(rig.log_path(), b"aaa\nbbb\ncc").unwrap();
    assert_eq!(ask().await.unwrap(), LogUploadOutcome::Queued);
    assert_eq!(rig.uploads.entries_for_test().await[0].length, 8);
}

#[tokio::test]
async fn a_read_whose_connection_ended_enqueues_nothing_and_frees_the_lock() {
    let rig = Rig::new();
    rig.job_with_log(b"a\nb\n");
    let cancel = Arc::new(AtomicBool::new(true));
    let outcome = rig
        .uploader
        .handle(JOB.to_owned(), GENERATION, cancel)
        .await;
    assert!(
        matches!(outcome, Err(LogUploadError::Cancelled)),
        "{outcome:?}"
    );
    assert!(rig.uploads.entries_for_test().await.is_empty());
    drop(rig.writer());
    assert_eq!(rig.ask().await.unwrap(), LogUploadOutcome::Queued);
}

#[tokio::test]
async fn a_ledger_that_cannot_make_the_entry_durable_gives_an_error_and_no_entry() {
    let rig = Rig::with_probe(Box::new(|step| match step {
        Step::BeforeWrite => Err(io::Error::other("a scripted write failure")),
        _ => Ok(()),
    }));
    rig.job_with_log(b"a\n");
    let error = rig.ask().await.unwrap_err();
    assert!(matches!(error, LogUploadError::Ledger(_)), "{error}");
    assert!(rig.uploads.entries_for_test().await.is_empty());
    // The failure left the lock free: the command can be asked again.
    drop(rig.writer());
}

#[tokio::test]
async fn a_directory_that_cannot_be_resolved_gives_an_error() {
    let rig = Rig::new();
    let uploader = LogUploader::with(
        Arc::new(|| Err(io::Error::other("no journal directory"))),
        rig.received.clone(),
        rig.uploads.clone(),
    );
    rig.take(JOB, GENERATION);
    let outcome = uploader
        .handle(JOB.to_owned(), GENERATION, not_cancelled())
        .await;
    assert!(matches!(outcome, Err(LogUploadError::Io(_))));
}

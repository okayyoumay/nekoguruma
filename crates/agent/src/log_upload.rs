//! What the agent does with a `logUploadCommand` (design 5.2, 8.11, ADR-508 items 4 and 5).
//!
//! An operator's request for a job's communication log reaches the agent as a control message
//! that names a job and an ownership generation. The agent never takes a path from it: the log
//! is the file [`comm_log_path`] names in the journal directory of its mode. [`LogUploader`]
//! answers, in this order:
//!
//! 1. `unknownJob` unless the job ID is a canonical UUID and the received-jobs ledger's highest
//!    generation for it is the one asked for (an earlier or later generation is as unknown as a
//!    job the agent never took);
//! 2. `queued` when the upload ledger already holds an entry for the log, without reading the
//!    file and before the writer lock is tried, so a repeated command never answers `busy` for
//!    a log that is already queued;
//! 3. `noLog` when there is no log file, before the lock is touched, so no lock file is made
//!    for a job without a log;
//! 4. `busy` when a run holds the log's writer lock, which is tried once;
//! 5. `noLog` when the file has no complete line, and `tooLarge` when its complete lines are over
//!    the maximum;
//! 6. otherwise the bytes up to the last line end are hashed under the lock and enqueued, and
//!    the answer is `queued` once the entry is durable.
//!
//! A failure (a journal directory or a log that cannot be read, a ledger that cannot take the
//! entry because it is full or refuses the source, the work cancelled because its connection
//! ended) is a [`LogUploadError`]: the control channel sends no answer and the server asks again
//! until the request expires.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use shared_proto::{LogUploadOutcome, is_canonical_uuid};
use worker_host::service::OperatingMode;

use crate::comm_log::{UploadRead, UploadStage, comm_log_path, digest_for_upload_traced};
use crate::journal::{JobKey, journal_dir};
use crate::received_jobs::ReceivedJobs;
use crate::upload_ledger::{LedgerError, MAX_COMM_LOG_BYTES};
use crate::uploader::SharedLedger;

/// Why a command was left without an answer. The cause is for the log; nothing of the command
/// is in it.
#[derive(Debug, thiserror::Error)]
pub enum LogUploadError {
    /// The journal directory, the log or its lock could not be read.
    #[error("the log could not be read: {0}")]
    Io(#[from] io::Error),
    /// The upload ledger could not take the entry.
    #[error("the upload ledger could not take the entry: {0}")]
    Ledger(#[from] LedgerError),
    /// The task that does the work ended without a result.
    #[error("the task for the log ended without a result")]
    Task,
    /// The connection the command came on ended while the log was being read.
    #[error("the connection ended while the log was being read")]
    Cancelled,
}

/// What [`LogUploads::handle`] returns: a future the control channel polls beside its frames.
pub type Handled<'a> =
    Pin<Box<dyn Future<Output = Result<LogUploadOutcome, LogUploadError>> + Send + 'a>>;

/// Where a `logUploadCommand` goes. The control channel runs the returned future beside its
/// reads and heartbeats and writes the answer when it is done, so a call may take as long as
/// hashing a large log; it must tolerate being dropped before it finishes, as a closed
/// connection does. `cancel` is set when the connection ends: work that goes on after its future
/// is dropped (a blocking read) stops at its next chunk and enqueues nothing.
pub trait LogUploads: Send + Sync {
    /// Judges the request for the log of `job_id` at `generation`.
    fn handle(&self, job_id: String, generation: u64, cancel: Arc<AtomicBool>) -> Handled<'_>;
}

/// The default, connected to no job store: every command is for an unknown job.
#[derive(Debug)]
pub struct NoLogUploads;

impl LogUploads for NoLogUploads {
    fn handle(&self, _job_id: String, _generation: u64, _cancel: Arc<AtomicBool>) -> Handled<'_> {
        Box::pin(std::future::ready(Ok(LogUploadOutcome::UnknownJob)))
    }
}

/// Where the logs are.
type LogDir = Arc<dyn Fn() -> io::Result<PathBuf> + Send + Sync>;

/// The agent's own handling: its received jobs, its upload ledger and its journal directory.
pub struct LogUploader {
    dir: LogDir,
    received: ReceivedJobs,
    uploads: SharedLedger,
    /// The longest log that is sent: the largest the upload ledger takes, which tests lower.
    max_bytes: u64,
    /// Called at the stages of a read, which tests hold.
    trace: Arc<dyn Fn(UploadStage) + Send + Sync>,
}

impl std::fmt::Debug for LogUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogUploader").finish_non_exhaustive()
    }
}

impl LogUploader {
    /// The handling for the journal directory of `mode`, resolved at each command (a failure to
    /// resolve it is a command left unanswered).
    pub fn new(mode: OperatingMode, received: ReceivedJobs, uploads: SharedLedger) -> Self {
        let dir = Arc::new(move || journal_dir(mode).map_err(io::Error::other));
        Self::with(dir, received, uploads)
    }

    /// The handling for logs in `dir`, which must be an absolute path.
    pub fn in_dir(dir: PathBuf, received: ReceivedJobs, uploads: SharedLedger) -> Self {
        Self::with(Arc::new(move || Ok(dir.clone())), received, uploads)
    }

    fn with(dir: LogDir, received: ReceivedJobs, uploads: SharedLedger) -> Self {
        Self {
            dir,
            received,
            uploads,
            max_bytes: MAX_COMM_LOG_BYTES,
            trace: Arc::new(|_| {}),
        }
    }

    /// The handling with `max_bytes` as the longest log sent.
    #[cfg(test)]
    pub(crate) fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// The handling that calls `trace` at the stages of each read.
    #[cfg(test)]
    pub(crate) fn with_trace(mut self, trace: Arc<dyn Fn(UploadStage) + Send + Sync>) -> Self {
        self.trace = trace;
        self
    }

    async fn run(
        &self,
        job_id: String,
        generation: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<LogUploadOutcome, LogUploadError> {
        if !is_canonical_uuid(&job_id) || self.received.highest(&job_id) != Some(generation) {
            return Ok(LogUploadOutcome::UnknownJob);
        }
        let key = JobKey {
            job_id: shared_proto::JobId(job_id),
            generation,
        };
        if self.uploads.has_comm_log(&key).await {
            return Ok(LogUploadOutcome::Queued);
        }
        let (dir, read_key) = (self.dir.clone(), key.clone());
        let (max, trace) = (self.max_bytes, self.trace.clone());
        let found = tokio::task::spawn_blocking(move || {
            let dir = dir()?;
            digest_for_upload_traced(&dir, &read_key, max, &cancel, &*trace).map(|read| (read, dir))
        })
        .await
        .map_err(|_| LogUploadError::Task)??;
        let (digest, dir) = match found {
            (UploadRead::Busy, _) => return Ok(LogUploadOutcome::Busy),
            (UploadRead::NoLog, _) => return Ok(LogUploadOutcome::NoLog),
            (UploadRead::TooLarge, _) => return Ok(LogUploadOutcome::TooLarge),
            (UploadRead::Cancelled, _) => return Err(LogUploadError::Cancelled),
            (UploadRead::Ready(digest), dir) => (digest, dir),
        };
        let path = comm_log_path(&dir, &key).map_err(io::Error::other)?;
        match self.uploads.enqueue_comm_log(key, path, digest).await {
            // An entry of the same job and generation is there already, with other bytes:
            // another command queued it first.
            Ok(_) | Err(LedgerError::Conflict) => Ok(LogUploadOutcome::Queued),
            Err(LedgerError::TooLarge) => Ok(LogUploadOutcome::TooLarge),
            Err(error) => Err(error.into()),
        }
    }
}

impl LogUploads for LogUploader {
    fn handle(&self, job_id: String, generation: u64, cancel: Arc<AtomicBool>) -> Handled<'_> {
        Box::pin(self.run(job_id, generation, cancel))
    }
}

#[cfg(test)]
mod tests;

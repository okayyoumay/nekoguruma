//! What the agent does with a `logUploadCommand` (design 5.2, 8.11, ADR-508 items 4 and 5).
//!
//! An operator's request for a job's communication log reaches the agent as a control message
//! that names a job and an ownership generation. The agent never takes a path from it: the log
//! is the file [`comm_log_path`] names in the journal directory of its mode. [`LogUploader`]
//! answers, in this order:
//!
//! 1. `unknownJob` unless the job ID is a canonical UUID and the received-jobs ledger's highest
//!    generation for it is the one asked for (an earlier generation is as unknown as a job the
//!    agent never took);
//! 2. `queued` when the upload ledger already holds an entry for the log, without reading the
//!    file and before the writer lock is tried, so a repeated command never answers `busy`
//!    for a log that is already queued;
//! 3. `busy`, `noLog` or `tooLarge` as [`digest_for_upload`] finds the log (the lock is tried
//!    once);
//! 4. `queued` once the entry that covers the log's bytes up to its last line end is durable.
//!
//! A ledger or file-system failure is an [`LogUploadError`]: the control channel sends no answer
//! and the server asks again.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use shared_proto::{LogUploadOutcome, is_canonical_uuid};
use worker_host::service::OperatingMode;

use crate::comm_log::{UploadRead, comm_log_path, digest_for_upload};
use crate::journal::{JobKey, journal_dir};
use crate::received_jobs::ReceivedJobs;
use crate::upload_ledger::LedgerError;
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
}

/// What [`LogUploads::handle`] returns: a future the control channel polls beside its frames.
pub type Handled<'a> =
    Pin<Box<dyn Future<Output = Result<LogUploadOutcome, LogUploadError>> + Send + 'a>>;

/// Where a `logUploadCommand` goes. The control channel runs the returned future beside its
/// reads and heartbeats and writes the answer when it is done, so a call may take as long as
/// hashing a large log; it must tolerate being dropped before it finishes, as a closed
/// connection does.
pub trait LogUploads: Send + Sync {
    /// Judges the request for the log of `job_id` at `generation`.
    fn handle(&self, job_id: String, generation: u64) -> Handled<'_>;
}

/// The default, connected to no job store: every command is for an unknown job.
#[derive(Debug)]
pub struct NoLogUploads;

impl LogUploads for NoLogUploads {
    fn handle(&self, _job_id: String, _generation: u64) -> Handled<'_> {
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
        Self {
            dir,
            received,
            uploads,
        }
    }

    /// The handling for logs in `dir`, which must be an absolute path.
    pub fn in_dir(dir: PathBuf, received: ReceivedJobs, uploads: SharedLedger) -> Self {
        Self {
            dir: Arc::new(move || Ok(dir.clone())),
            received,
            uploads,
        }
    }

    async fn run(
        &self,
        job_id: String,
        generation: u64,
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
        let dir = self.dir.clone();
        let read_key = key.clone();
        let found = tokio::task::spawn_blocking(move || {
            let dir = dir()?;
            digest_for_upload(&dir, &read_key).map(|read| (read, dir))
        })
        .await
        .map_err(|_| LogUploadError::Task)??;
        let (digest, dir) = match found {
            (UploadRead::Busy, _) => return Ok(LogUploadOutcome::Busy),
            (UploadRead::NoLog, _) => return Ok(LogUploadOutcome::NoLog),
            (UploadRead::TooLarge, _) => return Ok(LogUploadOutcome::TooLarge),
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
    fn handle(&self, job_id: String, generation: u64) -> Handled<'_> {
        Box::pin(self.run(job_id, generation))
    }
}

#[cfg(test)]
mod tests;

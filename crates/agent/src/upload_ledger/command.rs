//! `ngr-agent debug enqueue`: puts one object into the agent's upload ledger, so that a test can
//! drive the uploader end to end (design 13.4, ADR-493 item 10). It exists in a debug build only:
//! this module is not compiled in a release build, and the binary's parser then does not know the
//! command.
//!
//! The ledger has one writer (its lock, [`LedgerError::InUse`]), so the command cannot add to the
//! ledger of a running `ngr-agent connect`: it runs before `connect` starts, or between two runs,
//! and `connect` finds the entries when it opens the ledger. The ledger's enqueue wakes the
//! uploader in the same process only, and there is no cross-process wake.
//!
//! It reads the registration record for the agent ID (the ledger is the agent's), as
//! `ngr-agent debug fetch` does, and needs no key and no connection. It prints the entry's number
//! as one JSON line.

use std::path::PathBuf;

use shared_proto::JobId;
use worker_host::service::OperatingMode;

use super::{Kind, LedgerError, MAX_RESULT_BYTES, UploadLedger, digest_file};
use crate::journal::JobKey;
use crate::registration::origin::ServerOrigin;

/// What the command takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub mode: OperatingMode,
    pub job_id: String,
    pub generation: u64,
    pub kind: Kind,
    /// A result: the file whose bytes are the result, which the ledger copies. A communication
    /// log: the log file, which stays where it is, and whose whole current length is enqueued.
    pub file: PathBuf,
}

pub use crate::fetch::command::CommandError;
use crate::fetch::command::{fail, installed_record};

/// Enqueues the object and returns its entry's number. Exit 1: it cannot as asked (not
/// registered, a record for another server, a job ID that is not a canonical UUID, a generation
/// of 0, a file that cannot be read, a source or a length the ledger refuses, another upload of
/// the same job, generation and kind with other bytes, a corrupt ledger); 3: it may work later
/// (the ledger is open in another process, the state directory or a write is unusable). The
/// file is read, or hashed, before the ledger is opened, so its lock is held for the enqueue
/// alone.
pub fn run(args: Args, installed: &ServerOrigin) -> Result<u64, CommandError> {
    // A job ID that no begin would accept, or generation 0 (the first is 1), would be passed over
    // at every pass for as long as it stayed in the ledger.
    if !shared_proto::is_canonical_uuid(&args.job_id) {
        return Err(fail(1, "the job ID is not a canonical UUID"));
    }
    if args.generation == 0 {
        return Err(fail(1, "the generation starts at 1"));
    }
    let record = installed_record(args.mode, installed)?;
    let key = JobKey {
        job_id: JobId(args.job_id),
        generation: args.generation,
    };
    let unreadable =
        |error: std::io::Error| fail(1, format!("the file cannot be read: {:?}", error.kind()));
    enum Source {
        Bytes(Vec<u8>),
        Log(PathBuf, super::Digest),
    }
    let source = match args.kind {
        Kind::Result => {
            // The length first: a file over the bound is not read whole.
            let length = std::fs::metadata(&args.file).map_err(unreadable)?.len();
            if length > MAX_RESULT_BYTES {
                return Err(fail(1, LedgerError::TooLarge.to_string()));
            }
            Source::Bytes(std::fs::read(&args.file).map_err(unreadable)?)
        }
        Kind::CommLog => {
            let path = std::path::absolute(&args.file).map_err(|error| {
                fail(
                    1,
                    format!("the file's path is unusable: {:?}", error.kind()),
                )
            })?;
            let digest = digest_file(&path).map_err(unreadable)?;
            Source::Log(path, digest)
        }
    };
    let mut ledger =
        UploadLedger::open(args.mode, &record.agent_id).map_err(|error| ledger_failure(&error))?;
    match source {
        Source::Bytes(bytes) => ledger.enqueue_result(&key, &bytes),
        Source::Log(path, digest) => ledger.enqueue_comm_log(&key, &path, digest),
    }
    .map_err(|error| ledger_failure(&error))
}

fn ledger_failure(error: &LedgerError) -> CommandError {
    let exit = match error {
        LedgerError::InUse | LedgerError::State(_) | LedgerError::Io(_) | LedgerError::Poisoned => {
            3
        }
        _ => 1,
    };
    fail(exit, error.to_string())
}

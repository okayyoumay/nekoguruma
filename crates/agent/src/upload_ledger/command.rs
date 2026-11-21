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

use super::{Kind, LedgerError, UploadLedger, digest_file};
use crate::journal::JobKey;
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{self, Record};
use crate::state;

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

/// Why the command failed: the exit status and a fixed text.
#[derive(Debug)]
pub struct CommandError {
    pub exit: u8,
    pub message: String,
}

fn fail(exit: u8, message: impl Into<String>) -> CommandError {
    CommandError {
        exit,
        message: message.into(),
    }
}

/// Enqueues the object and returns its entry's number. Exit 1: it cannot as asked (not
/// registered, a record for another server, a file that cannot be read, a job ID, a source or a
/// length the ledger refuses, another upload of the same job, generation and kind with other
/// bytes, a corrupt ledger); 3: it may work later (the ledger is open in another process, the
/// state directory or a write is unusable).
pub fn run(args: Args, installed: &ServerOrigin) -> Result<u64, CommandError> {
    let dir = state::private_subdir(args.mode, record::DIR_NAME)
        .map_err(|error| fail(3, format!("the state directory is unusable: {error}")))?;
    let record = Record::read(&dir.join(record::RECORD_FILE)).map_err(|_| {
        fail(
            1,
            "not registered, or the registration record cannot be used",
        )
    })?;
    if record.server_origin != installed.as_str() {
        return Err(fail(
            1,
            "the registration record names another server than the installation file",
        ));
    }
    let key = JobKey {
        job_id: JobId(args.job_id),
        generation: args.generation,
    };
    let mut ledger = UploadLedger::open(args.mode, &record.agent_id).map_err(ledger_failure)?;
    let seq = match args.kind {
        Kind::Result => {
            let bytes = std::fs::read(&args.file)
                .map_err(|error| fail(1, format!("the file cannot be read: {:?}", error.kind())))?;
            ledger.enqueue_result(&key, &bytes)
        }
        Kind::CommLog => {
            let path = std::path::absolute(&args.file).map_err(|error| {
                fail(
                    1,
                    format!("the file's path is unusable: {:?}", error.kind()),
                )
            })?;
            let digest = digest_file(&path)
                .map_err(|error| fail(1, format!("the file cannot be read: {:?}", error.kind())))?;
            ledger.enqueue_comm_log(&key, &path, digest)
        }
    }
    .map_err(ledger_failure)?;
    Ok(seq)
}

fn ledger_failure(error: LedgerError) -> CommandError {
    let exit = match error {
        LedgerError::InUse | LedgerError::State(_) | LedgerError::Io(_) | LedgerError::Poisoned => {
            3
        }
        _ => 1,
    };
    fail(exit, error.to_string())
}

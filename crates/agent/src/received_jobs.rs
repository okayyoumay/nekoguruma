//! The received-jobs ledger: which job commands the agent has already taken (design 5.3, ADR-489;
//! ADR-229 for the ownership generation, ADR-461 Decision item 9 for the signed envelope).
//!
//! One file per agent ID, `<state>/registration/<agent-id>.received-jobs.v1`, holds one record for
//! every job the agent accepted: the job ID, the ownership generation the instruction carried and
//! the signed envelope byte for byte (nothing re-encodes it). The control channel asks
//! [`ReceivedJobs::highest`] before it takes a command: a job ID recorded under the same or a
//! later generation is a duplicate, acknowledged and not taken again; one with no record, or only
//! a record of a lower generation, is new, and [`ReceivedJobs::record`] makes its record durable
//! before the intake, the `received` event and the acknowledgement. A higher generation adds a
//! second record; the earlier one stays. The file is named after the agent ID, so a
//! re-registration (a new agent ID, ADR-461 Decision item 8) starts with an empty ledger and
//! leaves the old file alone. Nothing is ever removed from it.
//!
//! The file's layout, its open, its torn tail and its poisoning after a failed write are
//! [`crate::frame_file`]'s. Each later frame is one record, with consecutive numbers from 0. The
//! open reads the whole file once and keeps only an index from job ID to the highest generation.
//! A ledger that does not read back is [`LedgerError::Corrupt`] and is never repaired. After a
//! failed write the ledger refuses records until it is opened again
//! ([`ReceivedJobs::reopen_if_poisoned`]). [`ReceivedJobs::highest`] reads memory only, so a
//! poisoned ledger still recognises its duplicates.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use shared_crypto::instruction::{HEADER_LEN, MAX_PAYLOAD_LEN};
use worker_host::service::OperatingMode;

use crate::frame_file::{FileError, FrameFile, Layout};
use crate::journal::{FRAME_HEADER, JournalError};
use crate::outbox::Probe;
use crate::registration::record;
use crate::state::{StateDirError, private_subdir};

/// The ledger format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The longest envelope a record holds: the envelope's fixed part and the longest payload.
pub const MAX_ENVELOPE: usize = HEADER_LEN + MAX_PAYLOAD_LEN;

const MAGIC: [u8; 8] = *b"NGRRJOB\0";
/// Magic and format version.
#[cfg(test)]
const PREAMBLE: usize = MAGIC.len() + 4;
/// The ledger file's name ends with this; the agent ID comes first.
const FILE_SUFFIX: &str = ".received-jobs.v1";
/// The lock file: held while the store is open, and never deleted.
const LOCK_FILE: &str = "received-jobs.lock";
/// The room a record's number, job ID, generation and lengths take beyond the envelope.
const RECORD_OVERHEAD: usize = 128;
/// The longest a frame that a crash left unfinished can be.
const MAX_TORN: usize = FRAME_HEADER + MAX_ENVELOPE + RECORD_OVERHEAD;

const LAYOUT: Layout = Layout {
    magic: MAGIC,
    version: FORMAT_VERSION,
    suffix: FILE_SUFFIX,
    lock_file: LOCK_FILE,
    max_torn: MAX_TORN,
    what: "received-jobs ledger",
};

/// Why the ledger cannot be opened or written. The texts name no path and no job.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("the received-jobs ledger cannot be read or written: {0:?}")]
    Io(io::ErrorKind),
    #[error("the state directory: {0}")]
    State(#[from] StateDirError),
    /// The file is not what the writer makes (never repaired).
    /// The second field is the decoder's error when a record did not decode.
    #[error("the received-jobs ledger file is corrupt: {0}")]
    Corrupt(&'static str, #[source] Option<postcard::Error>),
    /// Another store of this directory is open, in this process or another.
    #[error("the received-jobs ledger is open for writing elsewhere")]
    InUse,
    #[error("the agent ID cannot name the received-jobs ledger file")]
    BadAgentId,
    /// A write failed (in this call or an earlier one); open the ledger again.
    #[error("a write to the received-jobs ledger failed")]
    Poisoned,
    /// The job ID is not a canonical UUID, or the envelope is empty or over [`MAX_ENVELOPE`]:
    /// nothing is written.
    #[error("the record is not one the ledger holds")]
    Refused,
    /// The record could not be encoded or framed; the source says why.
    #[error("the record is not one the ledger holds")]
    Unencodable(#[source] JournalError),
}

impl From<io::Error> for LedgerError {
    fn from(error: io::Error) -> Self {
        LedgerError::Io(error.kind())
    }
}

impl From<FileError> for LedgerError {
    fn from(error: FileError) -> Self {
        match error {
            FileError::Io(kind) => LedgerError::Io(kind),
            FileError::Corrupt(reason, source) => LedgerError::Corrupt(reason, source),
            FileError::InUse => LedgerError::InUse,
            FileError::Poisoned => LedgerError::Poisoned,
            FileError::Unencodable(source) => LedgerError::Unencodable(source),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    seq: u64,
    job_id: String,
    generation: u64,
    envelope: Vec<u8>,
}

/// The open ledger, the only writer of its agent ID's file. Cloning is cheap.
#[derive(Clone)]
pub struct ReceivedJobs {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ReceivedJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedJobs").finish_non_exhaustive()
    }
}

struct Inner {
    /// The highest generation recorded per job ID; changed only after a record is synced, or by a
    /// reopen that read the file again.
    index: Mutex<HashMap<String, u64>>,
    /// The write path; one writer at a time, held by a write that is still running after a stop.
    file: Mutex<FrameFile>,
}

impl ReceivedJobs {
    /// Opens the ledger of `agent_id` in the agent's registration directory for `mode`.
    pub fn open(mode: OperatingMode, agent_id: &str) -> Result<ReceivedJobs, LedgerError> {
        Self::open_in(&private_subdir(mode, record::DIR_NAME)?, agent_id)
    }

    /// Opens the ledger of `agent_id` in `dir`: takes the lock, then reads the file. An absent file
    /// is an empty ledger and is not created.
    pub fn open_in(dir: &Path, agent_id: &str) -> Result<ReceivedJobs, LedgerError> {
        Self::open_with(dir, agent_id, Box::new(|_| Ok(())))
    }

    /// [`ReceivedJobs::open_in`] with a hook on the write path, for tests.
    #[doc(hidden)]
    pub fn open_with(
        dir: &Path,
        agent_id: &str,
        probe: Probe,
    ) -> Result<ReceivedJobs, LedgerError> {
        if !shared_proto::is_canonical_uuid(agent_id) {
            return Err(LedgerError::BadAgentId);
        }
        let mut file = FrameFile::new(dir, agent_id, LAYOUT, probe);
        let index = read_index(&mut file)?;
        Ok(ReceivedJobs {
            inner: Arc::new(Inner {
                index: Mutex::new(index),
                file: Mutex::new(file),
            }),
        })
    }

    /// The highest generation recorded for `job_id`, from memory.
    pub fn highest(&self, job_id: &str) -> Option<u64> {
        self.index().get(job_id).copied()
    }

    /// How many job IDs have a record.
    pub fn jobs(&self) -> usize {
        self.index().len()
    }

    fn index(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.inner.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes the record of `job_id` under `generation`, with the signed `envelope`, durable, and
    /// only then counts it in the index. The first record creates the file. Blocking (file
    /// syncs): the channel calls it on a blocking task. An error leaves the index as it was and
    /// poisons the ledger (a refusal, [`LedgerError::Refused`], writes nothing and does not).
    pub fn record(
        &self,
        job_id: &str,
        generation: u64,
        envelope: &[u8],
    ) -> Result<(), LedgerError> {
        let mut file = self.inner.file.lock().unwrap_or_else(|e| e.into_inner());
        if file.is_poisoned() {
            return Err(LedgerError::Poisoned);
        }
        if !shared_proto::is_canonical_uuid(job_id)
            || envelope.is_empty()
            || envelope.len() > MAX_ENVELOPE
        {
            return Err(LedgerError::Refused);
        }
        let entry = Entry {
            seq: file.next(),
            job_id: job_id.to_owned(),
            generation,
            envelope: envelope.to_vec(),
        };
        file.append(&FrameFile::encode(&entry)?)?;
        self.index()
            .entry(job_id.to_owned())
            .and_modify(|highest| *highest = (*highest).max(generation))
            .or_insert(generation);
        Ok(())
    }

    /// Whether the ledger refuses records until [`ReceivedJobs::reopen_if_poisoned`] succeeds.
    pub fn is_poisoned(&self) -> bool {
        self.inner
            .file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_poisoned()
    }

    /// Opens the ledger again when it is poisoned: it releases its file handle and its lock, then
    /// repeats the open, so a torn frame is cut off, a whole one counts and the index is built
    /// from the file. `Ok(true)` when it did, `Ok(false)` when it was not poisoned. On failure the
    /// ledger stays poisoned, holds neither the file nor the lock and keeps the index it had.
    /// Blocking.
    pub fn reopen_if_poisoned(&self) -> Result<bool, LedgerError> {
        let mut file = self.inner.file.lock().unwrap_or_else(|e| e.into_inner());
        if !file.is_poisoned() {
            return Ok(false);
        }
        let mut index = HashMap::new();
        file.reopen_if_poisoned(
            &mut |seq, payload| read_record(&mut index, seq, payload),
            &seq_of,
        )?;
        *self.index() = index;
        Ok(true)
    }
}

/// Opens `file` and builds the index from its records.
fn read_index(file: &mut FrameFile) -> Result<HashMap<String, u64>, LedgerError> {
    let mut index = HashMap::new();
    file.open_state(
        &mut |seq, payload| read_record(&mut index, seq, payload),
        &seq_of,
    )?;
    Ok(index)
}

/// A record frame's number, `None` when the payload is not a record.
fn seq_of(payload: &[u8]) -> Option<u64> {
    postcard::from_bytes::<Entry>(payload).ok().map(|e| e.seq)
}

/// Checks the record number `seq`'s `payload` and counts it in `index`.
fn read_record(
    index: &mut HashMap<String, u64>,
    seq: u64,
    payload: &[u8],
) -> Result<(), FileError> {
    let corrupt = |reason| FileError::Corrupt(reason, None);
    let entry: Entry = postcard::from_bytes(payload)
        .map_err(|e| FileError::Corrupt("a record does not decode", Some(e)))?;
    if entry.seq != seq {
        return Err(corrupt("record numbers are not contiguous"));
    }
    if !shared_proto::is_canonical_uuid(&entry.job_id) {
        return Err(corrupt("a job ID is not a canonical UUID"));
    }
    if entry.envelope.is_empty() || entry.envelope.len() > MAX_ENVELOPE {
        return Err(corrupt("an envelope has a length no record holds"));
    }
    index
        .entry(entry.job_id)
        .and_modify(|highest| *highest = (*highest).max(entry.generation))
        .or_insert(entry.generation);
    Ok(())
}

#[cfg(test)]
mod tests;

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
//! Layout: the magic, the format version (`u32` LE), then frames in the journal's format (`len`,
//! CRC-32, payload; [`crate::journal`]). The first frame is the header (the agent ID and a
//! protection word that is 0); each later frame is one record, with consecutive numbers from 0.
//! The store is opened as the outbox is (ADR-478): a lock file of its own, never deleted, taken
//! first; an absent file is an empty ledger and is not created by the open; the first record
//! creates the file (a temporary file made owner-only, synced, renamed, the directory synced);
//! later records are appended and synced. The open reads the whole file once and keeps only an
//! index from job ID to the highest generation. A last frame that a crash tore is cut off (and
//! the cut synced); anything else that does not read back is [`LedgerError::Corrupt`] and is never
//! repaired. After a failed write the ledger refuses records until it is opened again
//! ([`ReceivedJobs::reopen_if_poisoned`]), since a failed sync can lose data the OS had accepted; a frame
//! that did reach the file counts at that open, and one that did not is cut. [`ReceivedJobs::highest`]
//! reads memory only, so a poisoned ledger still recognises its duplicates.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use shared_crypto::instruction::{HEADER_LEN, MAX_PAYLOAD_LEN};
use worker_host::service::OperatingMode;

use crate::journal::{
    FRAME_HEADER, FrameRead, JournalError, WriterLock, push_frame, read_frame, record_follows,
    sync_after_rename,
};
use crate::outbox::{Probe, Step};
use crate::registration::record;
use crate::state::{StateDirError, private_subdir};

/// The ledger format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The longest envelope a record holds: the envelope's fixed part and the longest payload.
pub const MAX_ENVELOPE: usize = HEADER_LEN + MAX_PAYLOAD_LEN;

const MAGIC: [u8; 8] = *b"NGRRJOB\0";
/// Magic and format version.
const PREAMBLE: usize = MAGIC.len() + 4;
/// The ledger file's name ends with this; the agent ID comes first.
const FILE_SUFFIX: &str = ".received-jobs.v1";
/// The lock file: held while the store is open, and never deleted.
const LOCK_FILE: &str = "received-jobs.lock";
/// The room a record's number, job ID, generation and lengths take beyond the envelope.
const RECORD_OVERHEAD: usize = 128;
/// The longest a frame that a crash left unfinished can be.
const MAX_TORN: usize = FRAME_HEADER + MAX_ENVELOPE + RECORD_OVERHEAD;

/// Why the ledger cannot be opened or written. The texts name no path and no job.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("the received-jobs ledger cannot be read or written: {0:?}")]
    Io(io::ErrorKind),
    #[error("the state directory: {0}")]
    State(#[from] StateDirError),
    /// The file is not what the writer makes (never repaired).
    #[error("the received-jobs ledger file is corrupt: {0}")]
    Corrupt(&'static str),
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
}

impl From<io::Error> for LedgerError {
    fn from(error: io::Error) -> Self {
        LedgerError::Io(error.kind())
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    agent_id: String,
    protection: u32,
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

struct Inner {
    /// The highest generation recorded per job ID; changed only after a record is synced, or by a
    /// reopen that read the file again.
    index: Mutex<HashMap<String, u64>>,
    /// The write path; one writer at a time, held by a write that is still running after a stop.
    io: Mutex<Io>,
}

struct Io {
    dir: PathBuf,
    path: PathBuf,
    agent_id: String,
    /// `None` while the file does not exist yet, and while the ledger is poisoned and closed.
    file: Option<File>,
    /// The writer lock; `None` only while the ledger is being opened or after a failed reopen.
    lock: Option<WriterLock>,
    /// The file's length up to the end of the last frame this process knows to be whole.
    len: u64,
    /// The number the next record takes.
    next: u64,
    poisoned: bool,
    probe: Probe,
}

/// What reading the file gave.
struct Parsed {
    index: HashMap<String, u64>,
    /// The number of records.
    records: u64,
    /// The length up to the end of the last whole frame.
    len: usize,
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
        let mut io = Io {
            dir: dir.to_owned(),
            path: dir.join(format!("{agent_id}{FILE_SUFFIX}")),
            agent_id: agent_id.to_owned(),
            file: None,
            lock: None,
            len: 0,
            next: 0,
            poisoned: false,
            probe,
        };
        let index = io.open_state()?;
        Ok(ReceivedJobs {
            inner: Arc::new(Inner {
                index: Mutex::new(index),
                io: Mutex::new(io),
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
        let mut io = self.inner.io.lock().unwrap_or_else(|e| e.into_inner());
        if io.poisoned {
            return Err(LedgerError::Poisoned);
        }
        if !shared_proto::is_canonical_uuid(job_id)
            || envelope.is_empty()
            || envelope.len() > MAX_ENVELOPE
        {
            return Err(LedgerError::Refused);
        }
        let entry = Entry {
            seq: io.next,
            job_id: job_id.to_owned(),
            generation,
            envelope: envelope.to_vec(),
        };
        let mut frame = Vec::new();
        let encoded = postcard::to_allocvec(&entry)
            .map_err(|_| LedgerError::Refused)
            .and_then(|payload| push_frame(&mut frame, &payload).map_err(|_| LedgerError::Refused));
        encoded?;
        if let Err(error) = io.write(&frame) {
            io.poisoned = true;
            return Err(error.into());
        }
        io.len += frame.len() as u64;
        io.next += 1;
        self.index()
            .entry(job_id.to_owned())
            .and_modify(|highest| *highest = (*highest).max(generation))
            .or_insert(generation);
        Ok(())
    }

    /// Whether the ledger refuses records until [`ReceivedJobs::reopen_if_poisoned`] succeeds.
    pub fn is_poisoned(&self) -> bool {
        self.inner
            .io
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poisoned
    }

    /// Opens the ledger again when it is poisoned: it releases its file handle and its lock, then
    /// repeats the open, so a torn frame is cut off, a whole one counts and the index is built
    /// from the file. `Ok(true)` when it did, `Ok(false)` when it was not poisoned. On failure the
    /// ledger stays poisoned, holds neither the file nor the lock and keeps the index it had.
    /// Blocking.
    pub fn reopen_if_poisoned(&self) -> Result<bool, LedgerError> {
        let mut io = self.inner.io.lock().unwrap_or_else(|e| e.into_inner());
        if !io.poisoned {
            return Ok(false);
        }
        io.file = None;
        io.lock = None;
        let index = match io.open_state() {
            Ok(index) => index,
            Err(error) => {
                // Neither the file nor the lock stays held after a reopen that failed.
                io.file = None;
                io.lock = None;
                return Err(error);
            }
        };
        io.poisoned = false;
        *self.index() = index;
        Ok(true)
    }
}

impl Io {
    /// Takes the writer lock, then reads the file: the steps of an open, which a reopen repeats.
    /// Returns the index; sets the file, the length and the next number only when it succeeds.
    fn open_state(&mut self) -> Result<HashMap<String, u64>, LedgerError> {
        self.file = None;
        self.lock = None;
        let lock = WriterLock::at(&self.dir.join(LOCK_FILE)).map_err(|error| match error {
            JournalError::InUse => LedgerError::InUse,
            JournalError::Io(error) => error.into(),
            _ => LedgerError::Io(io::ErrorKind::Other),
        })?;
        self.lock = Some(lock);
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.len = 0;
                self.next = 0;
                return Ok(HashMap::new());
            }
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            return Err(LedgerError::Corrupt("not a regular file"));
        }
        let bytes = fs::read(&self.path)?;
        let parsed = parse(&bytes, &self.agent_id)?;
        let file = open_rw(&self.path)?;
        if parsed.len < bytes.len() {
            tracing::warn!(
                cut = bytes.len() - parsed.len,
                "the received-jobs ledger ends in a frame its append did not finish; cutting it off"
            );
            file.set_len(parsed.len as u64)?;
        }
        // A frame that landed after a failed sync may be only in the OS's cache, and the file may
        // be one that a creation renamed into place whose directory entry never reached the disk:
        // make both durable before the records count. A failure fails the open.
        file.sync_all()?;
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &self.path)?;
        self.file = Some(file);
        self.len = parsed.len as u64;
        self.next = parsed.records;
        Ok(parsed.index)
    }

    fn tmp_path(&self) -> PathBuf {
        self.dir.join(format!("{}{FILE_SUFFIX}.tmp", self.agent_id))
    }

    /// Appends `frame` and syncs it, or, when the file does not exist yet, creates the file with
    /// it.
    fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        let Some(file) = self.file.as_mut() else {
            return self.create(frame);
        };
        (self.probe)(Step::BeforeWrite)?;
        file.seek(SeekFrom::Start(self.len))?;
        file.write_all(frame)?;
        (self.probe)(Step::AfterWrite)?;
        file.sync_all()?;
        (self.probe)(Step::AfterSync)
    }

    /// Writes the file whole (the preamble, the header and the first record) under a temporary
    /// name, owner-only, syncs it, renames it into place and syncs the directory. A stale
    /// temporary file is removed first. An error before the rename leaves no file and no
    /// temporary file.
    fn create(&mut self, frame: &[u8]) -> io::Result<()> {
        let tmp = self.tmp_path();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        let header = postcard::to_allocvec(&Header {
            agent_id: self.agent_id.clone(),
            protection: 0,
        })
        .map_err(io::Error::other)?;
        push_frame(&mut bytes, &header).map_err(|_| io::Error::other("the header is too large"))?;
        let preface = bytes.len() as u64;
        bytes.extend_from_slice(frame);
        let prepared = (|| {
            match fs::remove_file(&tmp) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            (self.probe)(Step::BeforeWrite)?;
            let mut file = record::create_private(&tmp)?;
            file.write_all(&bytes)?;
            (self.probe)(Step::AfterWrite)?;
            file.sync_all()?;
            (self.probe)(Step::AfterSync)?;
            (self.probe)(Step::BeforeRename)?;
            drop(file);
            fs::rename(&tmp, &self.path)
        })();
        if let Err(error) = prepared {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        // The file is in place; a failure from here poisons the ledger, and the next open reads
        // the file and syncs its entry.
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &self.path)?;
        self.file = Some(open_rw(&self.path)?);
        // `record` adds the frame's length to the length held: it is the length before the frame.
        self.len = preface;
        Ok(())
    }
}

fn open_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// A record frame's number, `None` when the payload is not a record.
fn seq_of(payload: &[u8]) -> Option<u64> {
    postcard::from_bytes::<Entry>(payload).ok().map(|e| e.seq)
}

/// Reads a whole file: the index, the number of records and the length up to the end of the last
/// whole frame. Anything that is not one unfinished append at the end is corrupt.
fn parse(bytes: &[u8], agent_id: &str) -> Result<Parsed, LedgerError> {
    let corrupt = LedgerError::Corrupt;
    if bytes.len() < PREAMBLE || bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt("not a received-jobs ledger"));
    }
    let version = u32::from_le_bytes(bytes[MAGIC.len()..PREAMBLE].try_into().expect("four bytes"));
    if version != FORMAT_VERSION {
        return Err(corrupt("unknown format version"));
    }
    let FrameRead::Ok { payload, next } = read_frame(bytes, PREAMBLE) else {
        return Err(corrupt("the header does not read back"));
    };
    let header: Header =
        postcard::from_bytes(payload).map_err(|_| corrupt("the header does not decode"))?;
    if header.agent_id != agent_id {
        return Err(corrupt("the file belongs to another agent ID"));
    }
    if header.protection != 0 {
        return Err(corrupt("unknown protection"));
    }
    let mut index: HashMap<String, u64> = HashMap::new();
    let mut records = 0u64;
    let mut offset = next;
    while offset < bytes.len() {
        match read_frame(bytes, offset) {
            FrameRead::Ok { payload, next } => {
                let entry: Entry = postcard::from_bytes(payload)
                    .map_err(|_| corrupt("a record does not decode"))?;
                if entry.seq != records {
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
                records += 1;
                offset = next;
            }
            // One unfinished append leaves at most one frame and no record after it; a damaged
            // length field or a zeroed block followed by whole records is damage.
            FrameRead::Torn
                if bytes.len() - offset <= MAX_TORN
                    && !record_follows(bytes, offset, records, seq_of) =>
            {
                break;
            }
            FrameRead::Torn => return Err(corrupt("a record does not read back")),
            FrameRead::Bad => return Err(corrupt("a record fails its checksum")),
        }
    }
    Ok(Parsed {
        index,
        records,
        len: offset,
    })
}

#[cfg(test)]
mod tests;

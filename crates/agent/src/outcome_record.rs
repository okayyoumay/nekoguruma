//! The outcome record: which jobs' terminal states the agent has decided and which of those it has
//! put in the outbox (design 5.3, 5.6, ADR-511 item 7).
//!
//! One file per agent ID, `<state>/registration/<agent-id>.outcomes.v1`, beside the received-jobs
//! ledger and in the same frame layout ([`crate::frame_file`]). It holds two kinds of record, in
//! the order they were written:
//!
//! - `ended`: the job, its ownership generation and the terminal state the intake decided for it,
//!   made durable before the state's event is appended to the outbox;
//! - `reported`: the job and generation whose `ended` event the outbox has taken, written after
//!   that append.
//!
//! A job's `ended` without a later `reported` is [`OutcomeRecord::owed`]: a crash, or a failed
//! append, came between the two, and the intake appends the state at its start, so a terminal
//! state is appended to the outbox at least once and, after a crash only between the two records,
//! at most one more time (the server stores a repeated terminal state once, ADR-478). The open
//! reads the whole file once and keeps the standing of each job in memory. A file that does not
//! read back is [`RecordError::Corrupt`] and is never repaired. After a failed write the record
//! refuses writes until it is opened again ([`OutcomeRecord::reopen_if_poisoned`]).

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use shared_proto::{JobId, JobState};
use worker_host::service::OperatingMode;

use crate::frame_file::{FileError, FrameFile, Layout};
use crate::journal::{FRAME_HEADER, JobKey, JournalError, MAX_FAILURE_DETAIL_LEN};
use crate::outbox::Probe;
use crate::registration::record;
use crate::state::{StateDirError, private_subdir};

/// The file format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;

const MAGIC: [u8; 8] = *b"NGROUTC\0";
/// The file's name ends with this; the agent ID comes first.
const FILE_SUFFIX: &str = ".outcomes.v1";
/// The lock file: held while the store is open, and never deleted.
const LOCK_FILE: &str = "outcomes.lock";
/// The room a record's number, job ID, generation, tags and the state's code and lengths take
/// beyond the state's detail and procedure code.
const RECORD_OVERHEAD: usize = 512;
/// The longest a frame that a crash left unfinished can be.
const MAX_TORN: usize = FRAME_HEADER + RECORD_OVERHEAD + 2 * MAX_FAILURE_DETAIL_LEN;

const LAYOUT: Layout = Layout {
    magic: MAGIC,
    version: FORMAT_VERSION,
    suffix: FILE_SUFFIX,
    lock_file: LOCK_FILE,
    max_torn: MAX_TORN,
    what: "outcome record",
};

/// Why the record cannot be opened or written. The texts name no path and no job.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("the outcome record cannot be read or written: {0:?}")]
    Io(io::ErrorKind),
    #[error("the state directory: {0}")]
    State(#[from] StateDirError),
    /// The file is not what the writer makes (never repaired). The second field is the decoder's
    /// error when a record did not decode.
    #[error("the outcome record file is corrupt: {0}")]
    Corrupt(&'static str, #[source] Option<postcard::Error>),
    /// Another store of this directory is open, in this process or another.
    #[error("the outcome record is open for writing elsewhere")]
    InUse,
    #[error("the agent ID cannot name the outcome record file")]
    BadAgentId,
    /// A write failed (in this call or an earlier one); open the record again.
    #[error("a write to the outcome record failed")]
    Poisoned,
    /// The job ID is not a canonical UUID: nothing is written.
    #[error("the record is not one the outcome record holds")]
    Refused,
    /// The record could not be encoded or framed; the source says why.
    #[error("the record is not one the outcome record holds")]
    Unencodable(#[source] JournalError),
}

impl From<FileError> for RecordError {
    fn from(error: FileError) -> Self {
        match error {
            FileError::Io(kind) => RecordError::Io(kind),
            FileError::Corrupt(reason, source) => RecordError::Corrupt(reason, source),
            FileError::InUse => RecordError::InUse,
            FileError::Poisoned => RecordError::Poisoned,
            FileError::Unencodable(source) => RecordError::Unencodable(source),
        }
    }
}

#[derive(Serialize, Deserialize)]
enum What {
    Ended(JobState),
    Reported,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    seq: u64,
    job_id: String,
    generation: u64,
    what: What,
}

/// What the record holds of one job.
#[derive(Debug, Clone, PartialEq)]
struct Standing {
    state: JobState,
    reported: bool,
}

/// The jobs' standings in the order they ended.
#[derive(Debug, Default)]
struct Standings {
    order: Vec<(String, u64)>,
    by_job: HashMap<(String, u64), Standing>,
}

impl Standings {
    fn apply(&mut self, entry: Entry) -> Result<(), FileError> {
        let key = (entry.job_id, entry.generation);
        match entry.what {
            What::Ended(state) => {
                if self.by_job.contains_key(&key) {
                    return Err(FileError::Corrupt("a job ended twice", None));
                }
                self.order.push(key.clone());
                self.by_job.insert(
                    key,
                    Standing {
                        state,
                        reported: false,
                    },
                );
            }
            What::Reported => match self.by_job.get_mut(&key) {
                Some(standing) if !standing.reported => standing.reported = true,
                Some(_) => return Err(FileError::Corrupt("a job was reported twice", None)),
                None => {
                    return Err(FileError::Corrupt(
                        "a job was reported before it ended",
                        None,
                    ));
                }
            },
        }
        Ok(())
    }
}

/// The open record, the only writer of its agent ID's file. Cloning is cheap.
#[derive(Clone)]
pub struct OutcomeRecord {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OutcomeRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutcomeRecord").finish_non_exhaustive()
    }
}

struct Inner {
    /// What the file holds; changed only after a record is synced, or by a reopen that read the
    /// file again.
    standings: Mutex<Standings>,
    /// The write path; one writer at a time.
    file: Mutex<FrameFile>,
}

impl OutcomeRecord {
    /// Opens the record of `agent_id` in the agent's registration directory for `mode`.
    pub fn open(mode: OperatingMode, agent_id: &str) -> Result<OutcomeRecord, RecordError> {
        Self::open_in(&private_subdir(mode, record::DIR_NAME)?, agent_id)
    }

    /// Opens the record of `agent_id` in `dir`: takes the lock, then reads the file. An absent file
    /// is an empty record and is not created.
    pub fn open_in(dir: &Path, agent_id: &str) -> Result<OutcomeRecord, RecordError> {
        Self::open_with(dir, agent_id, Box::new(|_| Ok(())))
    }

    /// [`OutcomeRecord::open_in`] with a hook on the write path, for tests.
    #[doc(hidden)]
    pub fn open_with(
        dir: &Path,
        agent_id: &str,
        probe: Probe,
    ) -> Result<OutcomeRecord, RecordError> {
        if !shared_proto::is_canonical_uuid(agent_id) {
            return Err(RecordError::BadAgentId);
        }
        let mut file = FrameFile::new(dir, agent_id, LAYOUT, probe);
        let mut standings = Standings::default();
        file.open_state(
            &mut |seq, payload| read(&mut standings, seq, payload),
            &seq_of,
        )?;
        Ok(OutcomeRecord {
            inner: Arc::new(Inner {
                standings: Mutex::new(standings),
                file: Mutex::new(file),
            }),
        })
    }

    fn standings(&self) -> std::sync::MutexGuard<'_, Standings> {
        self.inner
            .standings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The jobs that ended and are not reported, with the state each ended in, in the order they
    /// ended. From memory.
    pub fn owed(&self) -> Vec<(JobKey, JobState)> {
        let standings = self.standings();
        standings
            .order
            .iter()
            .filter_map(|key| {
                let standing = standings.by_job.get(key)?;
                (!standing.reported).then(|| {
                    let (job_id, generation) = key;
                    (
                        JobKey {
                            job_id: JobId(job_id.clone()),
                            generation: *generation,
                        },
                        standing.state.clone(),
                    )
                })
            })
            .collect()
    }

    /// The state `key` ended in, when the record holds one, and whether it is reported. From
    /// memory.
    pub fn standing(&self, key: &JobKey) -> Option<(JobState, bool)> {
        let standings = self.standings();
        let standing = standings
            .by_job
            .get(&(key.job_id.0.clone(), key.generation))?;
        Some((standing.state.clone(), standing.reported))
    }

    /// Makes the end of `key` in `state` durable, and only then counts it. A job that has ended
    /// already is left as it is: the first end stands. Blocking (file syncs).
    pub fn ended(&self, key: &JobKey, state: &JobState) -> Result<(), RecordError> {
        self.write(key, What::Ended(state.clone()))
    }

    /// Makes it durable that the event of `key`'s end is in the outbox. A job that did not end,
    /// or is reported already, writes nothing. Blocking (file syncs).
    pub fn reported(&self, key: &JobKey) -> Result<(), RecordError> {
        self.write(key, What::Reported)
    }

    fn write(&self, key: &JobKey, what: What) -> Result<(), RecordError> {
        let mut file = self.inner.file.lock().unwrap_or_else(|e| e.into_inner());
        if file.is_poisoned() {
            return Err(RecordError::Poisoned);
        }
        if !shared_proto::is_canonical_uuid(&key.job_id.0) {
            return Err(RecordError::Refused);
        }
        let known = self.standing(key);
        let needed = match (&what, known) {
            (What::Ended(_), None) => true,
            (What::Reported, Some((_, false))) => true,
            (What::Ended(_), Some(_)) | (What::Reported, _) => false,
        };
        if !needed {
            return Ok(());
        }
        let entry = Entry {
            seq: file.next(),
            job_id: key.job_id.0.clone(),
            generation: key.generation,
            what,
        };
        file.append(&FrameFile::encode(&entry)?)?;
        self.standings()
            .apply(entry)
            .expect("an entry that passed the check above is consistent with the standings");
        Ok(())
    }

    /// Whether the record refuses writes until [`OutcomeRecord::reopen_if_poisoned`] succeeds.
    pub fn is_poisoned(&self) -> bool {
        self.inner
            .file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_poisoned()
    }

    /// Opens the record again when it is poisoned (see [`FrameFile::reopen_if_poisoned`]):
    /// `Ok(true)` when it did, `Ok(false)` when it was not poisoned. Blocking.
    pub fn reopen_if_poisoned(&self) -> Result<bool, RecordError> {
        let mut file = self.inner.file.lock().unwrap_or_else(|e| e.into_inner());
        if !file.is_poisoned() {
            return Ok(false);
        }
        let mut standings = Standings::default();
        file.reopen_if_poisoned(
            &mut |seq, payload| read(&mut standings, seq, payload),
            &seq_of,
        )?;
        *self.standings() = standings;
        Ok(true)
    }
}

/// A record frame's number, `None` when the payload is not a record.
fn seq_of(payload: &[u8]) -> Option<u64> {
    postcard::from_bytes::<Entry>(payload).ok().map(|e| e.seq)
}

/// Checks the record number `seq`'s `payload` and applies it to `standings`.
fn read(standings: &mut Standings, seq: u64, payload: &[u8]) -> Result<(), FileError> {
    let entry: Entry = postcard::from_bytes(payload)
        .map_err(|e| FileError::Corrupt("a record does not decode", Some(e)))?;
    if entry.seq != seq {
        return Err(FileError::Corrupt(
            "record numbers are not contiguous",
            None,
        ));
    }
    if !shared_proto::is_canonical_uuid(&entry.job_id) {
        return Err(FileError::Corrupt("a job ID is not a canonical UUID", None));
    }
    standings.apply(entry)
}

#[cfg(test)]
mod tests;

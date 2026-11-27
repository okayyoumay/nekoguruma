//! The start deadline's clock and the highest issue time (design 5.3, ADR-486, ADR-461 Decision
//! item 6).
//!
//! A job's instruction names the time the server issued it and the time after which the job must
//! not be started, in Unix seconds. The agent judges the deadline against the later of the device
//! clock and the highest issue time it has verified so far, so a device clock that was set back
//! cannot revive an instruction the agent has already seen to be stale. This module holds the two
//! halves:
//!
//! - [`Clock`], the device clock as a trait, and [`SystemClock`], the production one. The control
//!   channel reads no wall clock itself (its tests scan for one), so the seam lives here.
//! - [`IssueMark`], the highest issue time: in memory, and in an owner-only file
//!   `<state>/registration/<agent-id>.issue-mark.v1.json` that holds the agent ID and the value. A
//!   value is raised in memory by [`IssueMark::raise`] (a maximum, never a store) and made durable
//!   by [`IssueMark::sync`] before the command is answered: a temporary file, synced, renamed over
//!   the old one, the directory synced. The file is named after the agent ID, so a
//!   re-registration (a new agent ID, ADR-461 Decision item 8) starts afresh and leaves the old
//!   file alone. [`IssueMark::open_in`] takes its own lock file, never creates or repairs the
//!   file, and refuses one that does not read back (a corrupt file is never read as 0).
//!
//! The judgement is [`IssueMark::judge`]: a deadline at or before the later of the two times is
//! expired. Nothing here opens a link or keeps a job state.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use worker_host::service::OperatingMode;

use crate::journal::{JournalError, WriterLock, sync_after_rename};
use crate::outbox::{Probe, Step};
use crate::registration::record;
use crate::state::{StateDirError, private_subdir};

/// The mark file's format version.
pub const FORMAT_VERSION: u32 = 1;
/// The longest mark file the reader takes, in bytes.
const MAX_FILE: u64 = 1024;
/// The mark file's name ends with this; the agent ID comes first.
const FILE_SUFFIX: &str = ".issue-mark.v1.json";
/// The lock file: held while the store is open, and never deleted.
const LOCK_FILE: &str = "issue-mark.lock";

/// The device's clock, in Unix seconds.
pub trait Clock: Send + Sync {
    /// The time now; 0 when the clock is before the epoch.
    fn now_unix(&self) -> u64;
}

/// The device's wall clock.
#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        unix_secs(SystemTime::now())
    }
}

/// `time` in Unix seconds; 0 for a time before the epoch.
fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// What the deadline came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The deadline lies after both times: the job may start.
    Open,
    /// The deadline is at or before the later of the two times. `by_mark` is true when the
    /// device clock alone would not have said so (the clock is behind the highest issue time).
    Expired { by_mark: bool },
}

/// Why the mark cannot be opened or written. The texts name no path and no value.
#[derive(Debug, thiserror::Error)]
pub enum MarkError {
    #[error("the issue-time mark cannot be read or written: {0:?}")]
    Io(io::ErrorKind),
    #[error("the state directory: {0}")]
    State(#[from] StateDirError),
    /// The file is not what the writer makes: it is not a regular file, is too large, is not the
    /// expected object, or names another agent. The second field is the JSON error when the
    /// content did not parse.
    #[error("the issue-time mark file is corrupt: {0}")]
    Corrupt(&'static str, #[source] Option<serde_json::Error>),
    /// Another store of this directory is open, in this process or another.
    #[error("the issue-time mark is open for writing elsewhere")]
    InUse,
    #[error("the agent ID cannot name the issue-time mark file")]
    BadAgentId,
}

impl From<io::Error> for MarkError {
    fn from(error: io::Error) -> Self {
        MarkError::Io(error.kind())
    }
}

/// The file's content.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    agent_id: String,
    highest_issued_at: u64,
}

/// The highest issue time verified so far, in memory and on disk. Cloning is cheap.
#[derive(Clone)]
pub struct IssueMark {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for IssueMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssueMark").finish_non_exhaustive()
    }
}

struct Inner {
    /// The mark: only ever raised.
    highest: AtomicU64,
    /// The value known to be in the file.
    synced: AtomicU64,
    /// The write path; one writer at a time.
    io: Mutex<Io>,
    /// Held for as long as the store, and by a write that is still running after a stop.
    _lock: WriterLock,
}

struct Io {
    dir: PathBuf,
    agent_id: String,
    probe: Probe,
}

impl IssueMark {
    /// Opens the store of `agent_id` in the agent's registration directory for `mode`.
    pub fn open(mode: OperatingMode, agent_id: &str) -> Result<IssueMark, MarkError> {
        Self::open_in(&private_subdir(mode, record::DIR_NAME)?, agent_id)
    }

    /// Opens the store of `agent_id` in `dir`: takes the lock, then reads the file. An absent file
    /// is the value 0 and is not created.
    pub fn open_in(dir: &Path, agent_id: &str) -> Result<IssueMark, MarkError> {
        Self::open_with(dir, agent_id, Box::new(|_| Ok(())))
    }

    /// [`IssueMark::open_in`] with a hook on the write path, for tests.
    #[doc(hidden)]
    pub fn open_with(dir: &Path, agent_id: &str, probe: Probe) -> Result<IssueMark, MarkError> {
        if !shared_proto::is_canonical_uuid(agent_id) {
            return Err(MarkError::BadAgentId);
        }
        let lock = WriterLock::at(&dir.join(LOCK_FILE)).map_err(|error| match error {
            JournalError::InUse => MarkError::InUse,
            JournalError::Io(error) => error.into(),
            _ => MarkError::Io(io::ErrorKind::Other),
        })?;
        let path = path_of(dir, agent_id);
        let value = read(&path, agent_id)?;
        // The file read may be one a write renamed into place whose directory entry never reached
        // the disk (a crash, or a failed sync after the rename). Its value counts as synced, so
        // a redelivery would write nothing: make the entry durable first, as the outbox's open
        // does (ADR-486). A failure fails the open.
        if value != 0 || path.exists() {
            probe(Step::AfterRename)?;
            sync_after_rename(dir, &path)?;
        }
        Ok(IssueMark {
            inner: Arc::new(Inner {
                highest: AtomicU64::new(value),
                synced: AtomicU64::new(value),
                io: Mutex::new(Io {
                    dir: dir.to_owned(),
                    agent_id: agent_id.to_owned(),
                    probe,
                }),
                _lock: lock,
            }),
        })
    }

    /// Raises the mark to `issued_at` when that is higher. The only way the value changes; it
    /// never lowers it and writes nothing.
    pub fn raise(&self, issued_at: u64) {
        self.inner.highest.fetch_max(issued_at, Ordering::SeqCst);
    }

    /// The mark in memory.
    pub fn highest(&self) -> u64 {
        self.inner.highest.load(Ordering::SeqCst)
    }

    /// Whether the mark in memory is higher than the file's.
    pub fn owes_write(&self) -> bool {
        self.inner.synced.load(Ordering::SeqCst) < self.highest()
    }

    /// Judges a start deadline: expired when it is at or before the later of the device clock and
    /// the mark. The clock is read once, here.
    pub fn judge(&self, clock: &dyn Clock, deadline: u64) -> Verdict {
        let device = clock.now_unix();
        if device.max(self.highest()) >= deadline {
            Verdict::Expired {
                by_mark: device < deadline,
            }
        } else {
            Verdict::Open
        }
    }

    /// Makes the mark durable when the file holds less: writes the whole file and syncs it. On an
    /// error the file's known value is unchanged, so the next call writes again. Blocking (file
    /// syncs): the channel calls it on a blocking task.
    pub fn sync(&self) -> Result<(), MarkError> {
        let mut io = self.inner.io.lock().unwrap_or_else(|e| e.into_inner());
        let target = self.highest();
        if self.inner.synced.load(Ordering::SeqCst) >= target {
            return Ok(());
        }
        io.write(target)?;
        self.inner.synced.fetch_max(target, Ordering::SeqCst);
        Ok(())
    }
}

fn path_of(dir: &Path, agent_id: &str) -> PathBuf {
    dir.join(format!("{agent_id}{FILE_SUFFIX}"))
}

/// Reads the value of the file at `path`: 0 when it is absent. A symbolic link, a directory, a
/// file over the limit or one that is not the writer's object is corrupt; a read that fails is an
/// I/O error.
fn read(path: &Path, agent_id: &str) -> Result<u64, MarkError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(MarkError::Corrupt(
            "not a regular file of the expected size",
            None,
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(MarkError::Corrupt(
            "not a regular file of the expected size",
            None,
        ));
    }
    let stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|e| MarkError::Corrupt("not the expected object", Some(e)))?;
    if stored.version != FORMAT_VERSION {
        return Err(MarkError::Corrupt("unknown version", None));
    }
    if stored.agent_id != agent_id {
        return Err(MarkError::Corrupt("another agent ID", None));
    }
    Ok(stored.highest_issued_at)
}

impl Io {
    fn tmp_path(&self) -> PathBuf {
        self.dir.join(format!("{}{FILE_SUFFIX}.tmp", self.agent_id))
    }

    /// Replaces the file with one holding `value`: a stale temporary file is removed, a new one is
    /// made owner-only, written and synced, renamed over the file and the directory synced. An
    /// error before the rename leaves the old file and no temporary file.
    fn write(&mut self, value: u64) -> Result<(), MarkError> {
        let tmp = self.tmp_path();
        let path = path_of(&self.dir, &self.agent_id);
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let renamed = self.write_temp(&tmp, value).and_then(|()| {
            (self.probe)(Step::BeforeRename)?;
            fs::rename(&tmp, &path)
        });
        if let Err(error) = renamed {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        // The file is in place; a failure from here leaves the known value as it was, so the
        // next call writes again.
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &path)?;
        Ok(())
    }

    fn write_temp(&self, tmp: &Path, value: u64) -> io::Result<()> {
        (self.probe)(Step::BeforeWrite)?;
        let mut file = record::create_private(tmp)?;
        let mut bytes = serde_json::to_vec(&Stored {
            version: FORMAT_VERSION,
            agent_id: self.agent_id.clone(),
            highest_issued_at: value,
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        (self.probe)(Step::AfterWrite)?;
        file.sync_all()?;
        (self.probe)(Step::AfterSync)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

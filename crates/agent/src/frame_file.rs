//! The file of an append-only store of frames, shared by the received-jobs ledger and the
//! outcome record (ADR-397, ADR-489, ADR-511 item 7).
//!
//! One file per agent ID, named `<agent-id><suffix>` in a directory, holds the magic, the format
//! version (`u32` LE), then frames in the journal's format (`len`, CRC-32, payload;
//! [`crate::journal`]). The first frame is the header (the agent ID and a protection word that is
//! 0); each later frame is one record whose payload starts with its number, consecutive from 0 (the
//! caller's type; this module reads the number through the function the caller gives).
//!
//! The store is opened as the outbox is (ADR-478): a lock file of its own, never deleted, taken
//! first; an absent file is an empty store and is not created by the open; the first record
//! creates the file (a temporary file made owner-only, synced, renamed, the directory synced);
//! later records are appended and synced. A last frame that a crash tore is cut off (and the cut
//! synced); anything else that does not read back is [`FileError::Corrupt`] and is never repaired.
//! After a failed write the file refuses records until it is opened again
//! ([`FrameFile::reopen_if_poisoned`]), since a failed sync can lose data the OS had accepted; a
//! frame that did reach the file counts at that open, and one that did not is cut.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::journal::{
    FrameRead, JournalError, WriterLock, encode, push_frame, read_frame, record_follows,
    sync_after_rename,
};
use crate::outbox::{Probe, Step};
use crate::registration::record;

/// What distinguishes one store's file from another's.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    pub magic: [u8; 8],
    pub version: u32,
    /// The file's name ends with this; the agent ID comes first.
    pub suffix: &'static str,
    /// The lock file: held while the store is open, and never deleted.
    pub lock_file: &'static str,
    /// The longest a frame that a crash left unfinished can be.
    pub max_torn: usize,
    /// What the log says the file is, for the cut of a torn frame.
    pub what: &'static str,
}

/// What reads one record of the file at an open: its number and payload.
pub(crate) type ReadRecord<'a> = dyn FnMut(u64, &[u8]) -> Result<(), FileError> + 'a;

/// Why the file cannot be opened or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FileError {
    #[error("the file cannot be read or written: {0:?}")]
    Io(io::ErrorKind),
    /// The file is not what the writer makes. The second field is the decoder's error when a
    /// record did not decode.
    #[error("the file is corrupt: {0}")]
    Corrupt(&'static str, #[source] Option<postcard::Error>),
    /// Another store of this directory is open, in this process or another.
    #[error("the file is open for writing elsewhere")]
    InUse,
    /// A write failed (in this call or an earlier one); open the file again.
    #[error("a write to the file failed")]
    Poisoned,
    /// The record could not be encoded or framed; the source says why.
    #[error("the record could not be framed")]
    Unencodable(#[source] JournalError),
}

impl From<io::Error> for FileError {
    fn from(error: io::Error) -> Self {
        FileError::Io(error.kind())
    }
}

#[derive(Serialize, Deserialize)]
struct Header {
    agent_id: String,
    protection: u32,
}

/// The open file, the only writer of its agent ID's file.
pub(crate) struct FrameFile {
    layout: Layout,
    dir: PathBuf,
    path: PathBuf,
    agent_id: String,
    /// `None` while the file does not exist yet, and while the store is poisoned and closed.
    file: Option<File>,
    /// The writer lock; `None` only while the store is being opened or after a failed reopen.
    lock: Option<WriterLock>,
    /// The file's length up to the end of the last frame this process knows to be whole.
    len: u64,
    /// The number the next record takes.
    next: u64,
    poisoned: bool,
    probe: Probe,
}

impl FrameFile {
    /// The store of `agent_id` in `dir`, not yet opened: call [`FrameFile::open_state`].
    pub(crate) fn new(dir: &Path, agent_id: &str, layout: Layout, probe: Probe) -> Self {
        Self {
            layout,
            dir: dir.to_owned(),
            path: dir.join(format!("{agent_id}{}", layout.suffix)),
            agent_id: agent_id.to_owned(),
            file: None,
            lock: None,
            len: 0,
            next: 0,
            poisoned: false,
            probe,
        }
    }

    /// The number the next record takes.
    pub(crate) fn next(&self) -> u64 {
        self.next
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Takes the writer lock, then reads the file, calling `record` with the number and the
    /// payload of each whole record in order; `number` reads a record's number from a payload,
    /// `None` when it is not a record (used to tell a torn tail from damage). A `record` that
    /// fails fails the open. Sets the file, the length and the next number only when it succeeds.
    pub(crate) fn open_state(
        &mut self,
        record: &mut ReadRecord<'_>,
        number: &dyn Fn(&[u8]) -> Option<u64>,
    ) -> Result<(), FileError> {
        self.file = None;
        self.lock = None;
        let lock =
            WriterLock::at(&self.dir.join(self.layout.lock_file)).map_err(|error| match error {
                JournalError::InUse => FileError::InUse,
                JournalError::Io(error) => error.into(),
                _ => FileError::Io(io::ErrorKind::Other),
            })?;
        self.lock = Some(lock);
        let meta = match fs::symlink_metadata(&self.path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.len = 0;
                self.next = 0;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            return Err(FileError::Corrupt("not a regular file", None));
        }
        let bytes = fs::read(&self.path)?;
        let (records, len) = self.parse(&bytes, record, number)?;
        let file = open_rw(&self.path)?;
        if len < bytes.len() {
            tracing::warn!(
                cut = bytes.len() - len,
                "the {} ends in a frame its append did not finish; cutting it off",
                self.layout.what
            );
            file.set_len(len as u64)?;
        }
        // A frame that landed after a failed sync may be only in the OS's cache, and the file may
        // be one that a creation renamed into place whose directory entry never reached the disk:
        // make both durable before the records count. A failure fails the open.
        file.sync_all()?;
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &self.path)?;
        self.file = Some(file);
        self.len = len as u64;
        self.next = records;
        Ok(())
    }

    /// Reads a whole file: the number of records and the length up to the end of the last whole
    /// frame. Anything that is not one unfinished append at the end is corrupt.
    fn parse(
        &self,
        bytes: &[u8],
        record: &mut ReadRecord<'_>,
        number: &dyn Fn(&[u8]) -> Option<u64>,
    ) -> Result<(u64, usize), FileError> {
        let corrupt = |reason| FileError::Corrupt(reason, None);
        let preamble = self.layout.magic.len() + 4;
        if bytes.len() < preamble || bytes[..self.layout.magic.len()] != self.layout.magic {
            return Err(corrupt("not a file of this store"));
        }
        let version = u32::from_le_bytes(
            bytes[self.layout.magic.len()..preamble]
                .try_into()
                .expect("four bytes"),
        );
        if version != self.layout.version {
            return Err(corrupt("unknown format version"));
        }
        let FrameRead::Ok { payload, next } = read_frame(bytes, preamble) else {
            return Err(corrupt("the header does not read back"));
        };
        let header: Header = postcard::from_bytes(payload)
            .map_err(|e| FileError::Corrupt("the header does not decode", Some(e)))?;
        if header.agent_id != self.agent_id {
            return Err(corrupt("the file belongs to another agent ID"));
        }
        if header.protection != 0 {
            return Err(corrupt("unknown protection"));
        }
        let mut records = 0u64;
        let mut offset = next;
        while offset < bytes.len() {
            match read_frame(bytes, offset) {
                FrameRead::Ok { payload, next } => {
                    record(records, payload)?;
                    records += 1;
                    offset = next;
                }
                // One unfinished append leaves at most one frame and no record after it; a damaged
                // length field or a zeroed block followed by whole records is damage.
                FrameRead::Torn
                    if bytes.len() - offset <= self.layout.max_torn
                        && !record_follows(bytes, offset, records, number) =>
                {
                    break;
                }
                FrameRead::Torn => return Err(corrupt("a record does not read back")),
                FrameRead::Bad => return Err(corrupt("a record fails its checksum")),
            }
        }
        Ok((records, offset))
    }

    /// Opens the file again when it is poisoned: it releases its file handle and its lock, then
    /// repeats [`FrameFile::open_state`]. `Ok(true)` when it did, `Ok(false)` when it was not
    /// poisoned. On failure it stays poisoned and holds neither the file nor the lock.
    pub(crate) fn reopen_if_poisoned(
        &mut self,
        record: &mut ReadRecord<'_>,
        number: &dyn Fn(&[u8]) -> Option<u64>,
    ) -> Result<bool, FileError> {
        if !self.poisoned {
            return Ok(false);
        }
        self.file = None;
        self.lock = None;
        if let Err(error) = self.open_state(record, number) {
            // Neither the file nor the lock stays held after a reopen that failed.
            self.file = None;
            self.lock = None;
            return Err(error);
        }
        self.poisoned = false;
        Ok(true)
    }

    /// Appends the record `payload` (which starts with the number [`FrameFile::next`] gave) and
    /// syncs it, creating the file with it when it does not exist yet. A failed write poisons the
    /// file.
    pub(crate) fn append(&mut self, payload: &[u8]) -> Result<(), FileError> {
        if self.poisoned {
            return Err(FileError::Poisoned);
        }
        let mut frame = Vec::new();
        push_frame(&mut frame, payload).map_err(FileError::Unencodable)?;
        if let Err(error) = self.write(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.len += frame.len() as u64;
        self.next += 1;
        Ok(())
    }

    /// Encodes `entry` for [`FrameFile::append`].
    pub(crate) fn encode<T: Serialize>(entry: &T) -> Result<Vec<u8>, FileError> {
        encode(entry).map_err(FileError::Unencodable)
    }

    fn tmp_path(&self) -> PathBuf {
        self.dir
            .join(format!("{}{}.tmp", self.agent_id, self.layout.suffix))
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
        bytes.extend_from_slice(&self.layout.magic);
        bytes.extend_from_slice(&self.layout.version.to_le_bytes());
        let header = postcard::to_allocvec(&Header {
            agent_id: self.agent_id.clone(),
            protection: 0,
        })
        .map_err(io::Error::other)?;
        push_frame(&mut bytes, &header).map_err(io::Error::other)?;
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
        // The file is in place; a failure from here poisons the store, and the next open reads
        // the file and syncs its entry.
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &self.path)?;
        self.file = Some(open_rw(&self.path)?);
        // `append` adds the frame's length to the length held: it is the length before the frame.
        self.len = preface;
        Ok(())
    }
}

fn open_rw(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

// Used by the stores' tests to build a file the way the writer does.
#[cfg(test)]
pub(crate) fn header_frame(agent_id: &str, out: &mut Vec<u8>) {
    let header = postcard::to_allocvec(&Header {
        agent_id: agent_id.to_owned(),
        protection: 0,
    })
    .expect("a header encodes");
    push_frame(out, &header).expect("a header frames");
}

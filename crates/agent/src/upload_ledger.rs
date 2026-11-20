//! The upload ledger: the agent's durable list of what the server has not yet received of a
//! finished job, a result or a communication log (design 5.2, ADR-493).
//!
//! One file per agent ID, `<state>/uploads/<agent-id>` (no extension), holds one entry for each
//! thing still to upload. Layout: the magic, the format version (`u32` LE), then frames in the
//! journal's format (`len`, CRC-32, payload; [`crate::journal`]). The first frame is the header
//! (the agent ID, the protection, the highest entry number ever assigned when the file was
//! written, and how many entries that write held); each later frame is one entry: its number,
//! the job and generation, whether it is a result or a communication log, where its bytes are
//! (stored by the ledger, or a file the ledger only names), their length and their SHA-256.
//!
//! The entries a write of the whole file produced are the retained ones (the header counts
//! them); [`UploadLedger::enqueue_result`] and [`UploadLedger::enqueue_comm_log`] append frames
//! after them, each numbered `highest + 1`, and an entry is pending only once its frame is
//! durable. A frame that a crash tore is cut off at the next open and its number is used again.
//! [`UploadLedger::finished`] removes an entry by rewriting the whole file without it (a
//! temporary file, synced, renamed over the old one, the directory synced); the header's
//! `highest` keeps the counter across an empty file, so a number is never given twice.
//!
//! A result's bytes are stored by the ledger in `<state>/uploads/<agent-id>.objects/<number>`,
//! written and synced before the entry is appended and deleted only after the entry's removal is
//! durable, so an object no entry names is a leftover of a failed or interrupted step and the
//! next open sweeps it. A communication log stays where its writer put it: its entry names the
//! file and the first `length` bytes of it, and the ledger never opens or deletes it.
//!
//! The file and the stored bytes are capped; nothing is dropped to make room. After a failed
//! write the ledger refuses enqueues and removals until it is opened again, since a failed sync
//! can lose data the OS had accepted. A file that does not read back is
//! [`LedgerError::Corrupt`] and is never repaired, set aside or swept around.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use worker_host::service::OperatingMode;

use crate::journal::{
    FRAME_HEADER, FrameRead, JobKey, LockFailure, WriterLock, job_file_stem, open_rw, push_frame,
    read_frame, record_follows, sync_after_rename, sync_dir, take_writer_lock, valid_id,
};
use crate::registration::record::create_private;
use crate::state::{StateDirError, prepare_subdir, private_subdir};

/// The ledger format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The largest the file may grow, in bytes. An enqueue that would pass it is refused.
pub const MAX_LEDGER_BYTES: u64 = 1 << 20;
/// The longest result the ledger stores, in bytes.
pub const MAX_RESULT_BYTES: u64 = 64 << 20;
/// The most the stored results may add up to, in bytes.
pub const MAX_STORED_BYTES: u64 = 1 << 30;
/// The longest communication log an entry may name: the most the server accepts for one upload.
pub const MAX_COMM_LOG_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// The longest path an entry may hold, in bytes.
pub const MAX_PATH_BYTES: usize = 4096;

const MAGIC: [u8; 8] = *b"NGRUPLD\0";
/// Magic and format version.
const PREAMBLE: usize = MAGIC.len() + 4;
const DIR_NAME: &str = "uploads";
const OBJECTS_SUFFIX: &str = ".objects";
const TMP_SUFFIX: &str = ".tmp";
const LOCK_FILE: &str = "uploads.lock";
/// The longest a job ID may be (the rule of [`job_file_stem`]).
const MAX_JOB_ID: usize = 64;
/// The longest an entry's payload can be: a varint is at most 10 bytes, a string carries a
/// 1 or 2 byte length, the kind is 1 byte, the source a tag and a path, the digest 32 bytes.
const MAX_ENTRY: usize = 10 + (1 + MAX_JOB_ID) + 10 + 1 + (1 + 2 + MAX_PATH_BYTES) + 10 + 32;
/// The longest a frame that a crash left unfinished can be.
const MAX_TORN: usize = FRAME_HEADER + MAX_ENTRY;

/// What an entry holds. Results upload before communication logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Result,
    CommLog,
}

/// Where an entry's bytes are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// An object of the ledger's own directory, named by the entry's number.
    Stored,
    /// A file the ledger does not own: an absolute UTF-8 path.
    File(String),
}

/// The length and SHA-256 of some bytes. Only this module makes one, from bytes it has read
/// ([`digest_file`]) or been given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digest {
    length: u64,
    sha256: [u8; 32],
}

impl Digest {
    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

/// Reads the file at `path` to its end through one handle and returns its length and SHA-256.
pub fn digest_file(path: &Path) -> io::Result<Digest> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
                length += read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Digest {
        length,
        sha256: hasher.finalize().into(),
    })
}

/// One thing still to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's number, at least 1, never given twice under one agent ID.
    pub seq: u64,
    pub key: JobKey,
    pub kind: Kind,
    pub source: Source,
    /// The length of the bytes, and so what to upload of a file that has grown since.
    pub length: u64,
    pub sha256: [u8; 32],
}

/// A point of the write path, for fault injection in tests (a failure the hook returns is
/// handled as one of the file system's; a hook that aborts models a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Before the object of a result is created.
    BeforeObjectWrite,
    /// After the object's bytes are written, before they are synced.
    AfterObjectWrite,
    /// After the object's bytes and its directory entry are durable, before the entry's append.
    AfterObjectSync,
    /// Before an entry's frame is written.
    BeforeWrite,
    AfterWrite,
    AfterSync,
    BeforeRename,
    /// After a rename, before it is made durable; the open calls it too, before its own sync.
    AfterRename,
    /// After an install's rename is durable: in a removal, before the object it made unnamed is
    /// deleted; also when the first open creates the file.
    AfterRenameSync,
    /// After the open cut a torn tail off, before the cut is synced.
    AfterCut,
    /// Before an object is deleted, by a removal or by the sweep at open.
    BeforeObjectDelete,
}

/// The hook called at each [`Step`].
pub type Probe = Box<dyn Fn(Step) -> io::Result<()> + Send>;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("upload ledger i/o: {0}")]
    Io(#[from] io::Error),
    #[error("the upload ledger directory: {0}")]
    State(#[from] StateDirError),
    #[error("the upload ledger file is corrupt: {0}")]
    Corrupt(&'static str),
    /// Another ledger of this directory is open for writing, in this process or another.
    #[error("the upload ledger is open for writing elsewhere")]
    InUse,
    #[error("the agent ID cannot name a ledger file")]
    BadAgentId,
    #[error("the job ID cannot name a file")]
    BadJobId,
    #[error("the source file must be an absolute UTF-8 path of at most 4096 bytes")]
    BadSource,
    #[error("the upload ledger is full")]
    Full,
    #[error("the bytes are longer than an upload may be")]
    TooLarge,
    /// The job already has an entry of this kind and generation with other bytes.
    #[error("the job already has an upload of this kind with other bytes")]
    Conflict,
    /// A write to the ledger failed (in this call or an earlier one; the cause is logged by its
    /// kind); open the ledger again.
    #[error("a write to the upload ledger failed")]
    Poisoned,
    /// A removal of a number above the highest assigned.
    #[error("finished {seq}, but the highest entry is {highest}")]
    OutOfRange { seq: u64, highest: u64 },
}

#[derive(Serialize, Deserialize)]
struct Header {
    agent_id: String,
    protection: u32,
    highest: u64,
    retained: u32,
}

/// An entry as the file holds it.
#[derive(Serialize, Deserialize)]
struct Wire {
    seq: u64,
    job_id: String,
    generation: u64,
    kind: Kind,
    source: Source,
    length: u64,
    sha256: [u8; 32],
}

impl From<&Entry> for Wire {
    fn from(entry: &Entry) -> Self {
        Self {
            seq: entry.seq,
            job_id: entry.key.job_id.0.clone(),
            generation: entry.key.generation,
            kind: entry.kind,
            source: entry.source.clone(),
            length: entry.length,
            sha256: entry.sha256,
        }
    }
}

impl From<Wire> for Entry {
    fn from(wire: Wire) -> Self {
        Self {
            seq: wire.seq,
            key: JobKey {
                job_id: shared_proto::JobId(wire.job_id),
                generation: wire.generation,
            },
            kind: wire.kind,
            source: wire.source,
            length: wire.length,
            sha256: wire.sha256,
        }
    }
}

/// An open upload ledger, the only writer of its agent ID's file.
pub struct UploadLedger {
    dir: PathBuf,
    path: PathBuf,
    objects: PathBuf,
    agent_id: String,
    /// `None` only while the file is being replaced, or after a failure to reopen it.
    file: Option<File>,
    /// The writer lock; `None` only while the ledger is being opened or after a failed reopen.
    lock: Option<WriterLock>,
    /// The highest number assigned.
    highest: u64,
    /// The entries, in number order.
    entries: Vec<Entry>,
    file_len: u64,
    /// The sum of the lengths of the stored entries.
    stored_bytes: u64,
    poisoned: bool,
    probe: Probe,
}

impl UploadLedger {
    /// Opens the ledger of `agent_id` in the agent's state directory.
    pub fn open(mode: OperatingMode, agent_id: &str) -> Result<Self, LedgerError> {
        Self::open_in(&private_subdir(mode, DIR_NAME)?, agent_id)
    }

    /// Opens the ledger of `agent_id` in `dir`, creating its file when it has none.
    pub fn open_in(dir: &Path, agent_id: &str) -> Result<Self, LedgerError> {
        Self::open_with(dir, agent_id, Box::new(|_| Ok(())))
    }

    /// [`UploadLedger::open_in`] with a hook on the write path, for tests.
    #[doc(hidden)]
    pub fn open_with(dir: &Path, agent_id: &str, probe: Probe) -> Result<Self, LedgerError> {
        if !valid_id(agent_id) {
            return Err(LedgerError::BadAgentId);
        }
        let mut ledger = Self {
            dir: dir.to_owned(),
            path: dir.join(agent_id),
            objects: dir.join(format!("{agent_id}{OBJECTS_SUFFIX}")),
            agent_id: agent_id.to_owned(),
            file: None,
            lock: None,
            highest: 0,
            entries: Vec::new(),
            file_len: 0,
            stored_bytes: 0,
            poisoned: false,
            probe,
        };
        ledger.open_state(true)?;
        Ok(ledger)
    }

    /// Takes the writer lock, loads the file (creating it when missing and `create` is set, as
    /// only the first open does), then sweeps the objects: the steps of an open, which
    /// [`UploadLedger::reopen`] repeats. A file that does not read back, or that a reopen finds
    /// gone, fails here before anything is created, cut or deleted.
    fn open_state(&mut self, create: bool) -> Result<(), LedgerError> {
        let lock = match take_writer_lock(&self.dir.join(LOCK_FILE)) {
            Ok(lock) => lock,
            Err(LockFailure::InUse) => return Err(LedgerError::InUse),
            Err(LockFailure::Io(error)) => return Err(error.into()),
        };
        self.lock = Some(lock);
        let others = other_entries(&self.dir, &self.agent_id);
        if others > 0 {
            tracing::warn!(
                count = others,
                "the upload directory holds entries of other agent IDs; leaving them alone"
            );
        }
        match fs::read(&self.path) {
            Ok(bytes) => {
                let parsed = parse(&bytes, &self.agent_id)?;
                self.prepare_objects()?;
                self.load(&bytes, parsed)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                self.prepare_objects()?;
                self.install(None)?;
            }
            // A ledger that was open and lost its file holds results the server may not have
            // received; making an empty file and sweeping their objects would lose them.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(LedgerError::Corrupt("the ledger file is gone"));
            }
            Err(error) => return Err(error.into()),
        }
        self.sweep();
        Ok(())
    }

    /// Creates the objects directory (0700 on Unix) or checks the one that is there.
    fn prepare_objects(&self) -> Result<(), LedgerError> {
        let name = format!("{}{OBJECTS_SUFFIX}", self.agent_id);
        prepare_subdir(&self.dir, false, &name)?;
        Ok(())
    }

    /// Opens the ledger again with what it was opened with (directory, agent ID, probe): it
    /// releases its file handle and its writer lock, then repeats the open, so a torn tail is
    /// cut off, the counter continues from the file, the entries are read back and the objects
    /// no entry names are swept. This is the way out of [`LedgerError::Poisoned`]. On failure
    /// the ledger is poisoned, holds neither the file nor the lock, keeps the entries it had in
    /// memory (so they still upload) and may be reopened again. A file that has vanished is
    /// [`LedgerError::Corrupt`] too: only the first open creates the file. Blocking.
    pub fn reopen(&mut self) -> Result<(), LedgerError> {
        self.poisoned = true;
        self.file = None;
        self.lock = None;
        let kept = (
            self.highest,
            std::mem::take(&mut self.entries),
            self.file_len,
            self.stored_bytes,
        );
        self.highest = 0;
        self.file_len = 0;
        self.stored_bytes = 0;
        if let Err(error) = self.open_state(false) {
            self.file = None;
            self.lock = None;
            self.poisoned = true;
            (self.highest, self.entries, self.file_len, self.stored_bytes) = kept;
            return Err(error);
        }
        self.poisoned = false;
        Ok(())
    }

    /// Makes the parsed file the ledger's state: cuts a torn tail off, makes the cut and the
    /// file's name durable, removes a stale temporary file. Nothing is swept here.
    fn load(&mut self, bytes: &[u8], parsed: Parsed) -> Result<(), LedgerError> {
        let file = open_rw(&self.path)?;
        if parsed.len < bytes.len() {
            tracing::warn!(
                cut = bytes.len() - parsed.len,
                "the upload ledger ends in a frame its append did not finish; cutting it off"
            );
            file.set_len(parsed.len as u64)?;
            (self.probe)(Step::AfterCut)?;
        }
        // A frame that landed after a failed sync may be only in the OS's cache; syncing here,
        // before the entries count as pending, also makes a cut durable.
        file.sync_all()?;
        // The file may be one whose rename never reached the disk; making it durable here,
        // before the open accepts writes, closes that gap. A failure fails the open.
        (self.probe)(Step::AfterRename)?;
        sync_after_rename(&self.dir, &self.path)?;
        match fs::remove_file(self.tmp_path()) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        self.file = Some(file);
        self.highest = parsed.highest;
        self.stored_bytes = stored_sum(&parsed.entries);
        self.entries = parsed.entries;
        self.file_len = parsed.len as u64;
        Ok(())
    }

    fn tmp_path(&self) -> PathBuf {
        self.dir.join(format!("{}{TMP_SUFFIX}", self.agent_id))
    }

    fn object_file(&self, seq: u64) -> PathBuf {
        self.objects.join(seq.to_string())
    }

    /// Deletes the files of the objects directory that no stored entry names. A file that
    /// cannot be deleted is logged and left for the next open; anything that is not a regular
    /// file is left alone. Returns how many were deleted.
    fn sweep(&self) -> usize {
        let named: HashSet<String> = self
            .entries
            .iter()
            .filter(|entry| entry.source == Source::Stored)
            .map(|entry| entry.seq.to_string())
            .collect();
        let Ok(listing) = fs::read_dir(&self.objects) else {
            tracing::warn!("the upload objects directory cannot be listed; nothing was swept");
            return 0;
        };
        let (mut removed, mut failed) = (0, 0);
        for item in listing.flatten() {
            if !item.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            if item
                .file_name()
                .to_str()
                .is_some_and(|name| named.contains(name))
            {
                continue;
            }
            match (self.probe)(Step::BeforeObjectDelete).and_then(|()| fs::remove_file(item.path()))
            {
                Ok(()) => removed += 1,
                Err(_) => failed += 1,
            }
        }
        if removed > 0 || failed > 0 {
            tracing::warn!(
                removed,
                failed,
                "swept stored uploads that no entry names; the ones that failed stay for the next open"
            );
        }
        removed
    }

    /// Stores `bytes` as the result of the job and makes its entry durable. Blocking (file
    /// syncs): the caller runs it on a blocking task. Returns the entry's number; the entry is
    /// pending from then on. The same job, generation and bytes again returns the number of the
    /// entry that is there and writes nothing; other bytes are [`LedgerError::Conflict`]. A
    /// refusal writes nothing and uses no number. A failed write of the object leaves the
    /// ledger usable; a failed append poisons it and keeps the object (the next open settles
    /// whether the entry landed).
    pub fn enqueue_result(&mut self, key: &JobKey, bytes: &[u8]) -> Result<u64, LedgerError> {
        self.check_writable()?;
        check_key(key)?;
        let length = bytes.len() as u64;
        if length > MAX_RESULT_BYTES {
            return Err(LedgerError::TooLarge);
        }
        let digest = Digest {
            length,
            sha256: Sha256::digest(bytes).into(),
        };
        if let Some(seq) = self.existing(key, Kind::Result, &digest)? {
            return Ok(seq);
        }
        self.add(key, Kind::Result, Source::Stored, digest, Some(bytes))
    }

    /// Records that the first `digest.length()` bytes of the file at `path` are the job's
    /// communication log, and makes the entry durable. The file is not opened. Duplicates,
    /// refusals and failures are as for [`UploadLedger::enqueue_result`].
    pub fn enqueue_comm_log(
        &mut self,
        key: &JobKey,
        path: &Path,
        digest: Digest,
    ) -> Result<u64, LedgerError> {
        self.check_writable()?;
        check_key(key)?;
        let source = path
            .to_str()
            .map(|text| Source::File(text.to_owned()))
            .filter(source_is_valid)
            .ok_or(LedgerError::BadSource)?;
        if digest.length > MAX_COMM_LOG_BYTES {
            return Err(LedgerError::TooLarge);
        }
        if let Some(seq) = self.existing(key, Kind::CommLog, &digest)? {
            return Ok(seq);
        }
        self.add(key, Kind::CommLog, source, digest, None)
    }

    fn check_writable(&self) -> Result<(), LedgerError> {
        if self.poisoned {
            Err(LedgerError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// The number of the entry of this job, generation and kind, when it has the same bytes;
    /// [`LedgerError::Conflict`] when it has other bytes.
    fn existing(
        &self,
        key: &JobKey,
        kind: Kind,
        digest: &Digest,
    ) -> Result<Option<u64>, LedgerError> {
        match self
            .entries
            .iter()
            .find(|entry| entry.key == *key && entry.kind == kind)
        {
            None => Ok(None),
            Some(entry) if entry.length == digest.length && entry.sha256 == digest.sha256 => {
                Ok(Some(entry.seq))
            }
            Some(_) => Err(LedgerError::Conflict),
        }
    }

    /// Gives the entry the next number, writes its object when it has one, then appends and
    /// syncs its frame.
    fn add(
        &mut self,
        key: &JobKey,
        kind: Kind,
        source: Source,
        digest: Digest,
        object: Option<&[u8]>,
    ) -> Result<u64, LedgerError> {
        // The number after the last entry must stay representable, since `parse` computes it;
        // so the highest number ever given is `u64::MAX - 1`.
        let seq = self
            .highest
            .checked_add(1)
            .filter(|seq| *seq < u64::MAX)
            .ok_or(LedgerError::Corrupt("the entry counter overflows"))?;
        let entry = Entry {
            seq,
            key: key.clone(),
            kind,
            source,
            length: digest.length,
            sha256: digest.sha256,
        };
        let frame = encode_frame(&entry)?;
        if self.file_len + frame.len() as u64 > MAX_LEDGER_BYTES
            || (object.is_some()
                && self.stored_bytes.saturating_add(digest.length) > MAX_STORED_BYTES)
        {
            return Err(LedgerError::Full);
        }
        if let Some(bytes) = object {
            self.write_object(seq, bytes)?;
        }
        if let Err(error) = self.write(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.highest = seq;
        self.file_len += frame.len() as u64;
        if object.is_some() {
            self.stored_bytes += digest.length;
        }
        self.entries.push(entry);
        Ok(seq)
    }

    /// Writes and syncs the object of entry `seq`, then its directory entry. A failure removes
    /// what was written, as far as it can, and leaves the ledger usable.
    fn write_object(&self, seq: u64, bytes: &[u8]) -> io::Result<()> {
        let path = self.object_file(seq);
        let written = (|| {
            (self.probe)(Step::BeforeObjectWrite)?;
            // A file under the fresh name is a leftover; a name that cannot be cleared fails.
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let mut file = create_private(&path)?;
            file.write_all(bytes)?;
            (self.probe)(Step::AfterObjectWrite)?;
            file.sync_all()?;
            sync_dir(&self.objects)?;
            (self.probe)(Step::AfterObjectSync)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&path);
        }
        written
    }

    fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the upload ledger file is closed"))?;
        (self.probe)(Step::BeforeWrite)?;
        file.seek(SeekFrom::Start(self.file_len))?;
        file.write_all(frame)?;
        (self.probe)(Step::AfterWrite)?;
        file.sync_all()?;
        (self.probe)(Step::AfterSync)
    }

    /// Whether the ledger refuses writes until [`UploadLedger::reopen`] succeeds.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The highest number assigned, 0 when none has been.
    pub fn highest(&self) -> u64 {
        self.highest
    }

    /// The entries to upload, in upload order: the results by number, then the communication
    /// logs by number. Served from memory, also when the ledger is poisoned.
    pub fn pending(&self) -> impl Iterator<Item = &Entry> + '_ {
        let of = |kind| self.entries.iter().filter(move |entry| entry.kind == kind);
        of(Kind::Result).chain(of(Kind::CommLog))
    }

    /// The entry to upload first, `None` when there is none.
    pub fn next(&self) -> Option<&Entry> {
        self.pending().next()
    }

    /// Where the bytes of `entry` are: its stored object, or the file it names.
    pub fn object_path(&self, entry: &Entry) -> PathBuf {
        match &entry.source {
            Source::Stored => self.object_file(entry.seq),
            Source::File(path) => PathBuf::from(path),
        }
    }

    /// Takes the entry `seq` out of the ledger once the server has it: the file is rewritten
    /// without the entry, then memory changes, then the stored object is deleted (a failed
    /// delete is logged and the next open sweeps it). A number at or below the highest that is
    /// not pending (it was finished before) changes nothing; one above it is
    /// [`LedgerError::OutOfRange`]. A failure before the rename leaves the old file and the
    /// entry in place and the ledger usable; a failure after it poisons the ledger, with the
    /// entry gone from memory and its object kept for the next open's sweep. A communication
    /// log's file is never touched.
    pub fn finished(&mut self, seq: u64) -> Result<(), LedgerError> {
        self.check_writable()?;
        if seq > self.highest {
            return Err(LedgerError::OutOfRange {
                seq,
                highest: self.highest,
            });
        }
        let Some(entry) = self.entries.iter().find(|entry| entry.seq == seq) else {
            return Ok(());
        };
        let stored = entry.source == Source::Stored;
        self.install(Some(seq))?;
        if stored {
            let path = self.object_file(seq);
            let deleted =
                (self.probe)(Step::BeforeObjectDelete).and_then(|()| {
                    match fs::remove_file(&path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                        _ => Ok(()),
                    }
                });
            if let Err(error) = deleted {
                tracing::warn!(kind = ?error.kind(), "deleting a finished upload's object failed; the next open sweeps it");
            }
        }
        Ok(())
    }

    /// Writes the file whole (the header, then the entries without `remove`) under a temporary
    /// name, syncs it and renames it over the ledger's file; the same routine creates the file.
    /// The old handle is closed before the rename, which Windows needs. A failure before the
    /// rename leaves the old file and memory as they were. From the rename on, memory is the
    /// new file's; the ledger is poisoned when the handle cannot be reopened (the same holds if
    /// the rename itself failed and the old file cannot be reopened) or when the sync after the
    /// rename fails (a rewrite only; the creation of the file returns the error and the open
    /// fails).
    fn install(&mut self, remove: Option<u64>) -> io::Result<()> {
        let kept: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| Some(entry.seq) != remove)
            .cloned()
            .collect();
        let tmp = self.tmp_path();
        let bytes = encode_file(&self.agent_id, self.highest, &kept)?;
        let prepared = (|| {
            match fs::remove_file(&tmp) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let mut file = create_private(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            (self.probe)(Step::BeforeRename)
        })();
        if let Err(error) = prepared {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        let had_file = self.file.take().is_some();
        if let Err(error) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            if had_file {
                self.file = open_rw(&self.path).ok();
                self.poisoned = self.file.is_none();
            }
            return Err(error);
        }
        self.stored_bytes = stored_sum(&kept);
        self.entries = kept;
        self.file_len = bytes.len() as u64;
        let synced = (self.probe)(Step::AfterRename)
            .and_then(|()| sync_after_rename(&self.dir, &self.path))
            .and_then(|()| (self.probe)(Step::AfterRenameSync));
        match open_rw(&self.path) {
            Ok(file) => self.file = Some(file),
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        }
        if let Err(error) = synced {
            self.poisoned = had_file;
            return Err(error);
        }
        Ok(())
    }
}

fn check_key(key: &JobKey) -> Result<(), LedgerError> {
    job_file_stem(key)
        .map(|_| ())
        .map_err(|_| LedgerError::BadJobId)
}

fn source_is_valid(source: &Source) -> bool {
    match source {
        Source::Stored => true,
        Source::File(path) => path.len() <= MAX_PATH_BYTES && Path::new(path).is_absolute(),
    }
}

fn stored_sum(entries: &[Entry]) -> u64 {
    entries
        .iter()
        .filter(|entry| entry.source == Source::Stored)
        .fold(0, |sum, entry| sum.saturating_add(entry.length))
}

/// How many entries of the upload directory belong to other agent IDs (their files, objects
/// directories and temporary files): everything but this agent's names and the lock.
fn other_entries(dir: &Path, agent_id: &str) -> usize {
    let own = [
        agent_id.to_owned(),
        format!("{agent_id}{TMP_SUFFIX}"),
        format!("{agent_id}{OBJECTS_SUFFIX}"),
        LOCK_FILE.to_owned(),
    ];
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| !own.iter().any(|name| e.file_name().to_str() == Some(name)))
            .count()
    })
}

fn encode_frame(entry: &Entry) -> io::Result<Vec<u8>> {
    let payload = postcard::to_allocvec(&Wire::from(entry)).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(FRAME_HEADER + payload.len());
    push_frame(&mut frame, &payload).map_err(|_| io::Error::other("an entry is too large"))?;
    Ok(frame)
}

fn encode_file(agent_id: &str, highest: u64, entries: &[Entry]) -> io::Result<Vec<u8>> {
    let header = Header {
        agent_id: agent_id.to_owned(),
        protection: 0,
        highest,
        retained: u32::try_from(entries.len()).map_err(io::Error::other)?,
    };
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    let header = postcard::to_allocvec(&header).map_err(io::Error::other)?;
    push_frame(&mut out, &header).map_err(|_| io::Error::other("the header is too large"))?;
    for entry in entries {
        out.extend_from_slice(&encode_frame(entry)?);
    }
    Ok(out)
}

/// An entry frame's payload, which must be one entry and nothing more.
fn decode_entry(payload: &[u8]) -> Option<Entry> {
    match postcard::take_from_bytes::<Wire>(payload) {
        Ok((wire, [])) => Some(wire.into()),
        _ => None,
    }
}

/// What an entry must satisfy wherever it is read.
fn check_entry(entry: &Entry) -> Result<(), &'static str> {
    if entry.seq == 0 {
        return Err("an entry has number 0");
    }
    if job_file_stem(&entry.key).is_err() {
        return Err("an entry has an invalid job ID");
    }
    if !source_is_valid(&entry.source) {
        return Err("an entry has an invalid path");
    }
    Ok(())
}

struct Parsed {
    highest: u64,
    entries: Vec<Entry>,
    /// The length up to the end of the last whole frame.
    len: usize,
}

/// Reads a whole file. The first `retained` entries must read back and have increasing numbers
/// up to `highest`; the entries after them must be numbered `highest + 1` on, and the last of
/// them may be one unfinished append, which is not part of the result. Anything else is corrupt.
fn parse(bytes: &[u8], agent_id: &str) -> Result<Parsed, LedgerError> {
    let corrupt = LedgerError::Corrupt;
    if bytes.len() < PREAMBLE || bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt("not an upload ledger"));
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
    let mut expected = header
        .highest
        .checked_add(1)
        .ok_or(corrupt("the entry counter overflows"))?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = next;
    let mut previous = 0;
    for _ in 0..header.retained {
        if offset >= bytes.len() {
            return Err(corrupt("the file holds fewer entries than it retained"));
        }
        let FrameRead::Ok { payload, next } = read_frame(bytes, offset) else {
            return Err(corrupt("a retained entry does not read back"));
        };
        let entry = decode_entry(payload).ok_or(corrupt("an entry does not decode"))?;
        check_entry(&entry).map_err(corrupt)?;
        if entry.seq <= previous || entry.seq > header.highest {
            return Err(corrupt("retained entry numbers are out of order or range"));
        }
        previous = entry.seq;
        push_unique(&mut entries, &mut seen, entry)?;
        offset = next;
    }
    let seq_of = |payload: &[u8]| decode_entry(payload).map(|entry| entry.seq);
    while offset < bytes.len() {
        match read_frame(bytes, offset) {
            FrameRead::Ok { payload, next } => {
                let entry = decode_entry(payload).ok_or(corrupt("an entry does not decode"))?;
                check_entry(&entry).map_err(corrupt)?;
                if entry.seq != expected {
                    return Err(corrupt("entry numbers are not contiguous"));
                }
                expected = expected
                    .checked_add(1)
                    .ok_or(corrupt("the entry counter overflows"))?;
                push_unique(&mut entries, &mut seen, entry)?;
                offset = next;
            }
            // One unfinished append leaves at most one frame and no entry after it; a damaged
            // length field or a zeroed block followed by whole entries is damage.
            FrameRead::Torn
                if bytes.len() - offset <= MAX_TORN
                    && !record_follows(bytes, offset, expected, seq_of) =>
            {
                break;
            }
            FrameRead::Torn => return Err(corrupt("an entry does not read back")),
            FrameRead::Bad => return Err(corrupt("an entry fails its checksum")),
        }
    }
    Ok(Parsed {
        highest: expected - 1,
        entries,
        len: offset,
    })
}

/// Adds an entry unless the job already has one of its generation and kind.
fn push_unique(
    entries: &mut Vec<Entry>,
    seen: &mut HashSet<(String, u64, Kind)>,
    entry: Entry,
) -> Result<(), LedgerError> {
    if !seen.insert((entry.key.job_id.0.clone(), entry.key.generation, entry.kind)) {
        return Err(LedgerError::Corrupt(
            "two entries have the same job and kind",
        ));
    }
    entries.push(entry);
    Ok(())
}

#[cfg(test)]
mod tests;

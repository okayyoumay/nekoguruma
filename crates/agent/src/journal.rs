//! Write-job journal (design 5.5, 8.2.5; ADR-229, ADR-244).
//!
//! One append-only file per job and ownership generation records what a write job has done:
//! the completed steps, the last confirmed block, the ECU identity, the per-stage resume counts
//! and the write-ahead intent markers. Each commit appends one checksummed frame and syncs the
//! file before it returns, so a crash or a power loss leaves every commit that returned `Ok`
//! readable. Reading the file back folds the records into [`RecoveryFacts`], which a handover's
//! checkpoint summary carries unchanged.
//!
//! Layout: the magic, the format version (`u32` LE), then frames of `len: u32 LE`,
//! `crc32: u32 LE` and a postcard payload of `len` bytes. The first frame is the [`JobKey`]
//! header; every later frame is one record.
//!
//! The journal only records. Committing a marker before the request it guards, and ending the
//! job when a commit fails, are the runner's duties (ADR-244).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use shared_proto::JobId;

/// The journal format this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The largest frame payload, in bytes. A record that would exceed it is refused.
pub const MAX_FRAME: u32 = 1 << 20;

const MAGIC: [u8; 8] = *b"NGRJRNL\0";
/// Magic and format version.
const PREAMBLE: usize = MAGIC.len() + 4;
/// Length and CRC in front of each payload.
const FRAME_HEADER: usize = 8;

/// The journal of one job under one ownership generation (ADR-229: a device that gets a job
/// back starts a new journal for the new generation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobKey {
    pub job_id: JobId,
    pub generation: u64,
}

/// A diagnostic primitive: `pc` is its instruction and `steps` is `VmState::steps` before it
/// runs, which keeps counting across resumes, so it orders steps even when a loop repeats `pc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRef {
    pub pc: u32,
    pub steps: u64,
}

/// A stage that has its own resume count. The IR gives it a meaning; the journal only keeps
/// stages apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StageId(pub u32);

/// The VIN a job targets. It is personal data (design 16.2): `Debug` hides it, there is no
/// `Display`, and callers use [`Vin::as_str`] only to compare. The journal keeps it as given;
/// `restart` decides whether it is well-formed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vin(String);

impl Vin {
    pub fn new(vin: String) -> Self {
        Self(vin)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Vin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Vin(<redacted>)")
    }
}

/// What a restart, or a device that takes the job over, needs to know from the journal
/// (ADR-229). It is the state the journal folds its records into, and the journal's part of the
/// checkpoint summary sent for handover, carried unchanged; the summary adds what the job itself
/// names (target VIN and ECU, the version being written, the stage reached; design 8.2.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryFacts {
    pub key: JobKey,
    /// The last completed step.
    pub last_step: Option<StepRef>,
    /// The hardware part number as the ECU answered it (the field's bytes, undecoded).
    pub ecu_hardware_part_number: Option<Vec<u8>>,
    /// The software version read before the first erase (the field's bytes, undecoded).
    pub pre_erase_software_version: Option<Vec<u8>>,
    /// Resumes made per stage, sorted by stage.
    pub resume_counts: Vec<(StageId, u16)>,
    /// The key of the current resume attempt; `None` for the original run.
    pub attempt_key: Option<Vec<u8>>,
    /// The latest transfer attempt.
    pub transfer: Option<TransferAttempt>,
    /// The newest request intent: a request at or past a plan's recovery-required point that is
    /// neither the erase nor RequestTransferExit, journaled before it is sent (ADR-253).
    pub last_intent: Option<StepRef>,
    /// The VIN the job targeted at its first run, recorded before any other record, so a resume
    /// can tell that the job's own data changed (ADR-261). `None` when the job named none.
    pub target_vin: Option<Vin>,
}

impl RecoveryFacts {
    fn new(key: JobKey) -> Self {
        Self {
            key,
            last_step: None,
            ecu_hardware_part_number: None,
            pre_erase_software_version: None,
            resume_counts: Vec::new(),
            attempt_key: None,
            transfer: None,
            last_intent: None,
            target_vin: None,
        }
    }

    /// The resumes made in `stage`.
    pub fn resume_count(&self, stage: StageId) -> u16 {
        self.resume_counts
            .iter()
            .find(|(id, _)| *id == stage)
            .map_or(0, |(_, count)| *count)
    }

    /// Applies `record`, or leaves the facts unchanged and says why it does not fit.
    fn apply(&mut self, record: &Record) -> Result<(), &'static str> {
        let after_last_step =
            |at: &StepRef, facts: &Self| facts.last_step.is_none_or(|last| at.steps > last.steps);
        match record {
            Record::TargetVin(vin) => {
                // Every other record changes the facts, so unchanged facts mean none came
                // before; a second target VIN finds the first.
                if *self != Self::new(self.key.clone()) {
                    return Err("the target VIN must be the first record, and at most one");
                }
                self.target_vin = Some(vin.clone());
            }
            Record::Step { at, .. } => {
                if !after_last_step(at, self) {
                    return Err("a step must come after the last step");
                }
                self.last_step = Some(*at);
                if let Some(transfer) = self.transfer.as_mut()
                    && !transfer.interrupted
                    && let Some(exit) = transfer.exit.as_mut()
                    && !exit.complete
                {
                    exit.last_post_step = Some(*at);
                }
            }
            Record::Block { block } => {
                let transfer = self
                    .open_transfer()
                    .ok_or("a block needs an open transfer")?;
                if transfer.exit.is_some() {
                    return Err("a block cannot follow RequestTransferExit");
                }
                if transfer.last_block.is_some_and(|last| *block <= last) {
                    return Err("blocks must increase");
                }
                transfer.last_block = Some(*block);
            }
            Record::EcuHardwarePartNumber(value) => {
                // A restart compares the ECU against this value, so it cannot change once the
                // ECU may hold a partial image.
                if self.transfer.is_some() && self.ecu_hardware_part_number.as_ref() != Some(value)
                {
                    return Err(
                        "the hardware part number cannot change after the transfer started",
                    );
                }
                self.ecu_hardware_part_number = Some(value.clone());
            }
            Record::PreEraseSoftwareVersion(value) => {
                if self.transfer.is_some() {
                    return Err("the pre-erase version must come before the transfer");
                }
                self.pre_erase_software_version = Some(value.clone());
            }
            Record::Resume {
                stage,
                count,
                attempt_key,
            } => {
                let next = self
                    .resume_count(*stage)
                    .checked_add(1)
                    .ok_or("the resume count is at its maximum")?;
                if *count != next {
                    return Err("a resume must count one more");
                }
                match self
                    .resume_counts
                    .binary_search_by_key(stage, |(id, _)| *id)
                {
                    Ok(index) => self.resume_counts[index].1 = next,
                    Err(index) => self.resume_counts.insert(index, (*stage, next)),
                }
                self.attempt_key = attempt_key.clone();
                // The attempt in progress ended with the interruption: steps from here on
                // belong to the recovery, not to its transfer or post-transfer steps.
                if let Some(transfer) = self.transfer.as_mut() {
                    transfer.interrupted = true;
                }
            }
            Record::TransferStart { stage, at } => {
                if !after_last_step(at, self) {
                    return Err("a marker must come after the last step");
                }
                // A new attempt starts clean: the previous exit marker and post-transfer
                // progress go with the previous attempt (ADR-229).
                self.transfer = Some(TransferAttempt {
                    stage: *stage,
                    started_at: *at,
                    last_block: None,
                    exit: None,
                    interrupted: false,
                });
            }
            Record::TransferExitIntent { at } => {
                if !after_last_step(at, self) {
                    return Err("a marker must come after the last step");
                }
                let transfer = self
                    .open_transfer()
                    .ok_or("RequestTransferExit needs an open transfer")?;
                if transfer.exit.is_some() {
                    return Err("RequestTransferExit is already marked");
                }
                transfer.exit = Some(TransferExit {
                    intent_at: *at,
                    last_post_step: None,
                    complete: false,
                });
            }
            Record::Intent { at } => {
                if !after_last_step(at, self) {
                    return Err("a marker must come after the last step");
                }
                if self.last_intent.is_some_and(|last| at.steps <= last.steps) {
                    return Err("intents must come in step order");
                }
                self.last_intent = Some(*at);
            }
            Record::PostTransferComplete => {
                let exit = self.open_transfer().and_then(|t| t.exit.as_mut()).ok_or(
                    "post-transfer completion needs RequestTransferExit in an open transfer",
                )?;
                if exit.complete {
                    return Err("the post-transfer steps are already complete");
                }
                exit.complete = true;
            }
        }
        Ok(())
    }

    /// The transfer attempt, unless a resume interrupted it.
    fn open_transfer(&mut self) -> Option<&mut TransferAttempt> {
        self.transfer
            .as_mut()
            .filter(|transfer| !transfer.interrupted)
    }
}

/// One transfer attempt, from its transfer-start marker on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferAttempt {
    pub stage: StageId,
    /// The erase or RequestDownload the marker was committed before.
    pub started_at: StepRef,
    /// The last confirmed block (progress only, ADR-229 item 3).
    pub last_block: Option<u32>,
    pub exit: Option<TransferExit>,
    /// A resume came after the marker: this attempt takes no more blocks or post-transfer
    /// progress, and only a new transfer-start marker opens another.
    pub interrupted: bool,
}

/// The RequestTransferExit marker and the post-transfer progress after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferExit {
    /// The RequestTransferExit the marker was committed before.
    pub intent_at: StepRef,
    /// The last step completed after the marker, until the post-transfer steps complete.
    pub last_post_step: Option<StepRef>,
    /// The post-transfer steps reached their end boundary.
    pub complete: bool,
}

/// Everything read back from a journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalState {
    pub facts: RecoveryFacts,
    /// The newest VM state a step record carried (postcard `VmState`, opaque here), with the
    /// step it was taken after.
    pub last_vm_state: Option<(StepRef, Vec<u8>)>,
    /// Records in the journal.
    pub records: u64,
}

/// One journal record. Variants are only ever appended: postcard encodes the variant index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Record {
    Step {
        at: StepRef,
        vm_state: Option<Vec<u8>>,
    },
    Block {
        block: u32,
    },
    EcuHardwarePartNumber(Vec<u8>),
    PreEraseSoftwareVersion(Vec<u8>),
    Resume {
        stage: StageId,
        count: u16,
        attempt_key: Option<Vec<u8>>,
    },
    TransferStart {
        stage: StageId,
        at: StepRef,
    },
    TransferExitIntent {
        at: StepRef,
    },
    PostTransferComplete,
    /// Appended for ADR-253.
    Intent {
        at: StepRef,
    },
    /// The job's target VIN; only before any other record.
    TargetVin(Vin),
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    key: JobKey,
    /// 0: none. MAC and encryption get their own values (design 5.5).
    protection: u8,
    created_unix_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    at_unix_ms: u64,
    record: Record,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("no journal for this job and generation")]
    NotFound,
    #[error("a journal for this job and generation already exists")]
    AlreadyExists,
    #[error("job ID {0:?} cannot name a journal file")]
    InvalidJobId(String),
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("an earlier commit failed; the journal takes no more records")]
    Poisoned,
    #[error("journal format version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("the journal belongs to another job or generation")]
    WrongJob,
    #[error("the journal is corrupt at byte {offset}: {reason}")]
    Corrupt { offset: u64, reason: &'static str },
    #[error("the record does not fit the journal: {0}")]
    Invariant(&'static str),
    #[error("the record is larger than a journal frame")]
    TooLarge,
    /// Another `Journal` of this job and generation is open for writing, in this process or
    /// another (ADR-255).
    #[error("the journal is open for writing elsewhere")]
    InUse,
}

/// Where commits go. A commit is durable once `append_sync` returns `Ok`.
pub trait Store {
    fn append_sync(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// The journal file, and the writer lock held for as long as it is open.
pub struct FileStore {
    file: File,
    len: u64,
    _writer: WriterLock,
}

/// An exclusive OS lock on the journal's sidecar `.journal.lock` file (ADR-255). It makes the
/// `Journal` that holds it the journal's only writer. The OS drops the lock with the file handle,
/// also when the process dies, so a crashed run never blocks the next one. The sidecar is
/// never deleted: a process that locked a recreated file would not exclude one still holding
/// the old one. It is a separate file so that [`Journal::read`] works while a job writes, also
/// where file locks are mandatory (Windows).
struct WriterLock(#[expect(dead_code, reason = "held only for its lock")] File);

impl WriterLock {
    fn take(journal: &Path) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(journal.with_extension("journal.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(JournalError::InUse),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Store for FileStore {
    fn append_sync(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(bytes)?;
        self.file.sync_all()?;
        self.len += bytes.len() as u64;
        Ok(())
    }
}

/// An open journal. Every `commit_*` returns only after the record is durable, and changes
/// [`Journal::state`] only then. After a failed write the journal is poisoned: a failed sync
/// can drop data the OS had accepted, so a later commit could claim what was lost.
pub struct Journal<S = FileStore> {
    store: S,
    state: JournalState,
    poisoned: bool,
}

impl Journal<FileStore> {
    /// Creates the journal of `key` in `dir`. The file appears complete or not at all: the
    /// header is written and synced under a temporary name, then linked to the journal's name,
    /// which fails if that name exists. The journal's writer lock is taken first, so another
    /// writer of the same job gets [`JournalError::InUse`]. The job's `target_vin`, when it names
    /// one, is the first record and is part of the same write, so the journal exists with it or
    /// not at all.
    pub fn create(
        dir: &Path,
        key: &JobKey,
        target_vin: Option<&Vin>,
    ) -> Result<Self, JournalError> {
        let path = journal_path(dir, key)?;
        let writer = WriterLock::take(&path)?;
        let tmp = path.with_extension("journal.tmp");
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        let header = Header {
            key: key.clone(),
            protection: 0,
            created_unix_ms: unix_ms(),
        };
        push_frame(&mut bytes, &encode(&header)?)?;
        let mut facts = RecoveryFacts::new(key.clone());
        let mut records = 0;
        if let Some(vin) = target_vin {
            let record = Record::TargetVin(vin.clone());
            facts.apply(&record).map_err(JournalError::Invariant)?;
            let entry = Entry {
                seq: 0,
                at_unix_ms: unix_ms(),
                record,
            };
            push_frame(&mut bytes, &encode(&entry)?)?;
            records = 1;
        }
        // A temporary file left by an earlier attempt that crashed holds no commits.
        match fs::remove_file(&tmp) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let written = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        let linked = fs::hard_link(&tmp, &path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(JournalError::AlreadyExists);
            }
            Err(error) => return Err(error.into()),
        }
        sync_dir(dir)?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        // Where the directory cannot be synced, the file's own sync covers its entry.
        file.sync_all()?;
        Ok(Self {
            store: FileStore {
                file,
                len: bytes.len() as u64,
                _writer: writer,
            },
            state: JournalState {
                facts,
                last_vm_state: None,
                records,
            },
            poisoned: false,
        })
    }

    /// Opens the journal of `key` in `dir` and reads it back. A frame left half-written by a
    /// commit that never returned is cut off; anything else that does not read back is
    /// [`JournalError::Corrupt`].
    ///
    /// Only the writer of a journal opens it: the journal's writer lock is taken before the file
    /// is read, so a second writer of the same job gets [`JournalError::InUse`] and never cuts off
    /// a frame the first one is writing (ADR-255). Other readers use [`Journal::read`], which
    /// takes no lock. A missing journal is [`JournalError::NotFound`] and leaves no lock file.
    pub fn open(dir: &Path, key: &JobKey) -> Result<Self, JournalError> {
        let path = journal_path(dir, key)?;
        match fs::metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(JournalError::NotFound);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let writer = WriterLock::take(&path)?;
        let (file, bytes) = read_file(&path, true)?;
        let loaded = load(&bytes, key)?;
        if loaded.len < bytes.len() {
            tracing::warn!(
                path = %path.display(),
                cut = bytes.len() - loaded.len,
                "journal ends in a frame its commit did not finish; cutting it off"
            );
            file.set_len(loaded.len as u64)?;
            file.sync_all()?;
        }
        Ok(Self {
            store: FileStore {
                file,
                len: loaded.len as u64,
                _writer: writer,
            },
            state: loaded.state,
            poisoned: false,
        })
    }
}

impl Journal {
    /// Reads the journal of `key` in `dir` without changing it: a half-written last frame is
    /// left in place and not counted. For readers other than the journal's writer (a summary,
    /// an audit upload).
    pub fn read(dir: &Path, key: &JobKey) -> Result<JournalState, JournalError> {
        let (_, bytes) = read_file(&journal_path(dir, key)?, false)?;
        Ok(load(&bytes, key)?.state)
    }
}

/// Opens `path` and reads it whole.
fn read_file(path: &Path, write: bool) -> Result<(File, Vec<u8>), JournalError> {
    let mut file = match OpenOptions::new().read(true).write(write).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(JournalError::NotFound);
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((file, bytes))
}

impl<S: Store> Journal<S> {
    /// A new, empty journal of `key` on `store`, for tests of the code that commits to it.
    #[cfg(test)]
    pub(crate) fn on_store(store: S, key: JobKey) -> Self {
        Self {
            store,
            state: JournalState {
                facts: RecoveryFacts::new(key),
                last_vm_state: None,
                records: 0,
            },
            poisoned: false,
        }
    }

    /// Records the target VIN on a journal made by `on_store`, for tests.
    #[cfg(test)]
    pub(crate) fn commit_target_vin(&mut self, vin: &Vin) -> Result<(), JournalError> {
        self.commit(Record::TargetVin(vin.clone()))
    }

    pub fn state(&self) -> &JournalState {
        &self.state
    }

    /// The checkpoint summary for handover: the facts, unchanged.
    pub fn summary(&self) -> RecoveryFacts {
        self.state.facts.clone()
    }

    /// Records a completed step and, optionally, the VM state after it.
    pub fn commit_step(
        &mut self,
        at: StepRef,
        vm_state: Option<&[u8]>,
    ) -> Result<(), JournalError> {
        // Refused before it is copied: a state this large cannot fit a frame.
        if vm_state.is_some_and(|state| state.len() > MAX_FRAME as usize) {
            return Err(JournalError::TooLarge);
        }
        self.commit(Record::Step {
            at,
            vm_state: vm_state.map(<[u8]>::to_vec),
        })
    }

    /// Records a block the ECU confirmed, counted from the start of the transfer.
    pub fn commit_block(&mut self, block: u32) -> Result<(), JournalError> {
        self.commit(Record::Block { block })
    }

    pub fn commit_ecu_hardware_part_number(&mut self, value: &[u8]) -> Result<(), JournalError> {
        self.commit(Record::EcuHardwarePartNumber(value.to_vec()))
    }

    /// Records the software version read before the erase; refused once a transfer started.
    pub fn commit_pre_erase_software_version(&mut self, value: &[u8]) -> Result<(), JournalError> {
        self.commit(Record::PreEraseSoftwareVersion(value.to_vec()))
    }

    /// Counts a resume of `stage` and records the attempt's key with it in one record, and
    /// returns the new count. Committed before the resume's first request to the ECU.
    pub fn commit_resume(
        &mut self,
        stage: StageId,
        attempt_key: Option<&[u8]>,
    ) -> Result<u16, JournalError> {
        let count =
            self.state
                .facts
                .resume_count(stage)
                .checked_add(1)
                .ok_or(JournalError::Invariant(
                    "the resume count is at its maximum",
                ))?;
        self.commit(Record::Resume {
            stage,
            count,
            attempt_key: attempt_key.map(<[u8]>::to_vec),
        })?;
        Ok(count)
    }

    /// The transfer-start marker, committed before the first erase or RequestDownload of an
    /// attempt is sent. It clears the previous attempt's exit marker and post-transfer progress.
    pub fn commit_transfer_start(
        &mut self,
        stage: StageId,
        at: StepRef,
    ) -> Result<(), JournalError> {
        self.commit(Record::TransferStart { stage, at })
    }

    /// The RequestTransferExit marker, committed before the request is sent.
    pub fn commit_transfer_exit_intent(&mut self, at: StepRef) -> Result<(), JournalError> {
        self.commit(Record::TransferExitIntent { at })
    }

    /// Records that the post-transfer steps reached their end boundary.
    pub fn commit_post_transfer_complete(&mut self) -> Result<(), JournalError> {
        self.commit(Record::PostTransferComplete)
    }

    /// A request intent, committed before a request at or past a plan's recovery-required point
    /// is sent, so a crash before its response still places the interruption at it (ADR-253).
    pub fn commit_intent(&mut self, at: StepRef) -> Result<(), JournalError> {
        self.commit(Record::Intent { at })
    }

    fn commit(&mut self, record: Record) -> Result<(), JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        // The target VIN is the journal's first record, by count.
        if matches!(record, Record::TargetVin(_)) && self.state.records != 0 {
            return Err(JournalError::Invariant(
                "the target VIN must be the first record",
            ));
        }
        let mut facts = self.state.facts.clone();
        facts.apply(&record).map_err(JournalError::Invariant)?;
        let entry = Entry {
            seq: self.state.records,
            at_unix_ms: unix_ms(),
            record,
        };
        let mut frame = Vec::new();
        push_frame(&mut frame, &encode(&entry)?)?;
        if let Err(error) = self.store.append_sync(&frame) {
            self.poisoned = true;
            return Err(error.into());
        }
        self.state.facts = facts;
        if let Record::Step {
            at,
            vm_state: Some(vm_state),
        } = entry.record
        {
            self.state.last_vm_state = Some((at, vm_state));
        }
        self.state.records += 1;
        Ok(())
    }
}

/// `{job_id}.g{generation}.journal` in `dir`. The job ID must be a plain name (a UUID is).
fn journal_path(dir: &Path, key: &JobKey) -> Result<PathBuf, JournalError> {
    let id = &key.job_id.0;
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(JournalError::InvalidJobId(id.clone()));
    }
    Ok(dir.join(format!("{id}.g{}.journal", key.generation)))
}

/// Makes a new directory entry durable. NTFS logs the entry with the file's metadata, which
/// the file's own sync covers.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, JournalError> {
    postcard::to_allocvec(value).map_err(|_| JournalError::TooLarge)
}

fn push_frame(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), JournalError> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or(JournalError::TooLarge)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// How a frame at some offset reads.
enum FrameRead<'a> {
    Ok {
        payload: &'a [u8],
        next: usize,
    },
    /// What a commit that never finished can leave: the frame runs past the end of the file,
    /// or it is the last thing in the file and fails its checksum (zeros or old data where the
    /// write did not land).
    Torn,
    /// A complete frame that fails its checksum with more bytes after it, which no single
    /// unfinished commit leaves.
    Bad,
}

fn read_frame(bytes: &[u8], offset: usize) -> FrameRead<'_> {
    let rest = &bytes[offset..];
    let Some((head, body)) = rest.split_first_chunk::<FRAME_HEADER>() else {
        return FrameRead::Torn;
    };
    let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let crc = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if len == 0 || len > MAX_FRAME || len as usize > body.len() {
        return FrameRead::Torn;
    }
    let payload = &body[..len as usize];
    let next = offset + FRAME_HEADER + payload.len();
    if crc32(payload) == crc {
        FrameRead::Ok { payload, next }
    } else if next == bytes.len() {
        FrameRead::Torn
    } else {
        FrameRead::Bad
    }
}

struct Loaded {
    state: JournalState,
    /// Bytes up to the end of the last whole frame.
    len: usize,
}

fn load(bytes: &[u8], key: &JobKey) -> Result<Loaded, JournalError> {
    let corrupt = |offset: usize, reason| JournalError::Corrupt {
        offset: offset as u64,
        reason,
    };
    if bytes.len() < PREAMBLE || bytes[..MAGIC.len()] != MAGIC {
        return Err(corrupt(0, "not a journal"));
    }
    let version = u32::from_le_bytes(bytes[MAGIC.len()..PREAMBLE].try_into().expect("four bytes"));
    if version != FORMAT_VERSION {
        return Err(JournalError::UnsupportedVersion(version));
    }
    // `create` makes the header durable before the file has its name, so it is never torn.
    let FrameRead::Ok { payload, next } = read_frame(bytes, PREAMBLE) else {
        return Err(corrupt(PREAMBLE, "the header does not read back"));
    };
    let header: Header = postcard::from_bytes(payload)
        .map_err(|_| corrupt(PREAMBLE, "the header does not decode"))?;
    if header.key != *key {
        return Err(JournalError::WrongJob);
    }
    if header.protection != 0 {
        return Err(corrupt(PREAMBLE, "unknown protection"));
    }
    let mut state = JournalState {
        facts: RecoveryFacts::new(header.key),
        last_vm_state: None,
        records: 0,
    };
    let mut offset = next;
    while offset < bytes.len() {
        let (payload, next) = match read_frame(bytes, offset) {
            FrameRead::Ok { payload, next } => (payload, next),
            // One unfinished commit leaves at most one frame, and no record after it.
            FrameRead::Torn
                if bytes.len() - offset <= FRAME_HEADER + MAX_FRAME as usize
                    && !record_follows(bytes, offset, state.records) =>
            {
                break;
            }
            FrameRead::Torn => return Err(corrupt(offset, "a record does not read back")),
            FrameRead::Bad => return Err(corrupt(offset, "a record fails its checksum")),
        };
        let entry: Entry = postcard::from_bytes(payload)
            .map_err(|_| corrupt(offset, "a record does not decode"))?;
        if entry.seq != state.records {
            return Err(corrupt(offset, "a record is out of sequence"));
        }
        if matches!(entry.record, Record::TargetVin(_)) && state.records != 0 {
            return Err(corrupt(offset, "the target VIN must be the first record"));
        }
        state
            .facts
            .apply(&entry.record)
            .map_err(|reason| corrupt(offset, reason))?;
        if let Record::Step {
            at,
            vm_state: Some(vm_state),
        } = entry.record
        {
            state.last_vm_state = Some((at, vm_state));
        }
        state.records += 1;
        offset = next;
    }
    Ok(Loaded { state, len: offset })
}

/// Whether committed records numbered `records` or later follow the frame at `torn` that does
/// not read back: a run of whole records with consecutive numbers after it. A damaged length
/// field or a zeroed region reads like a torn frame, but records committed after it show that
/// it is damage, not an unfinished commit.
///
/// A frame can also lie inside a torn record's payload (a VM state can hold any bytes). Inside
/// the extent the torn frame's own length claims, a run therefore counts only if it ends
/// exactly at the end of the file, which a write cut inside a payload does not leave unless it
/// stops exactly on a nested frame's end. Beyond that extent, a run counts however it ends, so
/// a newest commit torn after earlier damage does not hide the records in between.
fn record_follows(bytes: &[u8], torn: usize, records: u64) -> bool {
    let claimed_end = bytes[torn..]
        .first_chunk::<4>()
        .map(|len| u32::from_le_bytes(*len))
        .filter(|len| (1..=MAX_FRAME).contains(len))
        .map_or(torn + 1, |len| torn + FRAME_HEADER + len as usize);
    (torn + 1..bytes.len()).any(|start| {
        let mut offset = start;
        let mut expected = None;
        while let FrameRead::Ok { payload, next } = read_frame(bytes, offset) {
            let Ok(entry) = postcard::from_bytes::<Entry>(payload) else {
                break;
            };
            if entry.seq < records || expected.is_some_and(|seq| entry.seq != seq) {
                break;
            }
            let Some(following) = entry.seq.checked_add(1) else {
                break;
            };
            expected = Some(following);
            offset = next;
        }
        expected.is_some() && (offset == bytes.len() || start >= claimed_end)
    })
}

/// CRC-32 with the reflected polynomial 0xEDB88320, initial value and final XOR all ones (the
/// checksum zlib's `crc32` computes).
fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut crc = index as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    0xEDB8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[index] = crc;
            index += 1;
        }
        table
    };
    !bytes.iter().fold(!0u32, |crc, byte| {
        TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ngr-journal-{nanos}-{}",
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn key() -> JobKey {
        JobKey {
            job_id: JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e6f".to_owned()),
            generation: 1,
        }
    }

    fn at(pc: u32, steps: u64) -> StepRef {
        StepRef { pc, steps }
    }

    const STAGE: StageId = StageId(1);

    /// Every kind of commit, in a write job's order, with a second attempt at the end.
    fn script<S: Store>(journal: &mut Journal<S>, mut after_each: impl FnMut(&Journal<S>)) {
        type Commit<S> = fn(&mut Journal<S>) -> Result<(), JournalError>;
        let commits: [Commit<S>; 14] = [
            |j| j.commit_ecu_hardware_part_number(b"HW-1"),
            |j| j.commit_pre_erase_software_version(b"1.0.0"),
            |j| j.commit_step(at(2, 10), Some(b"vm-1")),
            |j| j.commit_transfer_start(STAGE, at(3, 11)),
            |j| j.commit_step(at(3, 11), None),
            |j| j.commit_block(0),
            |j| j.commit_block(1),
            |j| j.commit_transfer_exit_intent(at(5, 20)),
            |j| j.commit_step(at(5, 20), None),
            |j| j.commit_post_transfer_complete(),
            |j| j.commit_resume(STAGE, Some(b"attempt-1")).map(drop),
            |j| j.commit_transfer_start(STAGE, at(3, 30)),
            |j| j.commit_step(at(3, 30), Some(b"vm-2")),
            |j| j.commit_block(0),
        ];
        for commit in commits {
            commit(journal).expect("the script commits");
            after_each(journal);
        }
    }

    fn path(dir: &TempDir) -> PathBuf {
        journal_path(&dir.0, &key()).expect("valid key")
    }

    #[test]
    fn crc32_matches_the_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn reads_back_what_it_committed() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        script(&mut journal, |_| {});
        let (state, summary) = (journal.state().clone(), journal.summary());
        drop(journal);
        let reopened = Journal::open(&dir.0, &key()).expect("open");
        assert_eq!(reopened.state(), &state);
        assert_eq!(reopened.summary(), summary);
        let facts = reopened.summary();
        assert_eq!(facts.resume_counts, [(STAGE, 1)]);
        assert_eq!(facts.attempt_key.as_deref(), Some(&b"attempt-1"[..]));
        assert_eq!(
            facts.transfer,
            Some(TransferAttempt {
                stage: STAGE,
                started_at: at(3, 30),
                last_block: Some(0),
                exit: None,
                interrupted: false,
            })
        );
        assert_eq!(
            reopened.state().last_vm_state,
            Some((at(3, 30), b"vm-2".to_vec()))
        );
        assert_eq!(reopened.state().records, 14);
    }

    /// Every prefix of the file reads back as the state after its last whole frame: the
    /// prefixes are what a crash during an append can leave.
    #[test]
    fn every_cut_reads_back_as_the_last_whole_commit() {
        let vin = Vin::new("WDB12345678901234".to_owned());
        for target_vin in [None, Some(&vin)] {
            let dir = TempDir::new();
            let mut journal = Journal::create(&dir.0, &key(), target_vin).expect("create");
            let mut boundaries = vec![(journal.store.len as usize, journal.state().clone())];
            script(&mut journal, |j| {
                boundaries.push((j.store.len as usize, j.state().clone()));
            });
            let bytes = fs::read(path(&dir)).expect("read");
            assert_eq!(bytes.len(), boundaries.last().expect("some").0);
            if target_vin.is_some() {
                // A cut inside the VIN frame leaves the header alone: an empty journal.
                let FrameRead::Ok { next, .. } = read_frame(&bytes, PREAMBLE) else {
                    panic!("header");
                };
                let empty = JournalState {
                    facts: RecoveryFacts::new(key()),
                    last_vm_state: None,
                    records: 0,
                };
                boundaries.insert(0, (next, empty));
            }
            for cut in 0..=bytes.len() {
                let loaded = load(&bytes[..cut], &key());
                match boundaries.iter().rev().find(|(len, _)| *len <= cut) {
                    None => assert!(
                        matches!(loaded, Err(JournalError::Corrupt { .. })),
                        "cut {cut}"
                    ),
                    Some((len, state)) => {
                        let loaded = loaded.unwrap_or_else(|error| panic!("cut {cut}: {error}"));
                        assert_eq!((loaded.len, &loaded.state), (*len, state), "cut {cut}");
                        // Past the VIN frame, the VIN survives every cut.
                        if target_vin.is_some() && state.records > 0 {
                            assert_eq!(loaded.state.facts.target_vin.as_ref(), target_vin);
                        }
                    }
                }
            }
        }
    }

    /// Documented behaviour: a power loss that leaves the VIN frame as the last frame with its
    /// payload unwritten reads back as an empty journal, with no VIN. No later record was
    /// durable, so no transfer can have started (ADR-261).
    #[test]
    fn a_torn_target_vin_frame_reads_back_as_an_empty_journal() {
        let dir = TempDir::new();
        let vin = Vin::new("WDB12345678901234".to_owned());
        drop(Journal::create(&dir.0, &key(), Some(&vin)).expect("create"));
        let mut bytes = fs::read(path(&dir)).expect("read");
        let FrameRead::Ok { next, .. } = read_frame(&bytes, PREAMBLE) else {
            panic!("header");
        };
        bytes[next + FRAME_HEADER..].fill(0);
        let loaded = load(&bytes, &key()).expect("a torn tail reads back");
        assert_eq!(loaded.len, next);
        assert_eq!(loaded.state.facts.target_vin, None);
        assert_eq!(loaded.state.records, 0);
    }

    #[test]
    fn a_torn_tail_is_cut_off_and_the_journal_continues() {
        for tail in [vec![0u8; 64], vec![0xA5; 3], {
            // A whole frame whose payload did not land.
            let mut frame = Vec::new();
            push_frame(&mut frame, &[1, 2, 3, 4]).expect("frame");
            frame[FRAME_HEADER..].fill(0);
            frame
        }] {
            let dir = TempDir::new();
            let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
            journal.commit_block(0).expect_err("no transfer yet");
            journal
                .commit_ecu_hardware_part_number(b"HW-1")
                .expect("commit");
            let committed = journal.state().clone();
            drop(journal);
            let mut file = OpenOptions::new()
                .append(true)
                .open(path(&dir))
                .expect("open");
            file.write_all(&tail).expect("append");
            drop(file);

            let mut journal = Journal::open(&dir.0, &key()).expect("open");
            assert_eq!(journal.state(), &committed, "{tail:02X?}");
            journal
                .commit_pre_erase_software_version(b"1.0.0")
                .expect("commit after the cut");
            let state = journal.state().clone();
            drop(journal);
            let reopened = Journal::open(&dir.0, &key()).expect("reopen");
            assert_eq!(reopened.state(), &state);
        }
    }

    #[test]
    fn a_damaged_record_with_records_after_it_is_corrupt() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        let header_end = journal.store.len as usize;
        script(&mut journal, |_| {});
        drop(journal);
        let mut bytes = fs::read(path(&dir)).expect("read");
        // The first record's payload.
        bytes[header_end + FRAME_HEADER] ^= 0x40;
        fs::write(path(&dir), &bytes).expect("write");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::Corrupt { offset, .. }) if offset == header_end as u64
        ));
    }

    #[test]
    fn refuses_a_file_it_cannot_trust() {
        let dir = TempDir::new();
        drop(Journal::create(&dir.0, &key(), None).expect("create"));
        let good = fs::read(path(&dir)).expect("read");

        let other = JobKey {
            generation: 2,
            ..key()
        };
        fs::copy(path(&dir), journal_path(&dir.0, &other).expect("key")).expect("copy");
        assert!(matches!(
            Journal::open(&dir.0, &other),
            Err(JournalError::WrongJob)
        ));

        let mut newer = good.clone();
        newer[MAGIC.len()..PREAMBLE].copy_from_slice(&2u32.to_le_bytes());
        fs::write(path(&dir), &newer).expect("write");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::UnsupportedVersion(2))
        ));

        let mut foreign = good.clone();
        foreign[0] = b'X';
        fs::write(path(&dir), &foreign).expect("write");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::Corrupt { offset: 0, .. })
        ));

        // A header cut short is not a torn tail: `create` never leaves one.
        fs::write(path(&dir), &good[..good.len() - 1]).expect("write");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::Corrupt { .. })
        ));
    }

    #[test]
    fn create_and_open_check_the_file() {
        let dir = TempDir::new();
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::NotFound)
        ));
        drop(Journal::create(&dir.0, &key(), None).expect("create"));
        assert!(matches!(
            Journal::create(&dir.0, &key(), None),
            Err(JournalError::AlreadyExists)
        ));
        // The journal and its writer lock's sidecar; no temporary file is left.
        let mut names: Vec<_> = fs::read_dir(&dir.0)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                path(&dir).file_name().expect("name").to_owned(),
                path(&dir)
                    .with_extension("journal.lock")
                    .file_name()
                    .expect("name")
                    .to_owned(),
            ]
        );
        for id in ["", "../x", "a/b", "a.b", "a\\b"] {
            let key = JobKey {
                job_id: JobId(id.to_owned()),
                generation: 0,
            };
            assert!(
                matches!(
                    Journal::create(&dir.0, &key, None),
                    Err(JournalError::InvalidJobId(_))
                ),
                "{id:?}"
            );
        }
    }

    #[test]
    fn the_target_vin_is_the_first_record_and_reads_back() {
        let dir = TempDir::new();
        let vin = Vin::new("WDB12345678901234".to_owned());
        let mut j = Journal::create(&dir.0, &key(), Some(&vin)).expect("create");
        assert_eq!(j.state().facts.target_vin.as_ref(), Some(&vin));
        assert_eq!(j.state().records, 1);
        // A second target VIN is refused.
        assert!(matches!(
            j.commit_target_vin(&vin),
            Err(JournalError::Invariant(_))
        ));
        j.commit_step(at(1, 1), None).expect("step");
        drop(j);
        let state = Journal::read(&dir.0, &key()).expect("read back");
        assert_eq!(state.facts.target_vin, Some(vin.clone()));
        assert_eq!(state.records, 2);
        // No VIN in any debug output.
        assert!(!format!("{state:?} {:?}", state.facts).contains("WDB"));

        // A journal made without one has none.
        let dir = TempDir::new();
        drop(Journal::create(&dir.0, &key(), None).expect("create"));
        assert_eq!(
            Journal::read(&dir.0, &key())
                .expect("read")
                .facts
                .target_vin,
            None
        );
    }

    #[test]
    fn a_target_vin_after_another_record_is_refused() {
        let vin = Vin::new("WDB12345678901234".to_owned());
        let mut j = memory();
        j.commit_step(at(1, 1), None).expect("step");
        assert!(matches!(
            j.commit_target_vin(&vin),
            Err(JournalError::Invariant(_))
        ));
        // Not tied to the first sequence number: a journal that starts with it takes no second.
        let mut j = memory();
        j.commit_target_vin(&vin).expect("first");
        assert!(matches!(
            j.commit_target_vin(&vin),
            Err(JournalError::Invariant(_))
        ));
        j.commit_step(at(1, 1), None).expect("step");
    }

    /// A file whose second record is a target VIN does not read back.
    #[test]
    fn a_file_with_a_late_target_vin_is_corrupt() {
        let dir = TempDir::new();
        let vin = Vin::new("WDB12345678901234".to_owned());
        let mut j = Journal::create(&dir.0, &key(), None).expect("create");
        j.commit_step(at(1, 1), None).expect("step");
        drop(j);
        let mut frame = Vec::new();
        let entry = Entry {
            seq: 1,
            at_unix_ms: 0,
            record: Record::TargetVin(vin),
        };
        push_frame(&mut frame, &encode(&entry).expect("encode")).expect("frame");
        let mut file = OpenOptions::new()
            .append(true)
            .open(path(&dir))
            .expect("open");
        file.write_all(&frame).expect("append");
        drop(file);
        assert!(matches!(
            Journal::read(&dir.0, &key()),
            Err(JournalError::Corrupt { .. })
        ));
    }

    #[test]
    fn records_out_of_order_are_refused() {
        let dir = TempDir::new();
        let mut j = Journal::create(&dir.0, &key(), None).expect("create");
        let refused = |result: Result<(), JournalError>| {
            assert!(
                matches!(result, Err(JournalError::Invariant(_))),
                "{result:?}"
            );
        };
        refused(j.commit_block(0));
        refused(j.commit_transfer_exit_intent(at(1, 1)));
        refused(j.commit_post_transfer_complete());
        j.commit_step(at(1, 5), None).expect("step");
        refused(j.commit_step(at(1, 5), None));
        refused(j.commit_transfer_start(STAGE, at(2, 4)));
        j.commit_transfer_start(STAGE, at(2, 6)).expect("start");
        refused(j.commit_pre_erase_software_version(b"1.0.0"));
        j.commit_block(3).expect("block");
        refused(j.commit_block(3));
        j.commit_transfer_exit_intent(at(3, 7)).expect("exit");
        refused(j.commit_block(4));
        refused(j.commit_transfer_exit_intent(at(3, 8)));
        j.commit_post_transfer_complete().expect("complete");
        refused(j.commit_post_transfer_complete());
        // A step after completion is not post-transfer progress.
        j.commit_step(at(4, 9), None).expect("step");
        let exit = j.summary().transfer.and_then(|t| t.exit).expect("exit");
        assert_eq!((exit.last_post_step, exit.complete), (None, true));

        let refused_state = j.state().clone();
        let records = j.state().records;
        drop(j);
        let reopened = Journal::open(&dir.0, &key()).expect("open");
        assert_eq!(reopened.state(), &refused_state);
        assert_eq!(reopened.state().records, records);
    }

    #[test]
    fn the_resume_count_stops_at_its_maximum() {
        let mut facts = RecoveryFacts::new(key());
        facts.resume_counts = vec![(STAGE, u16::MAX)];
        let mut j = Journal {
            store: Memory::default(),
            state: JournalState {
                facts,
                last_vm_state: None,
                records: 0,
            },
            poisoned: false,
        };
        assert!(matches!(
            j.commit_resume(STAGE, None),
            Err(JournalError::Invariant(_))
        ));
        assert_eq!(j.commit_resume(StageId(2), None).expect("resume"), 1);
        assert!(j.store.0.len() == 1);
    }

    #[test]
    fn a_new_transfer_start_clears_the_exit_marker_and_post_transfer_progress() {
        let mut j = memory();
        j.commit_resume(STAGE, None).expect("resume");
        j.commit_transfer_start(STAGE, at(3, 1)).expect("start");
        j.commit_block(0).expect("block");
        j.commit_transfer_exit_intent(at(5, 2)).expect("exit");
        j.commit_step(at(5, 2), None).expect("post step");
        assert!(j.summary().transfer.and_then(|t| t.exit).is_some());
        j.commit_transfer_start(STAGE, at(3, 3)).expect("restart");
        let facts = j.summary();
        assert_eq!(
            facts.transfer,
            Some(TransferAttempt {
                stage: STAGE,
                started_at: at(3, 3),
                last_block: None,
                exit: None,
                interrupted: false,
            })
        );
        assert_eq!(facts.resume_counts, [(STAGE, 1)]);
        assert_eq!(facts.last_step, Some(at(5, 2)));
    }

    /// A damaged length field or a zeroed region reads like a torn frame, but the records
    /// committed after it must not be cut off with it.
    #[test]
    fn damage_with_records_after_it_is_not_a_torn_tail() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        let header_end = journal.store.len as usize;
        journal
            .commit_ecu_hardware_part_number(b"HW-1")
            .expect("commit");
        let second = journal.store.len as usize;
        journal
            .commit_transfer_start(STAGE, at(3, 11))
            .expect("commit");
        let third = journal.store.len as usize;
        journal.commit_block(0).expect("commit");
        journal
            .commit_transfer_exit_intent(at(5, 20))
            .expect("commit");
        drop(journal);
        let good = fs::read(path(&dir)).expect("read");

        let mut damaged = Vec::new();
        // One bit of the transfer-start frame's length.
        let mut bytes = good.clone();
        bytes[second] ^= 0x01;
        damaged.push(bytes);
        // Its length zeroed.
        let mut bytes = good.clone();
        bytes[second..second + 4].fill(0);
        damaged.push(bytes);
        // A length larger than a frame can be.
        let mut bytes = good.clone();
        bytes[second..second + 4].fill(0xFF);
        damaged.push(bytes);
        // Two whole records zeroed.
        let mut bytes = good.clone();
        bytes[header_end..third].fill(0);
        damaged.push(bytes);
        for (index, bytes) in damaged.iter().enumerate() {
            fs::write(path(&dir), bytes).expect("write");
            assert!(
                matches!(
                    Journal::open(&dir.0, &key()),
                    Err(JournalError::Corrupt { .. })
                ),
                "damage {index}"
            );
            assert_eq!(
                fs::read(path(&dir)).expect("read"),
                *bytes,
                "damage {index} is left in place"
            );
        }
    }

    /// Earlier damage, records committed after it, then a newest commit that was torn: the
    /// records in between still make the journal corrupt.
    #[test]
    fn damage_followed_by_records_and_a_torn_tail_is_corrupt() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal
            .commit_ecu_hardware_part_number(b"HW-1")
            .expect("commit");
        let damaged = journal.store.len as usize;
        journal
            .commit_transfer_start(STAGE, at(3, 11))
            .expect("commit");
        journal
            .commit_transfer_exit_intent(at(5, 20))
            .expect("commit");
        let end = journal.store.len as usize;
        journal
            .commit_step(at(5, 20), Some(&[0x33; 40]))
            .expect("commit");
        drop(journal);
        let full = fs::read(path(&dir)).expect("read");
        for (name, change) in [("length zeroed", [0u8; 4]), ("length too large", [0xFF; 4])] {
            // The newest commit stopped halfway.
            let mut bytes = full[..end + (full.len() - end) / 2].to_vec();
            bytes[damaged..damaged + 4].copy_from_slice(&change);
            fs::write(path(&dir), &bytes).expect("write");
            assert!(
                matches!(
                    Journal::open(&dir.0, &key()),
                    Err(JournalError::Corrupt { .. })
                ),
                "{name}"
            );
            assert_eq!(fs::read(path(&dir)).expect("read"), bytes, "{name}");
        }
    }

    /// A record numbered at the very end of the range is garbage, not a reason to panic.
    #[test]
    fn a_record_numbered_u64_max_does_not_panic() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal
            .commit_ecu_hardware_part_number(b"HW-1")
            .expect("commit");
        let damaged = journal.store.len as usize;
        journal
            .commit_ecu_hardware_part_number(b"HW-2")
            .expect("commit");
        drop(journal);
        let mut bytes = fs::read(path(&dir)).expect("read");
        bytes[damaged..damaged + 4].fill(0);
        for tail_complete in [true, false] {
            let mut file = bytes.clone();
            push_frame(
                &mut file,
                &encode(&Entry {
                    seq: u64::MAX,
                    at_unix_ms: 0,
                    record: Record::PostTransferComplete,
                })
                .expect("encode"),
            )
            .expect("frame");
            if !tail_complete {
                file.extend_from_slice(&[9, 0, 0, 0]);
            }
            fs::write(path(&dir), &file).expect("write");
            // Either outcome is a defined one; the call must return.
            let _ = Journal::open(&dir.0, &key());
        }
    }

    /// A torn step whose VM state holds the bytes of a whole frame is still a torn tail.
    #[test]
    fn a_frame_inside_a_torn_payload_is_not_a_later_record() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal
            .commit_ecu_hardware_part_number(b"HW-1")
            .expect("commit");
        let committed = journal.state().clone();
        let before = journal.store.len as usize;
        let mut nested = Vec::new();
        let entry = Entry {
            seq: 1,
            at_unix_ms: 0,
            record: Record::Block { block: 0 },
        };
        push_frame(&mut nested, &encode(&entry).expect("encode")).expect("frame");
        let mut vm_state = nested.clone();
        vm_state.extend_from_slice(&[0x55; 32]);
        journal
            .commit_step(at(1, 1), Some(&vm_state))
            .expect("commit");
        drop(journal);
        let bytes = fs::read(path(&dir)).expect("read");
        let nested_at = before
            + bytes[before..]
                .windows(nested.len())
                .position(|window| window == nested)
                .expect("the nested frame is in the file");
        // The step's write stopped inside or after the nested frame.
        for cut in [
            nested_at + 3,
            nested_at + nested.len() - 1,
            nested_at + nested.len() + 1,
            nested_at + nested.len() + 7,
        ] {
            fs::write(path(&dir), &bytes[..cut]).expect("write");
            let journal = Journal::open(&dir.0, &key()).expect("a torn tail");
            assert_eq!(journal.state(), &committed, "cut {cut}");
            assert_eq!(
                fs::metadata(path(&dir)).expect("metadata").len(),
                before as u64
            );
        }
        // Stopped exactly at the nested frame's end, the frame cannot be told from a committed
        // record: the journal reads as corrupt, the cautious outcome, and is left untouched.
        let cut = nested_at + nested.len();
        fs::write(path(&dir), &bytes[..cut]).expect("write");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::Corrupt { .. })
        ));
        assert_eq!(
            fs::metadata(path(&dir)).expect("metadata").len(),
            cut as u64
        );
    }

    #[test]
    fn read_leaves_a_torn_tail_in_place() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal
            .commit_ecu_hardware_part_number(b"HW-1")
            .expect("commit");
        let committed = journal.state().clone();
        drop(journal);
        let mut file = OpenOptions::new()
            .append(true)
            .open(path(&dir))
            .expect("open");
        file.write_all(&[7, 0, 0, 0, 1]).expect("append");
        drop(file);
        let before = fs::read(path(&dir)).expect("read");
        assert_eq!(Journal::read(&dir.0, &key()).expect("read"), committed);
        assert_eq!(fs::read(path(&dir)).expect("read"), before);
        assert!(matches!(
            Journal::read(
                &dir.0,
                &JobKey {
                    generation: 9,
                    ..key()
                }
            ),
            Err(JournalError::NotFound)
        ));
    }

    /// After a resume, the interrupted attempt takes no more progress: the recovery's steps are
    /// not its post-transfer steps, and only a new transfer-start marker opens a transfer.
    #[test]
    fn a_resume_closes_the_interrupted_attempt() {
        for exit_marked in [false, true] {
            let mut j = memory();
            j.commit_transfer_start(STAGE, at(3, 11)).expect("start");
            j.commit_block(0).expect("block");
            if exit_marked {
                j.commit_transfer_exit_intent(at(5, 20)).expect("exit");
                j.commit_step(at(5, 20), None).expect("post step");
            }
            let before = j.summary().transfer.expect("transfer");
            j.commit_resume(STAGE, None).expect("resume");
            // A replayed pre-erase step.
            j.commit_step(at(1, 21), None).expect("step");
            let refused = |result: Result<(), JournalError>| {
                assert!(
                    matches!(result, Err(JournalError::Invariant(_))),
                    "{result:?}"
                );
            };
            refused(j.commit_block(1));
            refused(j.commit_transfer_exit_intent(at(5, 22)));
            refused(j.commit_post_transfer_complete());
            assert_eq!(
                j.summary().transfer,
                Some(TransferAttempt {
                    interrupted: true,
                    ..before
                }),
                "exit marked: {exit_marked}"
            );
            j.commit_transfer_start(STAGE, at(3, 22))
                .expect("new attempt");
            j.commit_block(0).expect("block of the new attempt");
        }
    }

    #[test]
    fn the_hardware_part_number_is_fixed_once_the_transfer_started() {
        let mut j = memory();
        j.commit_ecu_hardware_part_number(b"HW-A")
            .expect("first read");
        j.commit_ecu_hardware_part_number(b"HW-B")
            .expect("before the transfer");
        j.commit_transfer_start(STAGE, at(3, 1)).expect("start");
        j.commit_ecu_hardware_part_number(b"HW-B")
            .expect("the same value again");
        assert!(matches!(
            j.commit_ecu_hardware_part_number(b"HW-A"),
            Err(JournalError::Invariant(_))
        ));
        assert_eq!(
            j.summary().ecu_hardware_part_number.as_deref(),
            Some(&b"HW-B"[..])
        );
    }

    /// A request intent comes after the last step, in step order, and reads back from the file.
    #[test]
    fn a_request_intent_is_ordered_and_reads_back() {
        let mut j = memory();
        j.commit_step(at(2, 10), None).expect("step");
        assert!(matches!(
            j.commit_intent(at(3, 10)),
            Err(JournalError::Invariant(_))
        ));
        j.commit_intent(at(3, 11)).expect("intent after the step");
        assert!(matches!(
            j.commit_intent(at(4, 11)),
            Err(JournalError::Invariant(_))
        ));
        assert_eq!(j.summary().last_intent, Some(at(3, 11)));
        // A step after the intent's request completed does not clear it.
        j.commit_step(at(3, 11), None)
            .expect("the request's own step");
        assert_eq!(j.summary().last_intent, Some(at(3, 11)));

        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal.commit_intent(at(7, 70)).expect("intent");
        drop(journal);
        let reopened = Journal::open(&dir.0, &key()).expect("open");
        assert_eq!(reopened.summary().last_intent, Some(at(7, 70)));
    }

    /// One writer per journal (ADR-255): a second `create` or `open` while a journal is open
    /// is refused without touching the file, and works once the writer is gone. Readers are
    /// not locked out.
    #[test]
    fn a_journal_has_one_writer() {
        let dir = TempDir::new();
        let mut journal = Journal::create(&dir.0, &key(), None).expect("create");
        journal
            .commit_ecu_hardware_part_number(b"HW")
            .expect("commit");
        let bytes = fs::read(path(&dir)).expect("read");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::InUse)
        ));
        assert!(matches!(
            Journal::create(&dir.0, &key(), None),
            Err(JournalError::InUse)
        ));
        assert_eq!(fs::read(path(&dir)).expect("read"), bytes);
        assert_eq!(
            Journal::read(&dir.0, &key()).expect("read").facts,
            journal.summary()
        );
        drop(journal);
        assert!(matches!(
            Journal::create(&dir.0, &key(), None),
            Err(JournalError::AlreadyExists)
        ));
        let reopened = Journal::open(&dir.0, &key()).expect("open after the writer is gone");
        assert!(matches!(
            Journal::open(&dir.0, &key()),
            Err(JournalError::InUse)
        ));
        drop(reopened);
        Journal::open(&dir.0, &key()).expect("open again");
    }

    #[test]
    fn the_vm_state_names_its_step() {
        let mut j = memory();
        j.commit_step(at(2, 10), Some(b"vm-1")).expect("step");
        j.commit_step(at(5, 20), None)
            .expect("step without a state");
        assert_eq!(j.state().last_vm_state, Some((at(2, 10), b"vm-1".to_vec())));
    }

    #[derive(Default)]
    struct Memory(Vec<Vec<u8>>);

    impl Store for Memory {
        fn append_sync(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.0.push(bytes.to_vec());
            Ok(())
        }
    }

    fn memory() -> Journal<Memory> {
        Journal {
            store: Memory::default(),
            state: JournalState {
                facts: RecoveryFacts::new(key()),
                last_vm_state: None,
                records: 0,
            },
            poisoned: false,
        }
    }

    struct Failing;

    impl Store for Failing {
        fn append_sync(&mut self, _: &[u8]) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[test]
    fn a_failed_write_changes_nothing_and_poisons_the_journal() {
        let mut j = Journal {
            store: Failing,
            state: memory().state,
            poisoned: false,
        };
        let before = j.state().clone();
        assert!(matches!(
            j.commit_transfer_start(STAGE, at(1, 1)),
            Err(JournalError::Io(_))
        ));
        assert_eq!(j.state(), &before);
        assert!(matches!(
            j.commit_step(at(1, 1), None),
            Err(JournalError::Poisoned)
        ));
    }

    #[test]
    fn an_oversized_record_is_refused_without_poisoning() {
        let mut j = memory();
        // Too large for a frame on its own, and too large once framed with its record.
        for len in [MAX_FRAME as usize + 1, MAX_FRAME as usize] {
            let blob = vec![0u8; len];
            assert!(
                matches!(
                    j.commit_step(at(1, 1), Some(&blob)),
                    Err(JournalError::TooLarge)
                ),
                "{len}"
            );
        }
        j.commit_step(at(1, 1), Some(b"vm"))
            .expect("a smaller one fits");
    }

    /// postcard encodes the variant index: a reordered or removed variant would misread every
    /// journal already written.
    #[test]
    fn record_variants_keep_their_index() {
        let records = [
            Record::Step {
                at: at(0, 0),
                vm_state: None,
            },
            Record::Block { block: 0 },
            Record::EcuHardwarePartNumber(Vec::new()),
            Record::PreEraseSoftwareVersion(Vec::new()),
            Record::Resume {
                stage: STAGE,
                count: 1,
                attempt_key: None,
            },
            Record::TransferStart {
                stage: STAGE,
                at: at(0, 0),
            },
            Record::TransferExitIntent { at: at(0, 0) },
            Record::PostTransferComplete,
            Record::Intent { at: at(0, 0) },
            Record::TargetVin(Vin::new(String::new())),
        ];
        for (index, record) in records.iter().enumerate() {
            assert_eq!(
                postcard::to_allocvec(record).expect("encode")[0] as usize,
                index,
                "{record:?}"
            );
        }
    }
}

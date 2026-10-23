//! The agent's communication log (design 8.11, ADR-346, ADR-347).
//!
//! One JSON Lines file per job and ownership generation, `{job_id}.g{generation}.commlog.jsonl`,
//! beside the job's journal. Every run of the job appends to the same file, under a writer lock
//! of its own (`{job_id}.g{generation}.commlog.lock`), so a second run of the job that finds the
//! log held runs without it. The log never steers a job: a failure to open it or to write it
//! changes the job's [`CommLogStatus`] and nothing else.
//!
//! This part writes the lines that exist without an exchange: the `header` (once per log), the
//! `agent_clock` reading a reopen writes, and a run's `job_start` and `job_end`. Every line has
//! the next `seq` and an `event` naming its kind. [`redaction::redact`] is how an exchange entry
//! will carry a message's bytes.
//!
//! A log is closed cleanly when it starts with its header, its lines number 0, 1, ... with
//! nothing blank or damaged between them, and its last line is a complete `job_end`. A log that
//! is opened again otherwise (a crash, a power loss, a write that failed) is cut after its last
//! complete line, which drops a torn tail, goes on after the largest `seq` it holds, and its
//! status ends as [`CommLogIncomplete::UncleanReopen`] at best; so does a resume that finds no
//! log, since the job ran before.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use diag_ir::VmState;
use serde::Serialize;

use crate::artifact::to_hex;
use crate::journal::{JobKey, JournalError, Vin, WriterLock, job_file_stem, sync_dir};
use crate::runner::JobError;

/// What an exchange entry carries of a message's bytes (design 8.11, ADR-347).
#[cfg_attr(not(test), expect(dead_code, reason = "the exchange entries use it"))]
pub(crate) mod redaction;

/// Where a job keeps its communication log and under which key. The journaled entry points
/// derive it from the journal's setup (same directory and key); a job without a journal gets one
/// only through `Job::with_comm_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommLogSetup {
    pub dir: PathBuf,
    pub key: JobKey,
}

impl CommLogSetup {
    /// The log file of the setup's job and generation.
    pub fn path(&self) -> Result<PathBuf, JournalError> {
        Ok(self
            .dir
            .join(format!("{}.commlog.jsonl", job_file_stem(&self.key)?)))
    }
}

/// How a job's log ended, which a run returns beside its result (`JobRun::comm_log`). The
/// result's `Ok` or `Err` never depends on it (ADR-347).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommLogStatus {
    /// No log was kept: none was configured, or the job was refused before its thread started,
    /// or it ended before the log would have been opened (a cancel, a journal another run holds,
    /// a resume its journal turns away: a declared failure, an identity abort, a run's stop).
    None,
    /// The log was opened (or reopened after a clean close), took every line and ended with a
    /// synced `job_end`.
    Complete,
    /// The log is missing lines or may be; design 8.11 marks the job's result "communication log
    /// incomplete".
    Incomplete(CommLogIncomplete),
}

/// Why a log is incomplete. The first cause stays; a write that failed outranks an unclean
/// reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommLogIncomplete {
    /// The log could not be opened and the job ran without it. A log another run holds is
    /// [`io::ErrorKind::WouldBlock`].
    NotOpened(io::ErrorKind),
    /// Line `seq` could not be written; the log took no line after it and no `job_end`.
    WriteFailed { seq: u64, kind: io::ErrorKind },
    /// The log was reopened without a clean close, or with a torn last line, so lines not yet
    /// flushed by the earlier run may be missing.
    UncleanReopen,
    /// The opening lines or the `job_end` could not be made durable.
    CloseFailed(io::ErrorKind),
}

/// What kind of run a job's `job_start` records, as the runner knows it when it opens the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunKind {
    /// A first run (`run_program`, `run_program_journaled`).
    First,
    /// A restart of a job whose journal exists (`resume_program_journaled`).
    Resume,
    /// A resume whose journal is missing, which the runner may take as a plain start.
    PlainStart,
}

/// What the job knows of itself when its log starts: the header's content, and a run's facts
/// for its `job_start`.
#[derive(Debug, Clone)]
pub(crate) struct RunFacts {
    pub job_id: String,
    pub generation: u64,
    /// SHA-256 of the job's document, when it has one.
    pub document_sha256: Option<[u8; 32]>,
    /// The ECU variant's short name, when the document gives one.
    pub variant_name: Option<String>,
    /// The job's target VIN. The log has the journal's protection (design 8.11); the VIN never
    /// goes into an error or a trace.
    pub target_vin: Option<Vin>,
    /// Whether the job's program writes.
    pub writes: bool,
}

/// A line of the log: its sequence number and its event.
#[derive(Serialize)]
struct Line<'a> {
    seq: u64,
    #[serde(flatten)]
    event: &'a Event<'a>,
}

/// The events of this part of the log (design 8.11).
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event<'a> {
    Header(Header<'a>),
    AgentClock(AgentClock),
    JobStart(JobStart),
    JobEnd(JobEnd<'a>),
}

/// The first line of a log. Groups the job does not have are left out: the vehicle without a
/// target VIN, the targets without a document. The conversion identity is absent (no accessor
/// gives it yet).
#[derive(Serialize)]
struct Header<'a> {
    job: HeaderJob<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vehicle: Option<HeaderVehicle<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    targets: Option<Vec<HeaderTarget<'a>>>,
    software: HeaderSoftware,
    clock: AgentClock,
}

#[derive(Serialize)]
struct HeaderJob<'a> {
    id: &'a str,
    generation: u64,
    /// The only kind the runner has.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    document_sha256: Option<String>,
}

#[derive(Serialize)]
struct HeaderVehicle<'a> {
    target_vin: &'a str,
}

/// A target as far as the job knows it: the runner binds no roles, so only the ECU variant.
#[derive(Serialize)]
struct HeaderTarget<'a> {
    ecu_variant: &'a str,
}

#[derive(Serialize)]
struct HeaderSoftware {
    agent: &'static str,
    ir_schema: u32,
}

/// One reading of the agent's clocks: the monotonic one as microseconds since the log began (in
/// this agent epoch), and the UTC wall clock. A later wall-clock step shows against the
/// monotonic reading. A reopen starts a new epoch, since the monotonic clock starts over.
#[derive(Serialize)]
struct AgentClock {
    epoch: u64,
    monotonic_us: u64,
    wall_unix_ms: u64,
    /// Set (`true`) when the log was reopened without a clean close.
    #[serde(skip_serializing_if = "Option::is_none")]
    incomplete: Option<bool>,
}

/// A run's start. A job's later runs write theirs to the same log.
#[derive(Serialize)]
struct JobStart {
    agent_version: &'static str,
    run: RunKind,
    writes: bool,
}

#[derive(Serialize)]
struct JobEnd<'a> {
    /// `ok`, or the name of the `JobError` variant. No VIN and no personal data.
    outcome: &'a str,
}

/// The `outcome` a result writes to `job_end`. Exhaustive, so a new [`JobError`] variant must
/// be named here.
pub(crate) fn outcome_name(result: &Result<VmState, JobError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(error) => match error {
            JobError::CurrentThreadRuntime => "CurrentThreadRuntime",
            JobError::Link(_) => "Link",
            JobError::Vm { .. } => "Vm",
            JobError::Program(_) => "Program",
            JobError::Host { .. } => "Host",
            JobError::Unanswerable(_) => "Unanswerable",
            JobError::StepLimit(_) => "StepLimit",
            JobError::Refused { .. } => "Refused",
            JobError::Cancelled => "Cancelled",
            JobError::Journal(_) => "Journal",
            JobError::IdentityUnreadable { .. } => "IdentityUnreadable",
            JobError::IdentityRead { .. } => "IdentityRead",
            JobError::IdentityMismatch { .. } => "IdentityMismatch",
            JobError::OnSiteInterventionRequired(_) => "OnSiteInterventionRequired",
            JobError::ProcedureFailed { .. } => "ProcedureFailed",
            JobError::NoReprogrammingSlot => "NoReprogrammingSlot",
            JobError::LinkUnconfirmed => "LinkUnconfirmed",
            JobError::Panicked => "Panicked",
            JobError::NoDeclarations => "NoDeclarations",
            JobError::Sources(_) => "Sources",
            JobError::NoArtifact => "NoArtifact",
            JobError::ArtifactDiffersFromJournal => "ArtifactDiffersFromJournal",
            JobError::ArtifactMismatch { .. } => "ArtifactMismatch",
            JobError::SegmentOutsideArtifact { .. } => "SegmentOutsideArtifact",
            JobError::Undeclared { .. } => "Undeclared",
            JobError::VehicleLock(_) => "VehicleLock",
            JobError::GuardsMissing => "GuardsMissing",
            JobError::CommLogMismatch => "CommLogMismatch",
        },
    }
}

/// Where lines go. The file is one; tests give a sink that fails.
pub(crate) trait LineSink: Send {
    /// Writes one whole line, newline included.
    fn write_line(&mut self, line: &[u8]) -> io::Result<()>;
    /// Makes what was written durable.
    fn sync(&mut self) -> io::Result<()>;
}

/// The log file, positioned at its end. Opened for reading and writing rather than appending,
/// since a Windows handle in append mode cannot be cut with `set_len`.
struct FileSink(File);

impl LineSink for FileSink {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.0.write_all(line)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
}

/// Logs whose writes fail after some lines, for the runner's tests: the runner opens a job's log
/// on the job's thread, out of a test's reach.
#[cfg(test)]
pub(crate) mod fault {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::LineSink;

    static FAIL_AFTER: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());

    /// Makes the next opening of the log at `path` take `lines` lines and fail every write
    /// after them.
    pub(crate) fn fail_after(path: PathBuf, lines: usize) {
        FAIL_AFTER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((path, lines));
    }

    pub(super) fn wrap(path: &Path, sink: Box<dyn LineSink>) -> Box<dyn LineSink> {
        let mut armed = FAIL_AFTER.lock().unwrap_or_else(|error| error.into_inner());
        match armed.iter().position(|(armed, _)| armed == path) {
            Some(at) => Box::new(FailingAfter {
                sink,
                left: armed.remove(at).1,
            }),
            None => sink,
        }
    }

    struct FailingAfter {
        sink: Box<dyn LineSink>,
        left: usize,
    }

    impl LineSink for FailingAfter {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            if self.left == 0 {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.left -= 1;
            self.sink.write_line(line)
        }

        fn sync(&mut self) -> io::Result<()> {
            self.sink.sync()
        }
    }
}

/// What the opening lines of this run are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// No line yet: the header comes first.
    Fresh,
    /// Lines of an earlier run: an `agent_clock` comes first.
    Reopened,
}

struct Inner {
    sink: Box<dyn LineSink>,
    next_seq: u64,
    /// The first failure; the log takes no line after it.
    failed: Option<CommLogIncomplete>,
    /// The log was reopened without a clean close.
    unclean: bool,
    /// This agent's epoch: 0 for a log's first agent, the previous one's plus one at a reopen.
    epoch: u64,
    origin: Instant,
    /// The opening lines not yet written.
    opening: Option<Opening>,
    ended: bool,
    /// Held while the log is open; absent for a sink a test gives.
    _lock: Option<WriterLock>,
}

/// A job's open communication log. Clones share one writer, so the host and the runner write to
/// the same file in order. A line is one `write_all` of a serialized object and a newline, with
/// no buffering in between and no sync per line; the opening lines and `job_end` are synced
/// (ADR-347), so a crash can lose lines written since but not tear an earlier one. A failed
/// write stops the log (the lines after it could claim what was lost), and the job goes on.
#[derive(Clone)]
pub struct CommLog(Arc<Mutex<Inner>>);

impl CommLog {
    /// Opens the log of `setup`'s job and generation for appending, creating it when missing
    /// (ADR-347). Takes the log's writer lock first: a log another run holds is
    /// [`CommLogIncomplete::NotOpened`] with [`io::ErrorKind::WouldBlock`]. A log that ends in a
    /// torn line is cut after its last `\n`; one that does not end in a `job_end` is
    /// reopened as unclean ([`CommLogIncomplete::UncleanReopen`] at the end of the run).
    pub fn open(setup: &CommLogSetup) -> Result<Self, CommLogIncomplete> {
        Self::open_file(setup).map_err(CommLogIncomplete::NotOpened)
    }

    fn open_file(setup: &CommLogSetup) -> Result<Self, io::ErrorKind> {
        let kind = |error: JournalError| match error {
            JournalError::InUse => io::ErrorKind::WouldBlock,
            JournalError::Io(error) => error.kind(),
            _ => io::ErrorKind::InvalidInput,
        };
        let path = setup.path().map_err(kind)?;
        let lock_path = setup.dir.join(format!(
            "{}.commlog.lock",
            job_file_stem(&setup.key).map_err(kind)?
        ));
        let lock = WriterLock::at(&lock_path).map_err(kind)?;
        let existed = path.try_exists().map_err(|error| error.kind())?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| error.kind())?;
        if !existed {
            // A new file's name is durable only once its directory is (as the journal's).
            sync_dir(&setup.dir).map_err(|error| error.kind())?;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| error.kind())?;
        let scan = scan(&bytes).ok_or(io::ErrorKind::InvalidData)?;
        let io_kind = |error: io::Error| error.kind();
        if scan.complete_len < bytes.len() {
            tracing::warn!(
                path = %path.display(),
                cut = bytes.len() - scan.complete_len,
                "communication log ends in a line that was not finished; cutting it off"
            );
            file.set_len(scan.complete_len as u64).map_err(io_kind)?;
            file.sync_data().map_err(io_kind)?;
        }
        file.seek(SeekFrom::Start(scan.complete_len as u64))
            .map_err(io_kind)?;
        let sink: Box<dyn LineSink> = Box::new(FileSink(file));
        #[cfg(test)]
        let sink = fault::wrap(&path, sink);
        Ok(Self::new(sink, scan, Some(lock)))
    }

    fn new(sink: Box<dyn LineSink>, scan: Scan, lock: Option<WriterLock>) -> Self {
        // A log goes on after an earlier run only when its first line is its header; otherwise
        // a header is written after whatever the file holds, marked incomplete.
        let reopened = scan.header;
        Self(Arc::new(Mutex::new(Inner {
            sink,
            next_seq: scan.next_seq,
            failed: None,
            unclean: scan.torn
                || (scan.complete_len > 0 && !(scan.header && scan.consistent && scan.clean_close)),
            epoch: if reopened { scan.next_epoch } else { 0 },
            origin: Instant::now(),
            opening: Some(if reopened {
                Opening::Reopened
            } else {
                Opening::Fresh
            }),
            ended: false,
            _lock: lock,
        })))
    }

    /// A log over `sink` that continues a file `scan` describes, for tests.
    #[cfg(test)]
    fn over(sink: Box<dyn LineSink>, existing: &[u8]) -> Self {
        Self::new(sink, scan(existing).expect("a scannable log"), None)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Writes the opening lines of a run and syncs them: the `header` on a fresh log or an
    /// `agent_clock` on a reopened one, then `job_start`. Written once per run.
    pub(crate) fn begin(&self, facts: &RunFacts, run: RunKind) {
        let mut inner = self.lock();
        let Some(opening) = inner.opening.take() else {
            return;
        };
        // A resume belongs to a job that ran before: a log it has to start was lost.
        if opening == Opening::Fresh && run == RunKind::Resume {
            inner.unclean = true;
        }
        match opening {
            Opening::Fresh => {
                // The log begins here: the header reads 0.
                inner.origin = Instant::now();
                let clock = AgentClock {
                    monotonic_us: 0,
                    ..inner.clock()
                };
                let variant = facts.variant_name.as_deref();
                let header = Header {
                    job: HeaderJob {
                        id: &facts.job_id,
                        generation: facts.generation,
                        kind: "procedure",
                        document_sha256: facts.document_sha256.as_ref().map(|hash| to_hex(hash)),
                    },
                    vehicle: facts.target_vin.as_ref().map(|target_vin| HeaderVehicle {
                        target_vin: target_vin.as_str(),
                    }),
                    targets: variant.map(|ecu_variant| vec![HeaderTarget { ecu_variant }]),
                    software: HeaderSoftware {
                        agent: env!("CARGO_PKG_VERSION"),
                        ir_schema: diag_ir::IR_SCHEMA_VERSION,
                    },
                    // A file that held only a torn line was reopened without a clean close.
                    clock: AgentClock {
                        incomplete: inner.unclean.then_some(true),
                        ..clock
                    },
                };
                inner.append(&Event::Header(header));
            }
            Opening::Reopened => {
                let mut clock = inner.clock();
                clock.incomplete = inner.unclean.then_some(true);
                inner.append(&Event::AgentClock(clock));
            }
        }
        inner.append(&Event::JobStart(JobStart {
            agent_version: env!("CARGO_PKG_VERSION"),
            run,
            writes: facts.writes,
        }));
        inner.sync();
    }

    /// Writes the run's `job_end` with `outcome` and syncs it. Nothing is written after a failure.
    pub(crate) fn end(&self, outcome: &str) {
        let mut inner = self.lock();
        if inner.failed.is_some() {
            return;
        }
        inner.append(&Event::JobEnd(JobEnd { outcome }));
        inner.sync();
        inner.ended = inner.failed.is_none();
    }

    /// How the log stands now. After [`CommLog::end`] it is the run's final status; before it,
    /// a log that has not failed reads as not closed.
    pub(crate) fn status(&self) -> CommLogStatus {
        let inner = self.lock();
        if let Some(failure) = &inner.failed {
            CommLogStatus::Incomplete(failure.clone())
        } else if inner.unclean {
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        } else if inner.ended {
            CommLogStatus::Complete
        } else {
            // No `job_end` was written: the run is not over, or it was never ended.
            CommLogStatus::Incomplete(CommLogIncomplete::CloseFailed(io::ErrorKind::Other))
        }
    }
}

impl Inner {
    fn clock(&self) -> AgentClock {
        AgentClock {
            epoch: self.epoch,
            monotonic_us: u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX),
            wall_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
            incomplete: None,
        }
    }

    /// Writes `event` as the next line unless the log has failed. A failure is recorded and
    /// stops the log.
    fn append(&mut self, event: &Event<'_>) {
        if self.failed.is_some() {
            return;
        }
        let seq = self.next_seq;
        let Some(next) = seq.checked_add(1) else {
            self.failed = Some(CommLogIncomplete::WriteFailed {
                seq,
                kind: io::ErrorKind::InvalidData,
            });
            return;
        };
        let mut line = Vec::with_capacity(256);
        let written = serde_json::to_writer(&mut line, &Line { seq, event })
            .map_err(io::Error::from)
            .and_then(|()| {
                line.push(b'\n');
                self.sink.write_line(&line)
            });
        match written {
            Ok(()) => self.next_seq = next,
            Err(error) => {
                self.failed = Some(CommLogIncomplete::WriteFailed {
                    seq,
                    kind: error.kind(),
                });
            }
        }
    }

    fn sync(&mut self) {
        if self.failed.is_some() {
            return;
        }
        if let Err(error) = self.sink.sync() {
            self.failed = Some(CommLogIncomplete::CloseFailed(error.kind()));
        }
    }
}

/// What an existing log's bytes say.
#[derive(Debug, Default)]
struct Scan {
    /// The length up to and including the last `\n`.
    complete_len: usize,
    /// Bytes follow the last `\n`.
    torn: bool,
    /// Complete lines that are not blank.
    lines: usize,
    /// The first of them is a header with `seq` 0.
    header: bool,
    /// Every one of them is a JSON object whose `seq` is one more than the line's before (the
    /// first 0), and the first is the header: nothing between them is missing or damaged.
    consistent: bool,
    /// The `seq` the next line takes: one more than the largest `seq` read, so numbering never
    /// goes back.
    next_seq: u64,
    /// The agent epoch the next reopen starts: one more than the largest read.
    next_epoch: u64,
    /// The last of them is a `job_end`.
    clean_close: bool,
}

/// Reads an existing log. `None` when it holds a `seq` or an epoch with no successor, which
/// only a damaged or forged file can.
fn scan(bytes: &[u8]) -> Option<Scan> {
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let mut scan = Scan {
        complete_len,
        torn: complete_len < bytes.len(),
        consistent: true,
        ..Scan::default()
    };
    let mut largest_seq: Option<u64> = None;
    let mut largest_epoch: Option<u64> = None;
    for line in bytes[..complete_len].split_inclusive(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            // A blank line is no line of this writer's.
            scan.consistent = false;
            continue;
        }
        let first = scan.lines == 0;
        scan.lines += 1;
        scan.clean_close = false;
        let parsed = serde_json::from_slice::<serde_json::Value>(line).ok();
        let object = parsed.as_ref().and_then(|value| value.as_object());
        let seq = object.and_then(|object| object.get("seq")?.as_u64());
        let event = object.and_then(|object| object.get("event")?.as_str());
        let expected = largest_seq.map_or(Some(0), |seq| seq.checked_add(1));
        if seq.is_none() || seq != expected {
            scan.consistent = false;
        }
        if let Some(seq) = seq {
            largest_seq = Some(largest_seq.map_or(seq, |largest| largest.max(seq)));
        }
        match event {
            Some("header") if first && seq == Some(0) => {
                scan.header = true;
                largest_epoch = Some(largest_epoch.unwrap_or(0));
            }
            Some("agent_clock") => {
                if let Some(epoch) = object.and_then(|object| object.get("epoch")?.as_u64()) {
                    largest_epoch = Some(largest_epoch.map_or(epoch, |largest| largest.max(epoch)));
                }
            }
            Some("job_end") => scan.clean_close = true,
            _ => {}
        }
        if first && !scan.header {
            scan.consistent = false;
        }
    }
    scan.next_seq = match largest_seq {
        Some(seq) => seq.checked_add(1)?,
        None => 0,
    };
    scan.next_epoch = match largest_epoch {
        Some(epoch) => epoch.checked_add(1)?,
        None => 0,
    };
    Some(scan)
}

/// A job's log as the job thread holds it: the setup, what the header needs, and the log once
/// it is open (ADR-347). `finish` ends the log with the job's result and gives the status.
pub(crate) struct JobLog {
    setup: Option<CommLogSetup>,
    facts: Option<RunFacts>,
    log: Option<CommLog>,
    not_opened: Option<CommLogIncomplete>,
}

impl JobLog {
    /// A job without a log.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            setup: None,
            facts: None,
            log: None,
            not_opened: None,
        }
    }

    pub(crate) fn new(setup: Option<CommLogSetup>, facts: RunFacts) -> Self {
        Self {
            facts: setup.is_some().then_some(facts),
            setup,
            log: None,
            not_opened: None,
        }
    }

    /// Opens the log, if the job has one, and writes the run's opening lines. A log that cannot
    /// be opened is remembered for [`JobLog::finish`] and the job goes on. Gives the log for the
    /// host.
    pub(crate) fn open(&mut self, run: RunKind) -> Option<CommLog> {
        let (setup, facts) = (self.setup.as_ref()?, self.facts.as_ref()?);
        match CommLog::open(setup) {
            Ok(log) => {
                log.begin(facts, run);
                self.log = Some(log.clone());
                Some(log)
            }
            Err(incomplete) => {
                tracing::warn!(?incomplete, "the communication log could not be opened");
                self.not_opened = Some(incomplete);
                None
            }
        }
    }

    /// Ends the log with `result` and gives the job's status.
    pub(crate) fn finish(self, result: &Result<VmState, JobError>) -> CommLogStatus {
        match (self.log, self.not_opened) {
            (Some(log), _) => {
                log.end(outcome_name(result));
                log.status()
            }
            (None, Some(incomplete)) => CommLogStatus::Incomplete(incomplete),
            (None, None) => CommLogStatus::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ngr-comm-log-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup(dir: &std::path::Path) -> CommLogSetup {
        CommLogSetup {
            dir: dir.to_owned(),
            key: JobKey {
                job_id: shared_proto::JobId("comm-log-test".to_owned()),
                generation: 3,
            },
        }
    }

    fn facts() -> RunFacts {
        RunFacts {
            job_id: "comm-log-test".to_owned(),
            generation: 3,
            document_sha256: Some([0xAB; 32]),
            variant_name: Some("Engine_V1".to_owned()),
            target_vin: Some(Vin::new("WVWZZZ1JZXW000001".to_owned())),
            writes: false,
        }
    }

    /// The lines of the log file, parsed; every one must be a JSON object.
    fn lines(setup: &CommLogSetup) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(setup.path().unwrap()).unwrap();
        assert!(text.is_empty() || text.ends_with('\n'), "{text:?}");
        text.lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect(line);
                assert!(value.is_object(), "{line}");
                value
            })
            .collect()
    }

    fn events(lines: &[serde_json::Value]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| line["event"].as_str().unwrap())
            .collect()
    }

    fn assert_consecutive(lines: &[serde_json::Value]) {
        for (at, line) in lines.iter().enumerate() {
            assert_eq!(line["seq"], at as u64, "{lines:?}");
        }
    }

    fn run_once(setup: &CommLogSetup, outcome: &str) -> CommLogStatus {
        let log = CommLog::open(setup).expect("opens");
        log.begin(&facts(), RunKind::First);
        log.end(outcome);
        log.status()
    }

    // ---- the log file ----

    #[test]
    fn a_fresh_log_has_the_header_then_job_start_then_job_end() {
        let dir = dir();
        let setup = setup(&dir);
        assert_eq!(run_once(&setup, "ok"), CommLogStatus::Complete);
        let lines = lines(&setup);
        assert_eq!(events(&lines), ["header", "job_start", "job_end"]);
        assert_consecutive(&lines);
        let header = &lines[0];
        assert_eq!(header["job"]["id"], "comm-log-test");
        assert_eq!(header["job"]["generation"], 3);
        assert_eq!(header["job"]["kind"], "procedure");
        assert_eq!(header["job"]["document_sha256"], "ab".repeat(32));
        assert_eq!(header["vehicle"]["target_vin"], "WVWZZZ1JZXW000001");
        assert_eq!(header["targets"][0]["ecu_variant"], "Engine_V1");
        assert_eq!(header["software"]["agent"], env!("CARGO_PKG_VERSION"));
        assert_eq!(header["software"]["ir_schema"], diag_ir::IR_SCHEMA_VERSION);
        assert_eq!(header["clock"]["epoch"], 0);
        assert_eq!(header["clock"]["monotonic_us"], 0);
        assert!(header["clock"]["wall_unix_ms"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["run"], "first");
        assert_eq!(lines[1]["writes"], false);
        assert_eq!(lines[2]["outcome"], "ok");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_header_without_a_document_or_a_vin_leaves_those_groups_out() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(
            &RunFacts {
                document_sha256: None,
                variant_name: None,
                target_vin: None,
                ..facts()
            },
            RunKind::First,
        );
        log.end("ok");
        let header = lines(&setup).remove(0);
        assert!(header.get("vehicle").is_none() && header.get("targets").is_none());
        assert!(header["job"].get("document_sha256").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reopen_after_a_clean_close_writes_a_new_epoch_and_goes_on_numbering() {
        let dir = dir();
        let setup = setup(&dir);
        run_once(&setup, "Cancelled");
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(log.status(), CommLogStatus::Complete);
        drop(log);
        let lines = lines(&setup);
        assert_eq!(
            events(&lines),
            [
                "header",
                "job_start",
                "job_end",
                "agent_clock",
                "job_start",
                "job_end"
            ]
        );
        assert_consecutive(&lines);
        assert_eq!(lines[3]["epoch"], 1);
        assert!(lines[3].get("incomplete").is_none());
        assert_eq!(lines[4]["run"], "resume");
        assert_eq!(lines[2]["outcome"], "Cancelled");
        // A third run is epoch 2.
        run_once(&setup, "ok");
        assert_eq!(self::lines(&setup)[6]["epoch"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_last_line_is_cut_and_the_reopen_is_incomplete() {
        let dir = dir();
        let setup = setup(&dir);
        run_once(&setup, "ok");
        // A run that died: its opening lines are there, its job_end is not, and its last line
        // is half written.
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::Resume);
        drop(log);
        let path = setup.path().unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"seq":6,"event":"job_en"#).unwrap();
        drop(file);

        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
        let lines = lines(&setup);
        assert_eq!(
            events(&lines),
            [
                "header",
                "job_start",
                "job_end",
                "agent_clock",
                "job_start",
                "agent_clock",
                "job_start",
                "job_end"
            ]
        );
        assert_consecutive(&lines);
        assert_eq!(lines[5]["incomplete"], true);
        assert_eq!(lines[5]["epoch"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_without_a_job_end_is_an_unclean_reopen_even_if_everything_else_succeeds() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        drop(log);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::PlainStart);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
        drop(log);
        assert_eq!(lines(&setup)[2]["incomplete"], true);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_another_run_holds_is_not_opened() {
        let dir = dir();
        let setup = setup(&dir);
        let held = CommLog::open(&setup).unwrap();
        let second = CommLog::open(&setup);
        assert!(
            matches!(
                second,
                Err(CommLogIncomplete::NotOpened(io::ErrorKind::WouldBlock))
            ),
            "{:?}",
            second.map(|_| ())
        );
        drop(held);
        CommLog::open(&setup).expect("free again");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_job_id_that_cannot_name_a_file_is_not_opened() {
        let dir = dir();
        let mut setup = setup(&dir);
        setup.key.job_id = shared_proto::JobId("../x".to_owned());
        assert!(matches!(
            CommLog::open(&setup),
            Err(CommLogIncomplete::NotOpened(io::ErrorKind::InvalidInput))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A sink that records what it took and fails the write numbered `fail_write` (from 0).
    #[derive(Clone, Default)]
    struct Recording {
        taken: Arc<Mutex<Vec<u8>>>,
        writes: Arc<AtomicU64>,
        fail_write: Option<u64>,
        fail_sync: bool,
    }

    impl LineSink for Recording {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            if Some(self.writes.fetch_add(1, Ordering::Relaxed)) == self.fail_write {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            self.taken.lock().unwrap().extend_from_slice(line);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            if self.fail_sync {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            Ok(())
        }
    }

    impl Recording {
        fn lines(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.taken.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn taken_events(sink: &Recording) -> Vec<String> {
        String::from_utf8(sink.taken.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["event"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    #[test]
    fn a_failed_write_stops_the_log_and_writes_no_job_end() {
        let sink = Recording {
            fail_write: Some(1),
            ..Recording::default()
        };
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::WriteFailed {
                seq: 1,
                kind: io::ErrorKind::StorageFull
            })
        );
        assert_eq!(taken_events(&sink), ["header"]);
        assert_eq!(
            sink.writes.load(Ordering::Relaxed),
            2,
            "nothing is written after the failure"
        );
    }

    #[test]
    fn a_failed_sync_is_a_failed_close() {
        let sink = Recording {
            fail_sync: true,
            ..Recording::default()
        };
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::CloseFailed(
                io::ErrorKind::PermissionDenied
            ))
        );
        assert_eq!(taken_events(&sink), ["header", "job_start"]);
    }

    #[test]
    fn a_write_failure_outranks_an_unclean_reopen() {
        let sink = Recording {
            fail_write: Some(0),
            ..Recording::default()
        };
        let log = CommLog::over(
            Box::new(sink),
            b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":1,\"event\":\"job_start\"}\n",
        );
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert!(matches!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::WriteFailed { seq: 2, .. })
        ));
    }

    #[test]
    fn the_scan_goes_on_after_the_largest_seq_and_epoch() {
        let scan = scan(
            b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":1,\"event\":\"agent_clock\",\"epoch\":6}\nnot json\n{\"seq\":\"x\"}\n[1]\n",
        )
        .unwrap();
        assert_eq!((scan.next_seq, scan.next_epoch, scan.lines), (2, 7, 5));
        assert!(scan.header && !scan.consistent && !scan.clean_close && !scan.torn);
        let empty = super::scan(b"").unwrap();
        assert_eq!((empty.lines, empty.next_seq, empty.torn), (0, 0, false));
        assert!(!empty.header);
    }

    /// A log whose lines run 0, 1, ... from its header to a `job_end`.
    const CLEAN: &[u8] = b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":1,\"event\":\"job_start\"}\n{\"seq\":2,\"event\":\"job_end\"}\n";

    #[test]
    fn a_seq_that_goes_back_does_not_number_lines_twice() {
        let mut bytes = CLEAN.to_vec();
        bytes.extend_from_slice(b"{\"seq\":1,\"event\":\"job_end\"}\n");
        let scan = scan(&bytes).unwrap();
        assert_eq!(scan.next_seq, 3, "after the largest seq, not the last");
        assert!(!scan.consistent, "a seq that goes back is damage");
        assert!(super::scan(CLEAN).unwrap().consistent);
    }

    #[test]
    fn a_damaged_line_before_a_job_end_is_not_a_clean_close() {
        // A sync that kept `job_end` but lost the line before it.
        let damaged =
            b"{\"seq\":0,\"event\":\"header\"}\n\0\0\0\0\n{\"seq\":2,\"event\":\"job_end\"}\n";
        let gap = b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":2,\"event\":\"job_end\"}\n";
        for bytes in [
            &damaged[..],
            &gap[..],
            b"{\"seq\":0,\"event\":\"header\"}\n \n{\"seq\":1,\"event\":\"job_end\"}\n",
        ] {
            let sink = Recording::default();
            let log = CommLog::over(Box::new(sink.clone()), bytes);
            log.begin(&facts(), RunKind::Resume);
            log.end("ok");
            assert_eq!(
                log.status(),
                CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), CLEAN);
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(log.status(), CommLogStatus::Complete);
    }

    #[test]
    fn a_file_without_a_header_first_gets_one_marked_incomplete() {
        for bytes in [
            &b"\n"[..],
            b" \n",
            b"junk\n",
            b"{\"seq\":0,\"event\":\"job_end\"}\n",
        ] {
            let sink = Recording::default();
            let log = CommLog::over(Box::new(sink.clone()), bytes);
            log.begin(&facts(), RunKind::First);
            log.end("ok");
            let lines = sink.lines();
            assert_eq!(
                lines[0]["event"],
                "header",
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
            assert_eq!(lines[0]["clock"]["incomplete"], true);
            assert_eq!(
                log.status(),
                CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
            );
        }
    }

    #[test]
    fn a_resume_that_finds_no_log_marks_the_new_one_incomplete() {
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(sink.lines()[0]["clock"]["incomplete"], true);
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
        // A first run starts a log cleanly.
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        assert_eq!(log.status(), CommLogStatus::Complete);
    }

    #[test]
    fn a_seq_or_epoch_without_a_successor_is_not_opened_and_never_overflows() {
        let max = u64::MAX;
        assert!(
            super::scan(format!("{{\"seq\":{max},\"event\":\"header\"}}\n").as_bytes()).is_none()
        );
        assert!(
            super::scan(
                format!("{{\"seq\":0,\"event\":\"header\"}}\n{{\"seq\":1,\"event\":\"agent_clock\",\"epoch\":{max}}}\n")
                    .as_bytes()
            )
            .is_none()
        );
        // A log that reaches the last seq stops instead of wrapping.
        let near = format!(
            "{{\"seq\":0,\"event\":\"header\"}}\n{{\"seq\":{},\"event\":\"job_end\"}}\n",
            max - 2
        );
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), near.as_bytes());
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::WriteFailed {
                seq: max,
                kind: io::ErrorKind::InvalidData,
            })
        );
        let seqs: Vec<u64> = sink
            .lines()
            .iter()
            .map(|line| line["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [max - 1]);
    }

    #[test]
    fn crlf_line_ends_read_as_lines() {
        let scan =
            scan(b"{\"seq\":0,\"event\":\"header\"}\r\n{\"seq\":1,\"event\":\"job_end\"}\r\n")
                .unwrap();
        assert!(scan.header && scan.consistent && scan.clean_close);
        assert_eq!(scan.next_seq, 2);
    }

    #[test]
    fn every_error_has_an_outcome_name_without_data() {
        assert_eq!(
            outcome_name(&Ok(VmState {
                schema_version: diag_ir::IR_SCHEMA_VERSION,
                pc: 0,
                stack: Vec::new(),
                locals: Vec::new(),
                globals: Vec::new(),
                call_stack: Vec::new(),
                steps: 0,
            })),
            "ok"
        );
        assert_eq!(
            outcome_name(&Err(JobError::IdentityMismatch {
                identity: diag_ir::IdentityKind::Vin
            })),
            "IdentityMismatch"
        );
        assert_eq!(
            outcome_name(&Err(JobError::CommLogMismatch)),
            "CommLogMismatch"
        );
    }
}

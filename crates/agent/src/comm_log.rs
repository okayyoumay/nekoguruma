//! The agent's communication log (design 8.11, ADR-346, ADR-347).
//!
//! One JSON Lines file per job and ownership generation, `{job_id}.g{generation}.commlog.jsonl`,
//! beside the job's journal. Every run of the job appends to the same file, under a writer lock
//! of its own (`{job_id}.g{generation}.commlog.lock`), so a second run of the job that finds the
//! log held runs without it. The log never steers a job: a failure to open it or to write it
//! changes the job's [`CommLogStatus`] and nothing else.
//!
//! The log holds the `header` (once per log), the `agent_clock` reading a reopen writes, a run's
//! `job_start` and `job_end`, and the exchange entries the host gives it ([`CommLog::exchange`]), classified against each target's
//! [`exchange::LinkState`], which the log keeps for the run. Every line has the next `seq` and an
//! `event` naming its kind. [`redaction::redact`] is how an exchange entry carries a message's
//! bytes.
//!
//! A log is closed cleanly when it holds its header and its last run's lines number on from the
//! largest `seq` before them, with nothing blank or damaged between them, to a complete
//! `job_end`; each run judges only the run before it. A log that
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
pub(crate) mod redaction;

/// The exchange entry, its evidence, the per-link history and the classification (design 8.11,
/// ADR-346 items 2, 3 and 6).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "evidence and origins the host does not give yet (transmit indications, VCI times, the runner's own reads)"
    )
)]
pub(crate) mod exchange;

/// Where a job keeps its communication log and under which key. The journaled entry points
/// derive it from the journal's setup (same directory and key); a job without a journal gets one
/// through `Job::with_comm_log`, or, when its procedure logs with `diag.log`, one of its own in
/// the directory `Job::with_comm_log_dir` gives ([`CommLogSetup::for_run`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommLogSetup {
    pub dir: PathBuf,
    pub key: JobKey,
}

/// A job key of its own for one run of `ngr-agent run`: a job ID minted from the clock, the
/// process ID and a count within the process, so two runs never share one, in generation 0.
pub fn run_key() -> JobKey {
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    JobKey {
        job_id: shared_proto::JobId(format!("run-{unix_ms}-{}-{run}", std::process::id())),
        generation: 0,
    }
}

impl CommLogSetup {
    /// A log of its own for one run in `dir`: a job ID minted from the clock, the process ID
    /// and a count within the process, so two runs never share a log, in generation 0
    /// (ADR-347, ADR-348 item 6). On Unix a missing directory is created writable and readable
    /// by its owner only, since the log holds the vehicle's data; an existing one keeps its
    /// permissions, and the log file itself is its owner's alone ([`CommLog`]).
    pub fn for_run(dir: PathBuf) -> io::Result<Self> {
        Self::for_run_key(dir, run_key())
    }

    /// [`for_run`](Self::for_run) with the run's key minted already ([`run_key`]), so the
    /// operational log's job span names the same job (ADR-348 item 3).
    pub fn for_run_key(dir: PathBuf, key: JobKey) -> io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir)?;
        Ok(Self { dir, key })
    }

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
    /// The VCI the worker was launched for, when the job was told (`Job::with_vci`).
    pub vci: Option<VciFacts>,
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
    ProcedureLog(ProcedureLog<'a>),
    VciConnected(VciConnected<'a>),
    LinkOpened(LinkOpened<'a>),
    Identity(Identity),
    Unattached(UnattachedEntry<'a>),
}

/// What the agent knows of the VCI it launched the job's worker for: the API its library
/// implements and the `unsigned long` width the worker runs with (design 7.1.2), for
/// `vci_connected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VciFacts {
    pub api: crate::launch::Api,
    pub long_size: u8,
}

/// The VCI's connection (design 8.11 "Header"): written when the job's link has opened the
/// module, before `link_opened`.
#[derive(Serialize)]
struct VciConnected<'a> {
    /// `dpdu` (ISO 22900-2:2022 clause 8) or `j2534_0404` (SAE J2534-1 v04.04 clause 7);
    /// absent when the job was not told which.
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<&'static str>,
    /// The `unsigned long` width a J2534 worker runs with (design 7.1.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    unsigned_long_bytes: Option<u8>,
    /// The worker's version data as far as the API gives it; `null` when the worker gave none.
    version: Option<VciVersion<'a>>,
    /// How far the VCI's clock may run from the agent's, and the tolerance of one clock reading:
    /// neither is configured yet, so the log uses none.
    drift_bound: Configured,
    clock_margin: Configured,
}

/// A worker's version data. J2534 v04.04's `PassThruReadVersion` gives the firmware, library
/// and API versions only, which the J2534 worker passes on as the hardware name, the firmware
/// name and the API version number; a D-PDU API module gives the rest as well.
#[derive(Serialize)]
#[serde(untagged)]
enum VciVersion<'a> {
    J2534 {
        firmware_version: &'a str,
        library_version: &'a str,
        api_version: u32,
    },
    Dpdu {
        vendor: &'a str,
        hardware_name: &'a str,
        hardware_serial_number: u32,
        hardware_version: u32,
        firmware_name: &'a str,
        firmware_version: u32,
        api_library_name: &'a str,
        api_library_version: u32,
    },
}

/// A value the log uses and where it came from (design 8.11 "Drift bound").
#[derive(Serialize)]
struct Configured {
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<u32>,
    source: &'static str,
}

/// An ECU identity value the runner read, as it was read (design 8.11 "Header"). The log has
/// the journal's protection, so it may hold the VIN.
#[derive(Serialize)]
struct Identity {
    kind: &'static str,
    /// `value`, `negative_response` or `unreadable`.
    outcome: &'static str,
    /// The value as text, when it is printable ASCII.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// The value's bytes in hex, when it is not printable ASCII.
    #[serde(skip_serializing_if = "Option::is_none")]
    hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nrc: Option<u8>,
}

/// An item of a link that the host read but that no exchange takes (design 8.11 "Link settings
/// for the log", ADR-346 item 4): one with no primitive (a transmit indication, a link error) or
/// one of an earlier primitive. The log attaches none of them to an exchange yet, so each is
/// logged `unattached`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnattachedItem {
    pub link: u32,
    /// Whether it named an earlier primitive; `false` for an item of no primitive.
    pub earlier_primitive: bool,
    /// The item's VCI time, raw (`EventItem.timestamp`).
    pub vci_us: u32,
    /// The agent's clocks when the host read it.
    pub read: Option<AgentNow>,
    pub data: UnattachedData,
}

/// What an [`UnattachedItem`] carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnattachedData {
    /// A result item: a received message or an indication, told apart by its RxFlag, with the
    /// CAN identifier its header carries, when it carries one.
    Result {
        rx_flag: Vec<u8>,
        identifier: Option<u32>,
        bytes: Vec<u8>,
    },
    /// An error event, with its extra error information as the worker gave it (ADR-351).
    Error {
        event: String,
        extra_error_info: Option<u32>,
        extra_error_info_name: Option<String>,
    },
}

impl UnattachedItem {
    /// Whether this is a transmit indication (RxFlag `TX_INDICATION`) for `identifier`.
    fn is_tx_indication_for(&self, identifier: u32) -> bool {
        matches!(
            &self.data,
            UnattachedData::Result { rx_flag, identifier: Some(id), .. }
                if rx_flag.get(3).is_some_and(|flags| flags & RX_FLAG_TX_INDICATION != 0)
                    && *id == identifier
        )
    }
}

/// RxFlag byte 3's bits (ISO 22900-2:2022 D.2.2, Table D.5), which say what a result item is.
const RX_FLAG_TX_MSG_TYPE: u8 = 0x01;
const RX_FLAG_START_OF_MESSAGE: u8 = 0x02;
const RX_FLAG_BREAK: u8 = 0x04;
const RX_FLAG_TX_INDICATION: u8 = 0x08;

/// The `unattached` event of an [`UnattachedItem`].
#[derive(Serialize)]
struct UnattachedEntry<'a> {
    link: u32,
    /// `transmit_indication`, `loopback`, `start_of_message`, `break`, `received` (a message
    /// another node sent: a response, a pending, or anything else), or `error_event`.
    item: &'static str,
    /// `none` or `earlier`.
    primitive: &'static str,
    /// The item's VCI time, raw. It names no clock (design 8.11 "On one clock"): the worker
    /// does not say which clock an item's time is on, and the agent does not infer it from the
    /// worker (design principle 6).
    vci_us: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<ReadAt>,
    /// The result item's RxFlag in hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    rx_flag: Option<String>,
    /// The CAN identifier the result item's header carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<u32>,
    /// The result item's message, as the log carries messages: what the tester sent for a
    /// transmit indication or a loopback, what it received otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<redaction::LoggedMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_event: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_error_info: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_error_info_name: Option<&'a str>,
}

/// When the host read an item, on the agent's clock.
#[derive(Serialize)]
struct ReadAt {
    epoch: u32,
    read_us: u64,
}

impl<'a> UnattachedEntry<'a> {
    fn of(item: &'a UnattachedItem) -> Self {
        use redaction::{Direction, redact};
        let mut entry = Self {
            link: item.link,
            item: "error_event",
            primitive: if item.earlier_primitive {
                "earlier"
            } else {
                "none"
            },
            vci_us: item.vci_us,
            agent: item.read.map(|read| ReadAt {
                epoch: read.epoch,
                read_us: read.monotonic_us,
            }),
            rx_flag: None,
            identifier: None,
            message: None,
            error_event: None,
            extra_error_info: None,
            extra_error_info_name: None,
        };
        match &item.data {
            UnattachedData::Result {
                rx_flag,
                identifier,
                bytes,
            } => {
                let flags = rx_flag.get(3).copied().unwrap_or(0);
                let (kind, direction) = if flags & RX_FLAG_TX_INDICATION != 0 {
                    ("transmit_indication", Direction::Request)
                } else if flags & RX_FLAG_TX_MSG_TYPE != 0 {
                    ("loopback", Direction::Request)
                } else if flags & RX_FLAG_START_OF_MESSAGE != 0 {
                    ("start_of_message", Direction::Response)
                } else if flags & RX_FLAG_BREAK != 0 {
                    ("break", Direction::Response)
                } else {
                    ("received", Direction::Response)
                };
                entry.item = kind;
                entry.rx_flag = Some(to_hex(rx_flag));
                entry.identifier = *identifier;
                entry.message = Some(redact(direction, bytes));
            }
            UnattachedData::Error {
                event,
                extra_error_info,
                extra_error_info_name,
            } => {
                entry.error_event = Some(event);
                entry.extra_error_info = *extra_error_info;
                entry.extra_error_info_name = extra_error_info_name.as_deref();
            }
        }
        entry
    }
}

/// What a link the job opened has in force (design 8.11 "Header", ADR-346): written right
/// after the link opens, before anything is sent on it.
#[derive(Serialize)]
struct LinkOpened<'a> {
    link: u32,
    /// The endpoint: roles are not bound on the agent's one link.
    target: exchange::Target,
    /// The physical channel the link shares with any other link of the job: the agent opens
    /// one link per job, on the worker's one module.
    channel: u32,
    protocol: &'a str,
    baud_rate: u32,
    request_id: u32,
    response_id: u32,
    /// Every ComParam by name: those the link sets, those the document declares, and the
    /// timing ones design 8.11 lists that neither sets, which keep the library's default.
    comparams: Vec<ComParamInForce<'a>>,
    /// What can confirm a request's transmission on this link: `transmit_indication` when the
    /// library accepted `CP_TransmitIndEnable`. The link turns no loopback on.
    transmit_confirmation: Vec<&'static str>,
    /// Whether response times can be judged on the link: not without a transmit time.
    response_times_judged: bool,
    /// Whether the worker passes each response pending on: not known to the agent yet, since
    /// it depends on the worker (the J2534 worker forwards them, a D-PDU library may absorb
    /// them), so `null` rather than a guess.
    forwards_pendings: Option<bool>,
    timing_margin: TimingMargin,
}

#[derive(Serialize)]
struct ComParamInForce<'a> {
    name: &'a str,
    /// The value set, absent when it keeps the library's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<u32>,
    set_by: SetBy,
    /// For an indication the link turns on (`link::INDICATION_COMPARAMS`): whether the library
    /// accepted it. A refused one leaves the link without the indication.
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SetBy {
    /// The agent's link sets it, to the agent's own value (`crate::link`).
    Link,
    /// Its value comes from the document (ADR-324): one the document declares and the link
    /// passes on (item 3), or one the link sets to the document's value (the baud rate, a
    /// declared timing ComParam, rounded up to whole milliseconds).
    Document,
    /// Neither sets it: the library's default is in force, and the log does not know it.
    LibraryDefault,
}

/// The timing margin in use and where it came from: none is configured yet, so no time is
/// compared with a limit beyond one.
#[derive(Serialize)]
struct TimingMargin {
    #[serde(skip_serializing_if = "Option::is_none")]
    ms: Option<u32>,
    source: &'static str,
}

/// The timing ComParams design 8.11 has `link_opened` name even when the link leaves them to
/// the library; the link sets the others it lists. `CP_P1Max` is left out: CAN has no
/// inter-byte time.
const LOGGED_TIMING_COMPARAMS: [&str; 1] = ["CP_TesterPresentHandling"];

/// A message the procedure logged with `diag.log`, with its level (design 8.11, ADR-348 item
/// 6). It may hold vehicle data, so it is kept here and never in the operational log.
#[derive(Serialize)]
struct ProcedureLog<'a> {
    level: u8,
    message: &'a str,
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
            JobError::NoCommLogDir => "NoCommLogDir",
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
    /// The link state of each target the run's exchanges went to, in the order of their first
    /// exchange. It lives as long as the log, so it outlives a link, a worker or a VCI connection
    /// within the run; a later run of the job starts it over, since the log is not read back.
    links: Vec<(exchange::Target, exchange::LinkState)>,
    /// The run's VCI, from its facts, for `vci_connected`.
    vci: Option<VciFacts>,
}

/// The agent's clocks at one moment, as an exchange's `times` give them: the agent epoch, the
/// monotonic microseconds since the log began, and the wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentNow {
    pub epoch: u32,
    pub monotonic_us: u64,
    pub wall_unix_ms: u64,
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
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        // The log holds the vehicle's data: on Unix a new one is its owner's alone. A file that
        // exists keeps its mode.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        // Created here, or found: an empty file found is a run that stopped before its header.
        let (mut file, created) = match options.clone().create_new(true).open(&path) {
            Ok(file) => (file, true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                (options.open(&path).map_err(|error| error.kind())?, false)
            }
            Err(error) => return Err(error.kind()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| error.kind())?;
        if bytes.is_empty() {
            // A new file's name is durable only once its directory is (as the journal's). An
            // empty file may be one whose earlier sync failed, so it is synced again.
            sync_dir(&setup.dir).map_err(|error| error.kind())?;
        }
        let mut scan = scan(&bytes).ok_or(io::ErrorKind::InvalidData)?;
        scan.found_empty = !created && bytes.is_empty();
        let io_kind = |error: io::Error| error.kind();
        if scan.complete_len < bytes.len() {
            tracing::warn!(
                external = %crate::oplog::External(path.display()),
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
        // A log goes on after an earlier run once it holds a header; otherwise a header is
        // written after whatever the file holds, marked incomplete.
        let reopened = scan.header;
        Self(Arc::new(Mutex::new(Inner {
            sink,
            next_seq: scan.next_seq,
            failed: None,
            unclean: scan.torn
                || scan.found_empty
                || (scan.complete_len > 0 && !(scan.header && scan.consistent && scan.clean_close)),
            // One more than any epoch the file holds, so two agents never share one.
            epoch: scan.next_epoch,
            origin: Instant::now(),
            opening: Some(if reopened {
                Opening::Reopened
            } else {
                Opening::Fresh
            }),
            ended: false,
            _lock: lock,
            vci: None,
            links: Vec::new(),
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
        inner.vci = facts.vci;
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

    /// Whether the log still takes lines: it has not failed.
    pub(crate) fn is_open(&self) -> bool {
        self.lock().failed.is_none()
    }

    /// The agent's clocks now, for an exchange's times.
    pub(crate) fn now(&self) -> AgentNow {
        let clock = self.lock().clock();
        AgentNow {
            epoch: u32::try_from(clock.epoch).unwrap_or(u32::MAX),
            monotonic_us: clock.monotonic_us,
            wall_unix_ms: clock.wall_unix_ms,
        }
    }

    /// Writes the exchange entry of `evidence` as the next line (design 8.11 "Exchange entry"),
    /// classified against its target's link state, and takes the entry into that state. A link
    /// error the entry carries ends the matched answers of every other target too: the run's
    /// targets share its one link, and so its physical channel. The entry's `seq` when it was
    /// written; `None` once the log has failed, and then the link state is left as it was.
    #[cfg(test)]
    pub(crate) fn exchange(&self, evidence: exchange::Evidence) -> Option<u64> {
        self.exchange_with(evidence, Vec::new())
    }

    /// Writes the exchange entry of `evidence` as [`CommLog::exchange`] does, after the items of
    /// the link the host read during the exchange that no primitive or an earlier one owned
    /// (`unbound`, in the order read). A transmit indication among them is the exchange's send
    /// evidence (`tx_indication`, design 8.11 `sent_evidence`) when it carries the exchange's
    /// request identifier and no ended exchange on the link could still own it; the others are
    /// `unattached` events, written before the entry. The other conditions design 8.11 sets
    /// hold on the agent's link by construction: the job runs one primitive at a time on its one
    /// link, so no other exchange on the physical channel is in flight, and the link starts no
    /// communication (`PDU_COPT_STARTCOMM`), so the API sends no tester present of its own (ISO
    /// 22900-2:2022 enables it only once communication has started).
    pub(crate) fn exchange_with(
        &self,
        mut evidence: exchange::Evidence,
        mut unbound: Vec<UnattachedItem>,
    ) -> Option<u64> {
        let mut inner = self.lock();
        if inner.failed.is_some() {
            return None;
        }
        let at = match inner
            .links
            .iter()
            .position(|(target, _)| *target == evidence.target)
        {
            Some(at) => at,
            None => {
                inner
                    .links
                    .push((evidence.target.clone(), exchange::LinkState::default()));
                inner.links.len() - 1
            }
        };
        if let exchange::Target::Endpoint { tx_id, .. } = evidence.target
            && evidence.transmit_evidence.is_none()
            && !inner.links[at].1.may_own_tx_indication()
            && let Some(own) = unbound
                .iter()
                .position(|item| item.is_tx_indication_for(tx_id))
        {
            unbound.remove(own);
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxIndication);
        }
        for item in &unbound {
            let event = Event::Unattached(UnattachedEntry::of(item));
            inner.append_with(|seq, line| {
                serde_json::to_writer(line, &Line { seq, event: &event })
            })?;
        }
        let history = inner.links[at].1.history_for(&evidence);
        let entry = exchange::ExchangeEntry::build(inner.next_seq, evidence, history);
        let seq = inner.append_with(|_, line| serde_json::to_writer(line, &entry))?;
        inner.links[at].1.record(&entry);
        if let Some(kind) = entry.link_error() {
            for (other, (_, state)) in inner.links.iter_mut().enumerate() {
                if other != at {
                    state.channel_link_error(seq, kind);
                }
            }
        }
        Some(seq)
    }

    /// Writes the `link_opened` event of the link `config` opened, as link `link` (design 8.11
    /// "Header"); `valued_by_document` names the link's own ComParams whose value the document
    /// gave (`link::valued_by_document`), and `indications` those of its indications the library
    /// accepted. The event's `seq`; `None` once the log has failed.
    pub(crate) fn link_opened(
        &self,
        link: u32,
        config: &crate::link::LinkConfig,
        valued_by_document: &[&str],
        indications: crate::link::Indications,
    ) -> Option<u64> {
        let mut comparams: Vec<ComParamInForce<'_>> = crate::link::own_comparams(config)
            .into_iter()
            .map(|(name, value)| ComParamInForce {
                name,
                value: Some(value),
                set_by: if valued_by_document.contains(&name) {
                    SetBy::Document
                } else {
                    SetBy::Link
                },
                accepted: None,
            })
            .collect();
        comparams.extend(crate::link::INDICATION_COMPARAMS.into_iter().map(|name| {
            ComParamInForce {
                name,
                value: Some(1),
                set_by: SetBy::Link,
                accepted: Some(indications.accepted(name)),
            }
        }));
        comparams.extend(config.params.iter().map(|(name, value)| ComParamInForce {
            name,
            value: Some(*value),
            set_by: SetBy::Document,
            accepted: None,
        }));
        for name in LOGGED_TIMING_COMPARAMS {
            if !comparams
                .iter()
                .any(|param| param.name.eq_ignore_ascii_case(name))
            {
                comparams.push(ComParamInForce {
                    name,
                    value: None,
                    set_by: SetBy::LibraryDefault,
                    accepted: None,
                });
            }
        }
        let event = Event::LinkOpened(LinkOpened {
            link,
            target: exchange::Target::Endpoint {
                link,
                tx_id: config.tx_id,
                rx_id: config.rx_id,
            },
            channel: 1,
            protocol: &config.protocol_short_name,
            baud_rate: config.baud_rate,
            request_id: config.tx_id,
            response_id: config.rx_id,
            comparams,
            transmit_confirmation: if indications.transmit {
                vec!["transmit_indication"]
            } else {
                Vec::new()
            },
            response_times_judged: false,
            forwards_pendings: None,
            timing_margin: TimingMargin {
                ms: None,
                source: "none",
            },
        });
        let mut inner = self.lock();
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
    }

    /// Writes the `vci_connected` event of the job's VCI (design 8.11 "Header"): the VCI as the
    /// agent launched it, when the run's facts name it, and the module's `version` data, when
    /// the worker gave it. The event's `seq`; `None` once the log has failed.
    pub(crate) fn vci_connected(
        &self,
        version: Option<&vci_service_interface::VersionData>,
    ) -> Option<u64> {
        use crate::launch::Api;
        let vci = self.lock().vci;
        let api = vci.map(|vci| vci.api);
        let event = Event::VciConnected(VciConnected {
            api: api.map(|api| match api {
                Api::Dpdu => "dpdu",
                Api::J2534 => "j2534_0404",
            }),
            unsigned_long_bytes: vci
                .filter(|vci| vci.api == Api::J2534)
                .map(|vci| vci.long_size),
            version: version.map(|v| match api {
                Some(Api::J2534) => VciVersion::J2534 {
                    firmware_version: &v.hw_name,
                    library_version: &v.fw_name,
                    api_version: v.pdu_api_sw_version,
                },
                _ => VciVersion::Dpdu {
                    vendor: &v.vendor_name,
                    hardware_name: &v.hw_name,
                    hardware_serial_number: v.hw_serial_number,
                    hardware_version: v.hw_version,
                    firmware_name: &v.fw_name,
                    firmware_version: v.fw_version,
                    api_library_name: &v.pdu_api_sw_name,
                    api_library_version: v.pdu_api_sw_version,
                },
            }),
            drift_bound: Configured {
                value: None,
                source: "none",
            },
            clock_margin: Configured {
                value: None,
                source: "none",
            },
        });
        let mut inner = self.lock();
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
    }

    /// Writes an `identity` event for the value of `kind` the runner read (design 8.11
    /// "Header"). The event's `seq`; `None` once the log has failed.
    pub(crate) fn identity(
        &self,
        kind: diag_ir::IdentityKind,
        read: diag_ir::IdentityRead<'_>,
    ) -> Option<u64> {
        use diag_ir::{IdentityKind, IdentityRead};
        let printable = |bytes: &[u8]| bytes.iter().all(|b| (0x20..0x7F).contains(b));
        let (outcome, text, hex, nrc) = match read {
            IdentityRead::Value(bytes) if printable(bytes) => (
                "value",
                Some(String::from_utf8_lossy(bytes).into_owned()),
                None,
                None,
            ),
            IdentityRead::Value(bytes) => ("value", None, Some(to_hex(bytes)), None),
            IdentityRead::Negative(nrc) => ("negative_response", None, None, Some(nrc)),
            IdentityRead::Unreadable => ("unreadable", None, None, None),
        };
        let event = Event::Identity(Identity {
            kind: match kind {
                IdentityKind::Vin => "vin",
                IdentityKind::HardwarePartNumber => "hardware_part_number",
                IdentityKind::SoftwareVersion => "software_version",
            },
            outcome,
            text,
            hex,
            nrc,
        });
        let mut inner = self.lock();
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
    }

    /// Writes `item` as an `unattached` event (design 8.11 "Link settings for the log"). The
    /// event's `seq`; `None` once the log has failed.
    #[cfg(test)]
    pub(crate) fn unattached(&self, item: &UnattachedItem) -> Option<u64> {
        let mut inner = self.lock();
        let event = Event::Unattached(UnattachedEntry::of(item));
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
    }

    /// Writes a `diag.log` message as a `procedure_log` event (design 8.11, ADR-348 item 6).
    /// The event's `seq`; `None` once the log has failed.
    pub(crate) fn procedure_log(&self, level: u8, message: &str) -> Option<u64> {
        let mut inner = self.lock();
        let event = Event::ProcedureLog(ProcedureLog { level, message });
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
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
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event }));
    }

    /// Writes the next line, which `serialize` puts into its buffer for the line's `seq`,
    /// unless the log has failed. The `seq` when the line was written; a failure is recorded
    /// and stops the log.
    fn append_with(
        &mut self,
        serialize: impl FnOnce(u64, &mut Vec<u8>) -> serde_json::Result<()>,
    ) -> Option<u64> {
        if self.failed.is_some() {
            return None;
        }
        let seq = self.next_seq;
        let Some(next) = seq.checked_add(1) else {
            self.failed = Some(CommLogIncomplete::WriteFailed {
                seq,
                kind: io::ErrorKind::InvalidData,
            });
            return None;
        };
        let mut line = Vec::with_capacity(256);
        let written = serialize(seq, &mut line)
            .map_err(io::Error::from)
            .and_then(|()| {
                line.push(b'\n');
                self.sink.write_line(&line)
            });
        match written {
            Ok(()) => {
                self.next_seq = next;
                Some(seq)
            }
            Err(error) => {
                self.failed = Some(CommLogIncomplete::WriteFailed {
                    seq,
                    kind: error.kind(),
                });
                None
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
    /// One of them is a header.
    header: bool,
    /// The lines from the last opening line (a header or an `agent_clock`) on are each a JSON
    /// object whose `seq` is one more than the largest before it (the first line's 0), with no
    /// blank line between: nothing the last run wrote is missing or damaged.
    consistent: bool,
    /// The `seq` the next line takes: one more than the largest `seq` read, so numbering never
    /// goes back.
    next_seq: u64,
    /// The agent epoch the next reopen starts: one more than the largest read.
    next_epoch: u64,
    /// The last of them is a `job_end`.
    clean_close: bool,
    /// The file was there, empty: an earlier run created it and stopped before its header.
    found_empty: bool,
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
        ..Scan::default()
    };
    let mut largest_seq: Option<u64> = None;
    let mut largest_epoch: Option<u64> = None;
    // Whether the lines since the last opening line (a header or an `agent_clock`, which starts
    // a run) are whole; `None` before the first. A run reports what it finds since the run
    // before it began: damage further back was reported by the run that found it.
    let mut since_opening: Option<bool> = None;
    for line in bytes[..complete_len].split_inclusive(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            // A blank line is no line of this writer's.
            since_opening = since_opening.map(|_| false);
            continue;
        }
        scan.lines += 1;
        scan.clean_close = false;
        let parsed = serde_json::from_slice::<serde_json::Value>(line).ok();
        let object = parsed.as_ref().and_then(|value| value.as_object());
        let seq = object.and_then(|object| object.get("seq")?.as_u64());
        let event = object.and_then(|object| object.get("event")?.as_str());
        let expected = largest_seq.map_or(Some(0), |seq| seq.checked_add(1));
        let whole = seq.is_some() && seq == expected;
        // A gap just before an `agent_clock` is the run before it losing its tail, which the run
        // that wrote the `agent_clock` reported: the check starts again there.
        let opens = seq.is_some();
        if let Some(seq) = seq {
            largest_seq = Some(largest_seq.map_or(seq, |largest| largest.max(seq)));
        }
        let epoch = match event {
            Some("header") => {
                scan.header = true;
                // A header starts the log: lines before it with a `seq` were lost from it.
                since_opening = Some(whole);
                // A header without a readable epoch began epoch 0.
                Some(
                    object
                        .and_then(|object| object.get("clock")?.get("epoch")?.as_u64())
                        .unwrap_or(0),
                )
            }
            Some("agent_clock") => {
                since_opening = Some(opens);
                object.and_then(|object| object.get("epoch")?.as_u64())
            }
            _ => {
                since_opening = since_opening.map(|before| before && whole);
                if event == Some("job_end") {
                    scan.clean_close = true;
                }
                None
            }
        };
        if let Some(epoch) = epoch {
            largest_epoch = Some(largest_epoch.map_or(epoch, |largest| largest.max(epoch)));
        }
    }
    scan.consistent = since_opening == Some(true);
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
        // The log never changes the job's result, a panic while opening it included.
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            CommLog::open(setup).inspect(|log| log.begin(facts, run))
        }))
        .unwrap_or(Err(CommLogIncomplete::NotOpened(io::ErrorKind::Other)));
        match opened {
            Ok(log) => {
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
            vci: None,
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

    /// Each identity value as it was read: printable text as text (the VIN included, which the
    /// log may hold), other bytes in hex, a negative response with its code, and an answer
    /// without a value as unreadable.
    #[test]
    fn identity_events_hold_each_value_as_it_was_read() {
        use diag_ir::{IdentityKind, IdentityRead};
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        log.identity(IdentityKind::Vin, IdentityRead::Value(b"WVWZZZ1JZXW000001"));
        log.identity(
            IdentityKind::HardwarePartNumber,
            IdentityRead::Value(&[0x01, 0xAB, 0x7F]),
        );
        log.identity(IdentityKind::SoftwareVersion, IdentityRead::Negative(0x31));
        log.identity(IdentityKind::SoftwareVersion, IdentityRead::Unreadable);
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let identities: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "identity")
            .map(|line| {
                let mut line = line.clone();
                line.as_object_mut().unwrap().remove("seq");
                line
            })
            .collect();
        assert_eq!(
            identities,
            [
                serde_json::json!({"event": "identity", "kind": "vin", "outcome": "value",
                    "text": "WVWZZZ1JZXW000001"}),
                serde_json::json!({"event": "identity", "kind": "hardware_part_number",
                    "outcome": "value", "hex": "01ab7f"}),
                serde_json::json!({"event": "identity", "kind": "software_version",
                    "outcome": "negative_response", "nrc": 0x31}),
                serde_json::json!({"event": "identity", "kind": "software_version",
                    "outcome": "unreadable"}),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Design 8.11 "Link settings for the log": an item no exchange takes is an `unattached`
    /// event that says what it is by its RxFlag (ISO 22900-2:2022 D.2.2) and carries its message
    /// as every message is carried (a loopback echoes what the tester sent, so a key in it stays
    /// hidden). Its VCI time names no clock, whatever API the job was told (design principle 6).
    #[test]
    fn unattached_items_say_what_they_are_and_keep_secrets_out() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(
            &RunFacts {
                vci: Some(VciFacts {
                    api: crate::launch::Api::J2534,
                    long_size: 8,
                }),
                ..facts()
            },
            RunKind::First,
        );
        let read = AgentNow {
            epoch: 0,
            monotonic_us: 1_500,
            wall_unix_ms: 0,
        };
        let result = |earlier_primitive, rx_flag: &[u8], bytes: &[u8]| UnattachedItem {
            link: 1,
            earlier_primitive,
            vci_us: 42,
            read: Some(read),
            data: UnattachedData::Result {
                rx_flag: rx_flag.to_vec(),
                // A transmit indication names the request identifier it confirms.
                identifier: (rx_flag.get(3) == Some(&0x09)).then_some(0x7E0),
                bytes: bytes.to_vec(),
            },
        };
        let items = [
            result(false, &[0, 0, 0, 0x09], &[]),
            // A SecurityAccess key request echoed back.
            result(false, &[0, 0, 0, 0x01], &[0x27, 0x02, 0xAA, 0xBB]),
            result(false, &[0, 0, 0, 0x02], &[]),
            result(true, &[], &[0x62, 0xF1, 0x90, 0x41]),
            UnattachedItem {
                link: 1,
                earlier_primitive: true,
                vci_us: 43,
                read: None,
                data: UnattachedData::Error {
                    event: "PDU_ERR_EVT_RX_TIMEOUT".to_owned(),
                    extra_error_info: None,
                    extra_error_info_name: Some("ISO15765_CF_TIMEOUT".to_owned()),
                },
            },
        ];
        for item in &items {
            log.unattached(item).unwrap();
        }
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let unattached: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "unattached")
            .map(|line| {
                let mut line = line.clone();
                line.as_object_mut().unwrap().remove("seq");
                line
            })
            .collect();
        let agent = serde_json::json!({"epoch": 0, "read_us": 1_500});
        assert_eq!(
            unattached,
            [
                serde_json::json!({"event": "unattached", "link": 1,
                    "item": "transmit_indication", "primitive": "none", "vci_us": 42,
                    "agent": agent, "rx_flag": "00000009", "identifier": 0x7E0,
                    "message": {"clear": ""}}),
                serde_json::json!({"event": "unattached", "link": 1, "item": "loopback",
                    "primitive": "none", "vci_us": 42, "agent": agent,
                    "rx_flag": "00000001",
                    "message": {"clear": "2702", "data": {"kind": "secret", "len": 2}}}),
                serde_json::json!({"event": "unattached", "link": 1,
                    "item": "start_of_message", "primitive": "none", "vci_us": 42,
                    "agent": agent, "rx_flag": "00000002",
                    "message": {"clear": ""}}),
                serde_json::json!({"event": "unattached", "link": 1, "item": "received",
                    "primitive": "earlier", "vci_us": 42, "agent": agent,
                    "rx_flag": "", "message": {"clear": "62f19041"}}),
                serde_json::json!({"event": "unattached", "link": 1, "item": "error_event",
                    "primitive": "earlier", "vci_us": 43,
                    "error_event": "PDU_ERR_EVT_RX_TIMEOUT",
                    "extra_error_info_name": "ISO15765_CF_TIMEOUT"}),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `link_opened` names a link's transmit indication as a transmit-confirmation source only
    /// when the library accepted `CP_TransmitIndEnable`.
    #[test]
    fn transmit_confirmation_names_the_indication_the_library_accepted() {
        for (transmit, expected) in [
            (true, serde_json::json!(["transmit_indication"])),
            (false, serde_json::json!([])),
        ] {
            let dir = dir();
            let setup = setup(&dir);
            let log = CommLog::open(&setup).unwrap();
            log.begin(&facts(), RunKind::First);
            log.link_opened(
                1,
                &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
                &[],
                crate::link::Indications {
                    transmit,
                    start_of_message: true,
                },
            )
            .unwrap();
            log.end("ok");
            let lines = lines(&setup);
            let opened = lines
                .iter()
                .find(|line| line["event"] == "link_opened")
                .unwrap();
            assert_eq!(opened["transmit_confirmation"], expected, "{transmit}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// An unattached error event gives its extra error code as the worker gave it (ADR-351).
    #[test]
    fn an_unattached_error_gives_its_extra_error_code() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        log.unattached(&UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us: 7,
            read: None,
            data: UnattachedData::Error {
                event: "PDU_ERR_EVT_FRAME_STRUCT".to_owned(),
                extra_error_info: Some(9),
                extra_error_info_name: None,
            },
        })
        .unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let line = lines
            .iter()
            .find(|line| line["event"] == "unattached")
            .unwrap();
        assert!(line.get("clock").is_none(), "{line}");
        assert_eq!(line["extra_error_info"], 9);
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
    fn the_epoch_goes_on_after_the_largest_and_every_header_counts() {
        let bytes = b"{\"seq\":0,\"event\":\"header\",\"clock\":{\"epoch\":0}}\n{\"seq\":1,\"event\":\"agent_clock\",\"epoch\":5}\n{\"seq\":2,\"event\":\"agent_clock\",\"epoch\":2}\n{\"seq\":3,\"event\":\"job_end\"}\n";
        assert_eq!(
            scan(bytes).unwrap().next_epoch,
            6,
            "the largest, not the last"
        );
        // A header that is not first, with an epoch of its own, counts as well.
        let later = b"junk\n{\"seq\":0,\"event\":\"header\",\"clock\":{\"epoch\":4}}\n";
        let scan = scan(later).unwrap();
        assert!(scan.header);
        assert_eq!(scan.next_epoch, 5);
    }

    #[test]
    fn two_runs_over_a_file_without_a_header_get_their_own_epochs_and_one_header() {
        let first = Recording::default();
        let log = CommLog::over(Box::new(first.clone()), b"junk\n");
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        let mut bytes = b"junk\n".to_vec();
        bytes.extend_from_slice(&first.taken.lock().unwrap());
        let second = Recording::default();
        let log = CommLog::over(Box::new(second.clone()), &bytes);
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        let lines = second.lines();
        assert_eq!(
            lines[0]["event"], "agent_clock",
            "the header is written once"
        );
        assert_eq!(lines[0]["epoch"], 1);
        assert_eq!(first.lines()[0]["clock"]["epoch"], 0);
    }

    #[test]
    fn damage_an_earlier_run_reported_does_not_mark_later_runs() {
        // Run 1 lost seq 2; run 2 reopened it as unclean and closed cleanly.
        let bytes = b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":1,\"event\":\"job_start\"}\n{\"seq\":3,\"event\":\"agent_clock\",\"epoch\":1,\"incomplete\":true}\n{\"seq\":4,\"event\":\"job_start\"}\n{\"seq\":5,\"event\":\"job_end\"}\n";
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), bytes);
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(log.status(), CommLogStatus::Complete);
        assert!(sink.lines()[0].get("incomplete").is_none());
        // A gap in the last run is still found.
        let gap = b"{\"seq\":0,\"event\":\"header\"}\n{\"seq\":1,\"event\":\"agent_clock\",\"epoch\":1}\n{\"seq\":3,\"event\":\"job_end\"}\n";
        let log = CommLog::over(Box::new(Recording::default()), gap);
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
    }

    #[test]
    fn a_first_header_whose_seq_is_not_zero_is_not_a_clean_start() {
        let bytes = b"{\"seq\":5,\"event\":\"header\"}\n{\"seq\":6,\"event\":\"job_end\"}\n";
        let log = CommLog::over(Box::new(Recording::default()), bytes);
        log.begin(&facts(), RunKind::Resume);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
    }

    #[test]
    fn an_empty_log_an_earlier_run_left_is_an_unclean_reopen() {
        let dir = dir();
        let setup = setup(&dir);
        // A run that created the file and stopped before its header.
        std::fs::write(setup.path().unwrap(), b"").unwrap();
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        assert_eq!(
            log.status(),
            CommLogStatus::Incomplete(CommLogIncomplete::UncleanReopen)
        );
        drop(log);
        let text = std::fs::read_to_string(setup.path().unwrap()).unwrap();
        let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(header["event"], "header");
        assert_eq!(header["clock"]["incomplete"], true);
        // A file this open creates is a clean start.
        std::fs::remove_file(setup.path().unwrap()).unwrap();
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        assert_eq!(log.status(), CommLogStatus::Complete);
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_new_log_file_is_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        log.end("ok");
        let mode = std::fs::metadata(setup.path().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
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

    // ---- exchange entries (design 8.11 "Exchange entry") ----

    fn evidence(
        target: exchange::Target,
        request: &[u8],
        end: exchange::End,
    ) -> exchange::Evidence {
        exchange::Evidence::new(
            exchange::Origin::Procedure,
            target,
            1,
            0x7E8,
            request.to_vec(),
            end,
        )
    }

    fn endpoint(tx_id: u32) -> exchange::Target {
        exchange::Target::Endpoint {
            link: 1,
            tx_id,
            rx_id: 0x7E8,
        }
    }

    /// A TxDone indication for `identifier`, read during the exchange.
    fn tx_done(identifier: u32) -> UnattachedItem {
        UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us: 5,
            read: None,
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0x09],
                identifier: Some(identifier),
                bytes: Vec::new(),
            },
        }
    }

    /// Design 8.11 `sent_evidence`: an unbound transmit indication confirms the send of the
    /// exchange it was read in when it carries that exchange's request identifier and no ended
    /// exchange on the link could still own it (one with `sent` `unknown`); otherwise it is
    /// `unattached`, written before the entry.
    #[test]
    fn an_unbound_transmit_indication_confirms_its_exchange_unless_another_could_own_it() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let silent = || {
            evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::NoResponse,
            )
        };
        // Another identifier's indication stays unattached; this exchange's confirms it.
        log.exchange_with(silent(), vec![tx_done(0x7E1), tx_done(0x7E0)])
            .unwrap();
        // No indication: `sent` `unknown`, which could own a late one.
        log.exchange_with(silent(), Vec::new()).unwrap();
        // So the next exchange's indication is unattached, and its send stays unknown.
        log.exchange_with(silent(), vec![tx_done(0x7E0)]).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let events = events(&lines);
        assert_eq!(
            &events[2..events.len() - 1],
            [
                "unattached",
                "exchange",
                "exchange",
                "unattached",
                "exchange"
            ]
        );
        let entries: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        assert_eq!(entries[0]["sent"], "confirmed");
        assert_eq!(entries[0]["sent_evidence"], "tx_indication");
        assert!(!entries[0]["reason"].as_str().unwrap().contains("(H1)"));
        for entry in &entries[1..] {
            assert_eq!(entry["sent"], "unknown", "{entry}");
            assert!(
                entry["reason"].as_str().unwrap().contains("(H1)"),
                "{entry}"
            );
        }
        let unattached: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "unattached")
            .map(|line| line["identifier"].clone())
            .collect();
        assert_eq!(unattached, [0x7E1, 0x7E0]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exchanges_take_the_next_seq_and_their_target_s_link_state() {
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        let answer = exchange::End::Positive(vec![0x62, 0xF1, 0x90]);
        assert_eq!(
            log.exchange(evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                answer.clone()
            )),
            Some(2)
        );
        assert_eq!(
            log.exchange(evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                answer.clone()
            )),
            Some(3)
        );
        // Another target has a state of its own.
        assert_eq!(
            log.exchange(evidence(endpoint(0x7E1), &[0x22, 0xF1, 0x90], answer)),
            Some(4)
        );
        let lines = sink.lines();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[2]["event"], "exchange");
        assert_eq!(lines[2]["link_history"]["answered"], 0);
        assert_eq!(lines[3]["link_history"]["answered"], 1);
        assert_eq!(lines[3]["link_history"]["answered_same_match"], 1);
        assert_eq!(lines[4]["link_history"]["answered"], 0);
    }

    #[test]
    fn a_link_error_ends_the_matched_answers_of_every_target() {
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        let answer = exchange::End::Positive(vec![0x62, 0xF1, 0x90]);
        let read = [0x22, 0xF1, 0x90];
        log.exchange(evidence(endpoint(0x7E1), &read, answer.clone()));
        log.exchange(evidence(
            endpoint(0x7E0),
            &read,
            exchange::End::LinkError(exchange::LinkErrorKind::Frame),
        ));
        log.exchange(evidence(endpoint(0x7E1), &read, answer));
        let lines = sink.lines();
        // The other target's matched answer ended with the error, its count since it too.
        assert_eq!(lines[4]["link_history"]["answered"], 1);
        assert_eq!(lines[4]["link_history"]["answered_since_error"], 0);
        assert_eq!(lines[4]["link_history"]["answered_same_match"], 0);
    }

    #[test]
    fn a_failed_log_takes_no_exchange() {
        let sink = Recording {
            fail_write: Some(2),
            ..Recording::default()
        };
        let log = CommLog::over(Box::new(sink.clone()), b"");
        log.begin(&facts(), RunKind::First);
        assert!(log.is_open());
        let answer = exchange::End::Positive(vec![0x62, 0xF1, 0x90]);
        assert_eq!(
            log.exchange(evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                answer.clone()
            )),
            None
        );
        assert!(!log.is_open());
        assert_eq!(
            log.exchange(evidence(endpoint(0x7E0), &[0x22, 0xF1, 0x90], answer)),
            None
        );
        assert_eq!(taken_events(&sink), ["header", "job_start"]);
    }
}

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

/// The VCI's clocks: alignment, unwrapping and epochs (design 8.11 `clock_epoch`, ADR-361).
pub(crate) mod clocks;

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

/// The judgement of an exchange's times against the limit in force (design 8.11).
pub(crate) mod timing;

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
    /// The VCI's margins a hardware profile gave (`Job::with_vci_margins`).
    pub margins: VciMargins,
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
    LateResponse(LateResponseEntry<'a>),
    PrimitiveEnded(Box<PrimitiveEndedEntry>),
    ClockEpoch(clocks::ClockEpoch),
}

/// What the agent knows of the VCI it launched the job's worker for: the API its library
/// implements and the `unsigned long` width the worker runs with (design 7.1.2), for
/// `vci_connected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VciFacts {
    pub api: crate::launch::Api,
    pub long_size: u8,
}

/// A VCI's timing tolerances as a hardware profile gives them (design 8.11 "Timing margin",
/// "Drift bound", 13.5, ADR-357): each value the profile does not give is `None` and takes the
/// agent's default ([`VciMargins::DEFAULT_TIMING_MARGIN_US`] and the others). A job told none
/// ([`crate::Job::with_vci_margins`]) takes every default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VciMargins {
    /// A link's timing margin, in microseconds.
    pub timing_margin_us: Option<u32>,
    /// How far the VCI's clock may run from the agent's, in parts per million.
    pub drift_bound_ppm: Option<u32>,
    /// The tolerance of one reading of the VCI's clock, in microseconds.
    pub clock_margin_us: Option<u32>,
}

impl VciMargins {
    /// The agent's timing margin: a USB VCI's timestamps and their latency vary by up to about
    /// a millisecond, so two.
    pub const DEFAULT_TIMING_MARGIN_US: u32 = 2_000;
    /// The agent's drift bound: a common crystal is within 50 ppm, on each side.
    pub const DEFAULT_DRIFT_BOUND_PPM: u32 = 100;
    /// The agent's clock margin.
    pub const DEFAULT_CLOCK_MARGIN_US: u32 = 1_000;

    fn timing_margin(&self) -> (u32, &'static str) {
        sourced(self.timing_margin_us, Self::DEFAULT_TIMING_MARGIN_US)
    }

    fn drift_bound(&self) -> (u32, &'static str) {
        sourced(self.drift_bound_ppm, Self::DEFAULT_DRIFT_BOUND_PPM)
    }

    fn clock_margin(&self) -> (u32, &'static str) {
        sourced(self.clock_margin_us, Self::DEFAULT_CLOCK_MARGIN_US)
    }
}

/// A value and where it came from: `profile` when the profile gave it, else `default`.
fn sourced(given: Option<u32>, default: u32) -> (u32, &'static str) {
    match given {
        Some(value) => (value, "profile"),
        None => (default, "default"),
    }
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
    /// How far the VCI's clock may run from the agent's, and the tolerance of one clock reading,
    /// each with where it came from ([`VciMargins`]).
    drift_bound: DriftBound,
    clock_margin: ClockMargin,
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
struct DriftBound {
    ppm: u32,
    source: &'static str,
}

#[derive(Serialize)]
struct ClockMargin {
    us: u32,
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
    /// The clock the worker says `vci_us` is on (`EventItem.timestamp_clock`, ADR-353); `None`
    /// when it does not say.
    pub clock: Option<exchange::Clock>,
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
    /// Whether this is an unbound transmit indication (RxFlag `TX_INDICATION`) for
    /// `identifier`. One bound to an earlier primitive belongs to that primitive's exchange
    /// (design 8.11 `late_item`), so it never confirms another's.
    fn is_tx_indication_for(&self, identifier: u32) -> bool {
        !self.earlier_primitive
            && matches!(
                &self.data,
                UnattachedData::Result { rx_flag, identifier: Some(id), .. }
                    if rx_flag.get(3).is_some_and(|flags| flags & RX_FLAG_TX_INDICATION != 0)
                        && *id == identifier
            )
    }

    /// Whether this may be an unbound start-of-message indication for response identifier
    /// `identifier`: one for it, or one that names no identifier.
    fn may_be_start_indication_for(&self, identifier: u32) -> bool {
        !self.earlier_primitive
            && matches!(
                &self.data,
                UnattachedData::Result { rx_flag, identifier: id, .. }
                    if rx_flag.get(3).is_some_and(|flags| {
                        flags & RX_FLAG_START_OF_MESSAGE != 0
                            && flags & (RX_FLAG_TX_INDICATION | RX_FLAG_TX_MSG_TYPE) == 0
                    }) && id.is_none_or(|id| id == identifier)
            )
    }

    /// Whether this is an unbound start-of-message indication (RxFlag `START_OF_MESSAGE`, and
    /// neither a transmit indication nor a loopback, which [`UnattachedEntry`] tells apart the
    /// same way) for response identifier `identifier`. One bound to an earlier primitive belongs
    /// to that primitive's exchange (design 8.11 `late_item`).
    fn is_start_indication_for(&self, identifier: u32) -> bool {
        !self.earlier_primitive
            && matches!(
                &self.data,
                UnattachedData::Result { rx_flag, identifier: Some(id), .. }
                    if rx_flag.get(3).is_some_and(|flags| {
                        flags & RX_FLAG_START_OF_MESSAGE != 0
                            && flags & (RX_FLAG_TX_INDICATION | RX_FLAG_TX_MSG_TYPE) == 0
                    }) && *id == identifier
            )
    }
}

/// RxFlag byte 3's bits (ISO 22900-2:2022 D.2.2, Table D.5), which say what a result item is.
pub(crate) const RX_FLAG_TX_MSG_TYPE: u8 = 0x01;
pub(crate) const RX_FLAG_START_OF_MESSAGE: u8 = 0x02;
pub(crate) const RX_FLAG_BREAK: u8 = 0x04;
pub(crate) const RX_FLAG_TX_INDICATION: u8 = 0x08;

/// The longest response a single classic CAN frame can carry, under normal addressing (ISO
/// 15765-2:2024 9.6.2: one byte of protocol information): a longer response spans frames
/// whatever the addressing format, while a response this long or shorter may fit one frame and
/// so may have no start-of-message indication of its own. The log does not know the link's
/// addressing format, so it takes no start for such a response.
const SINGLE_FRAME_MAX: usize = 7;

/// The `unattached` event of an [`UnattachedItem`].
#[derive(Serialize)]
struct UnattachedEntry<'a> {
    link: u32,
    /// `transmit_indication`, `loopback`, `start_of_message`, `break`, `received` (a message
    /// another node sent: a response, a pending, or anything else), or `error_event`.
    item: &'static str,
    /// `none` or `earlier`.
    primitive: &'static str,
    /// The item's VCI time, raw.
    vci_us: u32,
    /// The clock `vci_us` is on (design 8.11 "On one clock"), as the worker gave it (ADR-353).
    /// Absent when the worker gives none: the agent does not infer it from the worker (design
    /// principle 6).
    #[serde(skip_serializing_if = "Option::is_none")]
    clock: Option<exchange::Clock>,
    /// The epoch of `vci_us` on its clock (ADR-361); absent when it names no clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u32>,
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

/// A `late_response` settling entry (design 8.11 "Settlement", ADR-365): the response, as an
/// `unattached` event would give it, the exchange it settles, and that exchange's new
/// classification.
#[derive(Serialize)]
struct LateResponseEntry<'a> {
    refers_to: Vec<u64>,
    #[serde(flatten)]
    response: UnattachedEntry<'a>,
    exchanges: Vec<Settled>,
    text: String,
    classification: exchange::Classification,
}

/// The end of a primitive the agent stopped waiting for (an exchange logged `vci_did_not_end`),
/// read at job end (design 8.11 `primitive_ended`, ADR-366).
#[derive(Debug, Clone)]
pub(crate) struct PrimitiveEnded {
    /// How it ended, as an exchange entry's `outcome` gives it; `None` when the agent cancelled
    /// it before it ended.
    pub end: Option<exchange::End>,
    pub detail: exchange::Detail,
    /// The pendings it passed on after its exchange's entry, in order.
    pub pending: Vec<exchange::Pending>,
    /// The VCI times of its end: the response's own, the receive timeout, the error.
    pub vci: exchange::VciTimes,
    /// The VCI ended it with a receive timeout.
    pub vci_ended: bool,
}

/// A `primitive_ended` settling entry (design 8.11 "Settlement", ADR-366): the primitive's end,
/// as an exchange entry gives its end, and the exchange's new classification. A cancellation by
/// the agent is recorded and settles nothing: the exchange keeps the classification it had.
#[derive(Serialize)]
struct PrimitiveEndedEntry {
    refers_to: Vec<u64>,
    link: u32,
    cancelled_by_agent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent: Option<exchange::Sent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent_evidence: Option<exchange::SentEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<exchange::Outcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    answers: Vec<exchange::AnswerItem>,
    /// The VCI times of the end, which `timing` was judged from.
    #[serde(skip_serializing_if = "Option::is_none")]
    vci: Option<exchange::VciTimes>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pending: Vec<exchange::Pending>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timing: Option<exchange::Timing>,
    detail: exchange::Detail,
    exchanges: Vec<Settled>,
    text: String,
    classification: exchange::Classification,
}

/// An exchange a settling entry settles, with what it carries so it can be read without the
/// earlier entry.
#[derive(Serialize)]
struct Settled {
    seq: u64,
    link: u32,
    origin: exchange::Origin,
    request: exchange::Request,
    classification: exchange::Classification,
    reason: String,
}

/// Whether a result item is a message another node sent, not an indication, a copy of a sent
/// message or a break (RxFlag byte 3).
fn is_received(rx_flag: &[u8]) -> bool {
    rx_flag.get(3).is_none_or(|flags| {
        flags
            & (RX_FLAG_TX_MSG_TYPE
                | RX_FLAG_START_OF_MESSAGE
                | RX_FLAG_BREAK
                | RX_FLAG_TX_INDICATION)
            == 0
    })
}

/// When the host read an item, on the agent's clock.
#[derive(Serialize)]
struct ReadAt {
    epoch: u32,
    read_us: u64,
}

impl<'a> UnattachedEntry<'a> {
    fn of(item: &'a UnattachedItem, placed: Option<clocks::Placed>) -> Self {
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
            clock: item.clock,
            epoch: placed.map(|placed| placed.epoch),
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
    /// Whether the worker passes on every response pending the link sees, as the worker's
    /// answer to `CreateComLogicalLink` says (ADR-355); `null` when the worker does not say.
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
    us: u32,
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

/// A VCI time the host read during an exchange, by where the evidence holds it.
enum HostRead {
    Unbound(usize),
    Pending(usize),
    Rejected(usize),
    /// The final response's item.
    End,
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
    /// The run's VCI margins, from its facts, for `vci_connected` and `link_opened`.
    margins: VciMargins,
    /// The VCI's clocks as the run's entries placed their times (ADR-361).
    clocks: clocks::VciClocks,
    /// When the exchange whose times are being placed began, on the agent's clock.
    exchange_began_us: Option<u64>,
    /// The limits and the margin of each link `link_opened` recorded, by link, which an
    /// exchange's timing is judged against.
    limits: Vec<(u32, timing::Limits)>,
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
            margins: VciMargins::default(),
            links: Vec::new(),
            clocks: clocks::VciClocks::starting_at(scan.next_clock_epoch),
            exchange_began_us: None,
            limits: Vec::new(),
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
        inner.margins = facts.margins;
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
    /// 22900-2:2022 8.2.6.3.2 and 8.4.9.2.1: tester present runs only on a link whose
    /// communication was started; `j2534-0404-service` arms it on `CoptStartcomm` alone).
    ///
    /// A start-of-message indication among them is the first frame of the exchange's response
    /// when it carries the exchange's response identifier and no owed request on the link could
    /// own it; no other exchange on the physical channel awaits a response on that identifier by
    /// the same construction. The exchange then has an answer of the ECU, and one that ended
    /// without a response ended with `response_incomplete`. Every other one is `unattached`.
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
        // The entry's VCI times (design 8.11 `times`): those the host read from the primitive's
        // own items, completed by the indications that attach, each on its item's clock.
        let mut vci = evidence.times.vci.take().unwrap_or_default();
        // Each VCI time placed on its clock (ADR-361), in the order the host read it (times
        // read together in their VCI order); the epochs they open are written first, before
        // the lines that use them.
        inner.exchange_began_us = Some(evidence.times.agent.before_us);
        let mut unbound_placed = vec![None; unbound.len()];
        let mut pending_placed = vec![None; evidence.pending.len()];
        let mut reads = Vec::new();
        for (index, item) in unbound.iter().enumerate() {
            if let Some(read) = item.read {
                reads.push((
                    read.monotonic_us,
                    item.clock,
                    Some(u64::from(item.vci_us)),
                    HostRead::Unbound(index),
                ));
            }
        }
        for (index, pending) in evidence.pending.iter().enumerate() {
            if let Some(raw) = pending.vci_us {
                reads.push((
                    pending.agent_us,
                    pending.clock,
                    Some(raw),
                    HostRead::Pending(index),
                ));
            }
        }
        for (index, rejected) in evidence.rejected.iter().enumerate() {
            if let Some(raw) = rejected.vci_us {
                reads.push((
                    rejected.agent_us,
                    rejected.clock,
                    Some(raw),
                    HostRead::Rejected(index),
                ));
            }
        }
        // The final response's item, when the host read it; without that time, as read when
        // the primitive ended, after everything else.
        let end_us = evidence.times.agent.end_us;
        if let Some(end) = vci.rx_end {
            reads.push((
                vci.response_read_us.unwrap_or(end_us),
                end.clock,
                vci.response_read_us.map(|_| end.us),
                HostRead::End,
            ));
        }
        // Among times read together, the VCI order is taken on each clock nearest its latest
        // time, or nearest the clock's first time read here when it has none, so a wrap between
        // them keeps their order.
        let anchor = |clock| {
            inner.clocks.latest(clock).or_else(|| {
                reads
                    .iter()
                    .filter(|(_, of, _, _)| *of == Some(clock))
                    .min_by_key(|(read_us, _, _, _)| *read_us)
                    .and_then(|(_, _, raw, _)| raw.and_then(|raw| i64::try_from(raw).ok()))
            })
        };
        let anchors = [
            anchor(exchange::Clock::Device),
            anchor(exchange::Clock::Worker),
        ];
        let mut reads: Vec<_> = reads
            .into_iter()
            .map(|(read_us, clock, raw, read)| {
                let order = match (raw, clock) {
                    (None, _) => i64::MAX,
                    (Some(raw), Some(clock)) => match anchors[clock as usize] {
                        Some(anchor) => clocks::nearest(anchor, raw),
                        None => raw as i64,
                    },
                    (Some(raw), None) => raw as i64,
                };
                (read_us, order, read)
            })
            .collect();
        reads.sort_by_key(|(read_us, order, _)| (*read_us, *order));
        let agent_epoch = evidence.times.agent.epoch;
        let read_at = |read_us| {
            Some(AgentNow {
                epoch: agent_epoch,
                monotonic_us: read_us,
                wall_unix_ms: 0,
            })
        };
        for (read_us, _, read) in reads {
            match read {
                HostRead::Unbound(index) => {
                    let item = &unbound[index];
                    unbound_placed[index] =
                        inner.place_read(item.clock, u64::from(item.vci_us), item.read)?;
                }
                HostRead::Pending(index) => {
                    let pending = &evidence.pending[index];
                    let raw = pending.vci_us.unwrap_or_default();
                    let placed = inner.place_read(pending.clock, raw, read_at(read_us))?;
                    pending_placed[index] = placed;
                    evidence.pending[index].epoch = placed.map(|placed| placed.epoch);
                }
                HostRead::Rejected(index) => {
                    let rejected = &evidence.rejected[index];
                    let raw = rejected.vci_us.unwrap_or_default();
                    let placed = inner.place_read(rejected.clock, raw, read_at(read_us))?;
                    evidence.rejected[index].epoch = placed.map(|placed| placed.epoch);
                }
                HostRead::End => {
                    if let Some(end) = vci.rx_end {
                        vci.rx_end =
                            Some(end.at(inner.place_read(end.clock, end.us, read_at(read_us))?));
                    }
                }
            }
        }
        // The transmit done and start the response's item carries come at or before its end.
        // The reading at the agent's deadline was taken after everything else, as read when the
        // primitive ended; an error's time, and an item read at no known time, are placed
        // nearest their clock's latest time.
        let anchor = vci.rx_end.and_then(|end| end.placed());
        let end_read = read_at(end_us);
        for time in [&mut vci.tx_done, &mut vci.rx_start].into_iter().flatten() {
            let placed = match anchor {
                Some((clock, anchor)) if time.clock == Some(clock) => {
                    Some(clocks::before(anchor, time.us))
                }
                _ => inner.place_unread(time.clock, time.us, end_read)?,
            };
            *time = time.at(placed);
        }
        if let Some(mut deadline) = vci.deadline {
            deadline.time = deadline.time.at(inner.place_read(
                deadline.time.clock,
                deadline.time.us,
                end_read,
            )?);
            vci.deadline = Some(deadline);
        }
        if let Some(error) = vci.error {
            vci.error = Some(error.at(inner.place_unread(error.clock, error.us, end_read)?));
        }
        if let Some(timeout) = vci.rx_timeout {
            vci.rx_timeout =
                Some(timeout.at(inner.place_unread(timeout.clock, timeout.us, end_read)?));
        }
        for (index, item) in unbound.iter().enumerate() {
            if item.read.is_none() {
                unbound_placed[index] =
                    inner.place_unread(item.clock, u64::from(item.vci_us), end_read)?;
            }
        }
        inner.exchange_began_us = None;
        // The response's own transmit done, which the host read from its item.
        let response_tx_done = vci.tx_done.is_some();
        if let exchange::Target::Endpoint { tx_id, .. } = evidence.target
            && evidence.transmit_evidence.is_none()
            && !inner.links[at].1.may_own_tx_indication()
            && let Some(own) = unbound
                .iter()
                .position(|item| item.is_tx_indication_for(tx_id))
        {
            let indication = unbound.remove(own);
            let placed = unbound_placed.remove(own);
            let read = indication.read;
            vci.tx_done = Some(
                exchange::VciTime::new(u64::from(indication.vci_us), indication.clock).at(placed),
            )
            .into_iter()
            .chain(vci.tx_done)
            .reduce(exchange::VciTime::prefer);
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxIndication);
            // The agent's time of the transmit confirmation is when the host read it, on the
            // entry's agent epoch.
            evidence.times.agent.tx_confirmed_us = read
                .filter(|read| read.epoch == evidence.times.agent.epoch)
                .map(|read| read.monotonic_us);
        }
        // A transmit done the response's own item carries confirms the send (design 8.11
        // `sent_evidence` `tx_done_time`) when no indication did.
        if response_tx_done && evidence.transmit_evidence.is_none() {
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxDoneTime);
        }
        // A start-of-message indication carrying the exchange's response identifier is the
        // first frame of its response, which counts as an answer of the ECU (design 8.11 "Link
        // settings for the log", "Answer of the ECU"), unless an owed request on the link could
        // own it. Nothing a refusal ended can have an answer. The latest start can be the final
        // response's start only when it came after the last pending and the response spans
        // frames: a start comes only with a multi-frame response's first frame, so one before a
        // single-frame answer was a transfer the ECU aborted (design 8.11 "Link settings for the
        // log"). The entry has no agent-side field for it.
        if !matches!(
            evidence.end,
            exchange::End::NotSent(_)
                | exchange::End::VciLost {
                    refused_after_loss: true
                }
        ) && !inner.links[at].1.may_own_start_indication(evidence.rx_id)
            && unbound
                .iter()
                .any(|item| item.is_start_indication_for(evidence.rx_id))
        {
            // Every start with the identifier is the same response's: with no other owner,
            // the ECU aborted and began again.
            let rx_id = evidence.rx_id;
            let response_len = match &evidence.end {
                exchange::End::Positive(bytes)
                | exchange::End::PositiveUnusable(bytes)
                | exchange::End::Negative(bytes) => Some(bytes.len()),
                _ => None,
            };
            if let Some(at) = unbound
                .iter()
                .rposition(|item| item.is_start_indication_for(rx_id))
            {
                let start = &unbound[at];
                let time = exchange::VciTime::new(u64::from(start.vci_us), start.clock)
                    .at(unbound_placed[at]);
                // After the last pending, on one clock and epoch, unwrapped: with a pending
                // whose time, clock or place is unknown, or a start with none, the order is
                // unknown and no start is taken.
                let last_pending = evidence
                    .pending
                    .last()
                    .map(|pending| pending.clock.zip(pending_placed.last().copied().flatten()));
                let after_pending = match last_pending {
                    None => true,
                    Some(pending) => match (pending, time.placed()) {
                        (Some((clock, at)), Some((own, start))) => {
                            clock == own
                                && at.epoch == start.epoch
                                && start.unwrapped >= at.unwrapped
                        }
                        _ => false,
                    },
                };
                let spans_frames = response_len.is_none_or(|len| len > SINGLE_FRAME_MAX);
                if after_pending && spans_frames {
                    vci.rx_start = Some(time)
                        .into_iter()
                        .chain(vci.rx_start)
                        .reduce(exchange::VciTime::prefer);
                }
            }
            let mut keep = unbound
                .iter()
                .map(|item| !item.is_start_indication_for(rx_id))
                .collect::<Vec<_>>()
                .into_iter();
            unbound_placed.retain(|_| keep.next().unwrap_or(true));
            unbound.retain(|item| !item.is_start_indication_for(rx_id));
            evidence.response_started = true;
            // Only a wait the VCI ended is an incomplete response; one the agent's own
            // deadline ended may still be arriving (design 8.11 `vci_did_not_end`).
            if evidence.vci_ended && matches!(evidence.end, exchange::End::NoResponse) {
                evidence.end = exchange::End::ResponseIncomplete;
            }
        }
        // The transmit done starts the first pending's interval.
        let transmit = vci.tx_done.and_then(|time| time.placed());
        exchange::pending_intervals(&mut evidence.pending, &pending_placed, transmit);
        // The times judged against the limits of the link `link_opened` recorded (design 8.11
        // "Timing judged"); evidence on a link the log did not open keeps the timing it gives.
        let mut judged_from: Option<(Vec<timing::At>, timing::Limits)> = None;
        if let Some((_, limits)) = inner.limits.iter().find(|(of, _)| *of == evidence.link) {
            let pendings: Vec<_> = evidence
                .pending
                .iter()
                .zip(&pending_placed)
                .map(|(pending, placed)| pending.clock.zip(*placed))
                .collect();
            let rx_id = evidence.rx_id;
            let response_may_have_begun = evidence.response_started
                || !limits.start_indications
                || unbound
                    .iter()
                    .any(|item| item.is_start_indication_for(rx_id));
            evidence.timing = timing::judge(
                *limits,
                evidence.absorbing(),
                timing::Times {
                    transmit,
                    pendings: &pendings,
                    start: vci.rx_start.and_then(|time| time.placed()),
                    end: vci.rx_end.and_then(|time| time.placed()),
                    end_len: evidence.end.response().map(<[u8]>::len),
                    timeout: vci.rx_timeout.and_then(|time| time.placed()),
                    response_may_have_begun,
                },
            );
            // The VCI's own limits past by the reading at the agent's deadline, on a link
            // whose start-of-message indications are on and showed no response under way: none
            // bound to the primitive, none unbound that names no identifier or this exchange's,
            // and no result the worker matched and the agent rejected, which may have ended the
            // VCI's wait.
            let start_unbound = unbound
                .iter()
                .any(|item| item.may_be_start_indication_for(rx_id));
            if matches!(evidence.end, exchange::End::VciDidNotEnd)
                && !response_may_have_begun
                && !evidence.bound_start
                && !start_unbound
                && evidence.rejected.is_empty()
            {
                evidence.vci_limits_past_by_us = vci.deadline.and_then(|reading| {
                    timing::past_vci_limits(
                        *limits,
                        evidence.absorbing(),
                        transmit,
                        &pendings,
                        reading.time.placed(),
                        reading.round_trip_us,
                    )
                });
            }
            judged_from = Some((pendings, *limits));
        }
        evidence.times.vci = (!vci.is_empty()).then_some(vci);
        for (item, placed) in unbound.iter().zip(unbound_placed) {
            if let Some(owner) = late_response_owner(&inner.links[at].1, item, &evidence) {
                inner.late_response(at, owner, item, placed)?;
                continue;
            }
            let event = Event::Unattached(UnattachedEntry::of(item, placed));
            inner.append_with(|seq, line| {
                serde_json::to_writer(line, &Line { seq, event: &event })
            })?;
        }
        let history = inner.links[at].1.history_for(&evidence);
        let kept = exchange::Kept {
            evidence: evidence.clone(),
            origin: evidence.origin,
            request: exchange::Request::of(&evidence),
            classification: exchange::Classification::None,
            sent: exchange::Sent::Unknown,
            transmit,
            pendings: judged_from
                .as_ref()
                .map(|(pendings, _)| pendings.clone())
                .unwrap_or_default(),
            limits: judged_from.map(|(_, limits)| limits),
        };
        let entry = exchange::ExchangeEntry::build(inner.next_seq, evidence, history);
        let seq = inner.append_with(|_, line| serde_json::to_writer(line, &entry))?;
        inner.links[at].1.record(&entry);
        // Only an exchange the VCI ended can be settled by a late response (ADR-365), and one
        // the agent's deadline ended by its primitive's end (ADR-366).
        if kept.evidence.vci_ended || matches!(kept.evidence.end, exchange::End::VciDidNotEnd) {
            inner.links[at].1.keep(
                seq,
                exchange::Kept {
                    classification: entry.classification,
                    sent: entry.sent,
                    ..kept
                },
            );
        }
        if let Some(kind) = entry.link_error() {
            for (other, (_, state)) in inner.links.iter_mut().enumerate() {
                if other != at {
                    state.channel_link_error(seq, kind);
                }
            }
        }
        Some(seq)
    }

    /// Drops what the log kept of the exchange logged as `seq`, which nothing will settle: its
    /// primitive was cancelled before the link's next request.
    pub(crate) fn release_kept(&self, seq: u64) {
        let mut inner = self.lock();
        for (_, state) in &mut inner.links {
            state.release_kept(seq);
        }
    }

    /// Writes the `primitive_ended` settling entry of the exchange logged as `seq`, whose
    /// primitive the agent stopped waiting for, from how it ended at job end (design 8.11
    /// "Settlement", ADR-366). The entry's `seq`; `None` when the log keeps no such exchange,
    /// or once the log has failed.
    pub(crate) fn primitive_ended(&self, seq: u64, ended: PrimitiveEnded) -> Option<u64> {
        let mut inner = self.lock();
        let now = {
            let clock = inner.clock();
            Some(AgentNow {
                epoch: u32::try_from(clock.epoch).unwrap_or(u32::MAX),
                monotonic_us: clock.monotonic_us,
                wall_unix_ms: clock.wall_unix_ms,
            })
        };
        let at = inner
            .links
            .iter()
            .position(|(_, state)| state.kept(seq).is_some())?;
        let kept = inner.links[at].1.kept(seq)?.clone();
        let Some(end) = ended.end else {
            let event = Event::PrimitiveEnded(Box::new(PrimitiveEndedEntry {
                refers_to: vec![seq],
                link: kept.evidence.link,
                cancelled_by_agent: true,
                sent: None,
                sent_evidence: None,
                outcome: None,
                answers: Vec::new(),
                vci: None,
                pending: Vec::new(),
                timing: None,
                detail: ended.detail,
                exchanges: vec![Settled {
                    seq,
                    link: kept.evidence.link,
                    origin: kept.origin,
                    request: kept.request.clone(),
                    classification: kept.classification,
                    reason: "The agent cancelled the primitive at job end before it ended; this settles nothing, and the exchange keeps its classification.".to_owned(),
                }],
                text: format!(
                    "The agent cancelled the primitive of the exchange logged as {seq} at job end."
                ),
                classification: kept.classification,
            }));
            return inner.append_with(|seq, line| {
                serde_json::to_writer(line, &Line { seq, event: &event })
            });
        };
        let observed = kept.evidence.vci_limits_past_by_us.is_some();
        let mut evidence = kept.evidence.clone();
        evidence.end = end;
        evidence.detail = ended.detail;
        evidence.vci_ended = ended.vci_ended;
        evidence.vci_limits_past_by_us = None;
        // A response that began and did not complete, as an exchange's end gives it.
        if evidence.vci_ended
            && evidence.response_started
            && matches!(evidence.end, exchange::End::NoResponse)
        {
            evidence.end = exchange::End::ResponseIncomplete;
        }
        // The end's times, read at job end, are placed nearest their clock's latest time.
        let mut vci = ended.vci;
        for time in [&mut vci.rx_end, &mut vci.rx_timeout, &mut vci.error]
            .into_iter()
            .flatten()
        {
            *time = time.at(inner.place_unread(time.clock, time.us, now)?);
        }
        // The transmit done and start the response's item carries come at or before its end.
        let anchor = vci.rx_end.and_then(|end| end.placed());
        for time in [&mut vci.tx_done, &mut vci.rx_start].into_iter().flatten() {
            let placed = match anchor {
                Some((clock, anchor)) if time.clock == Some(clock) => {
                    Some(clocks::before(anchor, time.us))
                }
                _ => inner.place_unread(time.clock, time.us, now)?,
            };
            *time = time.at(placed);
        }
        let mut pendings = kept.pendings.clone();
        let mut placed_all: Vec<Option<clocks::Placed>> = kept
            .pendings
            .iter()
            .map(|at| at.map(|(_, placed)| placed))
            .collect();
        for mut pending in ended.pending {
            let placed =
                inner.place_unread(pending.clock, pending.vci_us.unwrap_or_default(), now)?;
            pending.epoch = placed.map(|placed| placed.epoch);
            pendings.push(pending.clock.zip(placed));
            placed_all.push(placed);
            evidence.pending.push(pending);
        }
        let transmit = kept
            .transmit
            .or_else(|| vci.tx_done.and_then(|time| time.placed()));
        exchange::pending_intervals(&mut evidence.pending, &placed_all, transmit);
        if let Some(limits) = kept.limits {
            let response_may_have_begun = evidence.response_started || !limits.start_indications;
            evidence.timing = timing::judge(
                limits,
                evidence.absorbing(),
                timing::Times {
                    transmit,
                    pendings: &pendings,
                    start: vci.rx_start.and_then(|time| time.placed()),
                    end: vci.rx_end.and_then(|time| time.placed()),
                    end_len: evidence.end.response().map(<[u8]>::len),
                    timeout: vci.rx_timeout.and_then(|time| time.placed()),
                    response_may_have_begun,
                },
            );
        }
        let final_response = evidence.end.response().is_some();
        let state = &mut inner.links[at].1;
        if final_response {
            state.settle_owed(seq);
        }
        let history = state.history_settling(&evidence, seq);
        let link = evidence.link;
        evidence.times.vci = (!vci.is_empty()).then_some(vci);
        let times = evidence.times.vci;
        let entry = exchange::ExchangeEntry::build(seq, evidence, history);
        let (classification, reason) = exchange::settle_primitive_ended(&entry, observed);
        let event = Event::PrimitiveEnded(Box::new(PrimitiveEndedEntry {
            refers_to: vec![seq],
            link,
            cancelled_by_agent: false,
            sent: Some(entry.sent),
            sent_evidence: Some(entry.sent_evidence),
            text: format!(
                "The primitive of the exchange logged as {seq} ended after the agent had stopped waiting for it. {}",
                entry.text
            ),
            outcome: Some(entry.outcome),
            answers: entry.answers,
            vci: times,
            pending: entry.pending,
            timing: Some(entry.timing),
            detail: entry.detail,
            exchanges: vec![Settled {
                seq,
                link,
                origin: kept.origin,
                request: kept.request,
                classification,
                reason,
            }],
            classification,
        }));
        inner.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
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
        forwards_pendings: Option<bool>,
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
            response_times_judged: indications.transmit,
            forwards_pendings,
            timing_margin: {
                let (us, source) = self.lock().margins.timing_margin();
                TimingMargin { us, source }
            },
        });
        let mut inner = self.lock();
        let limits = timing::Limits {
            p2_us: u64::from(config.p2_max_ms) * 1000,
            p2_star_us: u64::from(config.p2_star_ms) * 1000,
            completion_us: (config.rc78_completion_ms != 0)
                .then(|| u64::from(config.rc78_completion_ms) * 1000),
            margin_us: u64::from(inner.margins.timing_margin().0),
            start_indications: indications.start_of_message,
            single_frame: config.single_frame(),
        };
        inner.limits.retain(|(of, _)| *of != link);
        inner.limits.push((link, limits));
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
        let (vci, margins) = {
            let inner = self.lock();
            (inner.vci, inner.margins)
        };
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
            drift_bound: {
                let (ppm, source) = margins.drift_bound();
                DriftBound { ppm, source }
            },
            clock_margin: {
                let (us, source) = margins.clock_margin();
                ClockMargin { us, source }
            },
        });
        let mut inner = self.lock();
        // A time base that carried on over the connection is not shown yet: each clock's next
        // time opens a new epoch.
        inner.clocks.reconnect();
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
        let clock = inner.clock();
        let now = Some(AgentNow {
            epoch: u32::try_from(clock.epoch).unwrap_or(u32::MAX),
            monotonic_us: clock.monotonic_us,
            wall_unix_ms: clock.wall_unix_ms,
        });
        let placed = match item.read {
            Some(_) => inner.place_read(item.clock, u64::from(item.vci_us), item.read)?,
            None => inner.place_unread(item.clock, u64::from(item.vci_us), now)?,
        };
        let event = Event::Unattached(UnattachedEntry::of(item, placed));
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

/// The owed exchange on `state`'s link that `item`, read during the exchange of `evidence`, is a
/// late response of (design 8.11 `late_response`): a message another node sent, not a pending,
/// that exactly one owed exchange the VCI ended could own, and that the exchange in flight does
/// not also await.
fn late_response_owner(
    state: &exchange::LinkState,
    item: &UnattachedItem,
    evidence: &exchange::Evidence,
) -> Option<u64> {
    let UnattachedData::Result {
        rx_flag,
        identifier,
        bytes,
    } = &item.data
    else {
        return None;
    };
    if bytes.is_empty() || !is_received(rx_flag) {
        return None;
    }
    let in_flight =
        identifier.is_none_or(|id| id == evidence.rx_id) && evidence.expected.matches(bytes);
    let [owner] = state.late_response_owners(*identifier, bytes)[..] else {
        return None;
    };
    let kept = state.kept(owner)?;
    // A pending is a late item, not a late response.
    let negative = &kept.evidence.expected.negative_pattern;
    let pending = !negative.is_empty()
        && bytes.starts_with(negative)
        && bytes.get(negative.len()) == Some(&exchange::NRC_PENDING);
    (!in_flight && !pending && kept.evidence.vci_ended).then_some(owner)
}

impl Inner {
    /// Writes the `late_response` settling entry of `item`, a response of the owed exchange
    /// `owner` on link `at`, which it settles (design 8.11 "Settlement", ADR-365).
    fn late_response(
        &mut self,
        at: usize,
        owner: u64,
        item: &UnattachedItem,
        placed: Option<clocks::Placed>,
    ) -> Option<()> {
        let UnattachedData::Result { bytes, .. } = &item.data else {
            return Some(());
        };
        let state = &mut self.links[at].1;
        let Some(kept) = state.settle_owed(owner) else {
            return Some(());
        };
        let answered = kept.answered(bytes);
        let history = state.history_for(&answered);
        let time = item.clock.zip(placed);
        let window = kept.limits.map_or(timing::LateWindow::Open, |limits| {
            timing::judge_late(
                limits,
                kept.evidence.absorbing(),
                kept.transmit,
                &kept.pendings,
                time,
                bytes.len(),
            )
        });
        let (classification, reason) =
            exchange::settle_late_response(&kept, &answered, window, &history);
        let event = Event::LateResponse(LateResponseEntry {
            refers_to: vec![owner],
            response: UnattachedEntry::of(item, placed),
            exchanges: vec![Settled {
                seq: owner,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request.clone(),
                classification,
                reason,
            }],
            text: format!(
                "The ECU's response to the exchange logged as {owner} was delivered after that exchange had ended."
            ),
            classification,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| ())
    }

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

    /// The tolerance the VCI's clocks are followed with, as `vci_connected` records it.
    fn tolerance(&self) -> clocks::Tolerance {
        clocks::Tolerance {
            drift_bound_ppm: self.margins.drift_bound().0,
            clock_margin_us: self.margins.clock_margin().0,
            began_us: self.exchange_began_us,
        }
    }

    /// Places `raw` on `clock` as the host read it at `read`, or nearest the clock's latest
    /// time when no read time is known, and writes the `clock_epoch` it opened. `None` once the
    /// log has failed; `Some(None)` for a time with no clock, or none to place it by.
    fn place_read(
        &mut self,
        clock: Option<exchange::Clock>,
        raw: u64,
        read: Option<AgentNow>,
    ) -> Option<Option<clocks::Placed>> {
        let Some(clock) = clock else {
            return Some(None);
        };
        let Some(read) = read else {
            return Some(self.clocks.near_latest(clock, raw));
        };
        let tolerance = self.tolerance();
        let (placed, opened) =
            self.clocks
                .read(clock, raw, read.monotonic_us, read.epoch, tolerance);
        if let Some(opened) = opened {
            self.append_with(|seq, line| {
                serde_json::to_writer(
                    line,
                    &Line {
                        seq,
                        event: &Event::ClockEpoch(opened),
                    },
                )
            })?;
        }
        Some(Some(placed))
    }

    /// Places `raw`, a time on `clock` the host read at no known time, nearest the clock's
    /// latest time; on a clock with no epoch yet, as read at `fallback`, a time after it was
    /// read.
    fn place_unread(
        &mut self,
        clock: Option<exchange::Clock>,
        raw: u64,
        fallback: Option<AgentNow>,
    ) -> Option<Option<clocks::Placed>> {
        match clock.and_then(|clock| self.clocks.near_latest(clock, raw)) {
            Some(placed) => Some(Some(placed)),
            None => self.place_read(clock, raw, fallback),
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
    /// The VCI clock epoch each clock opens next (`device`, then `worker`): one more than the
    /// largest `clock_epoch` read for it (ADR-361).
    next_clock_epoch: [u32; 2],
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
                if event == Some("clock_epoch") {
                    let clock = match object.and_then(|object| object.get("clock")?.as_str()) {
                        Some("device") => Some(0),
                        Some("worker") => Some(1),
                        _ => None,
                    };
                    let epoch = object.and_then(|object| object.get("epoch")?.as_u64());
                    if let Some((clock, epoch)) = clock.zip(epoch) {
                        // An epoch with no successor, as for the agent's.
                        let next = u32::try_from(epoch).ok()?.checked_add(1)?;
                        let slot = &mut scan.next_clock_epoch[clock];
                        *slot = (*slot).max(next);
                    }
                }
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
            margins: VciMargins::default(),
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
    /// hidden). Its VCI time names the clock the worker gave the item and none when it gave none,
    /// whatever API the job was told (design 8.11 "On one clock", design principle 6, ADR-353).
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
            // The worker's clock where a J2534 library's would be the device's: the log takes
            // it from the item.
            clock: Some(exchange::Clock::Worker),
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
                clock: None,
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
                    "item": "transmit_indication", "primitive": "none", "vci_us": 42, "clock": "worker", "epoch": 0,
                    "agent": agent, "rx_flag": "00000009", "identifier": 0x7E0,
                    "message": {"clear": ""}}),
                serde_json::json!({"event": "unattached", "link": 1, "item": "loopback",
                    "primitive": "none", "vci_us": 42, "clock": "worker", "epoch": 0, "agent": agent,
                    "rx_flag": "00000001",
                    "message": {"clear": "2702", "data": {"kind": "secret", "len": 2}}}),
                serde_json::json!({"event": "unattached", "link": 1,
                    "item": "start_of_message", "primitive": "none", "vci_us": 42, "clock": "worker", "epoch": 0,
                    "agent": agent, "rx_flag": "00000002",
                    "message": {"clear": ""}}),
                serde_json::json!({"event": "unattached", "link": 1, "item": "received",
                    "primitive": "earlier", "vci_us": 42, "clock": "worker", "epoch": 0, "agent": agent,
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
                None,
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

    /// `forwards_pendings` is what the worker said when the link was created (ADR-355), and
    /// `null` when it did not say.
    #[test]
    fn forwards_pendings_records_what_the_worker_said() {
        for (said, expected) in [
            (Some(true), serde_json::json!(true)),
            (Some(false), serde_json::json!(false)),
            (None, serde_json::Value::Null),
        ] {
            let dir = dir();
            let setup = setup(&dir);
            let log = CommLog::open(&setup).unwrap();
            log.begin(&facts(), RunKind::First);
            log.link_opened(
                1,
                &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
                &[],
                crate::link::Indications::default(),
                said,
            )
            .unwrap();
            log.end("ok");
            let lines = lines(&setup);
            let opened = lines
                .iter()
                .find(|line| line["event"] == "link_opened")
                .unwrap();
            assert_eq!(opened["forwards_pendings"], expected, "{said:?}");
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
            clock: None,
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
            clock: Some(exchange::Clock::Device),
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
        let read_at = |read_us| AgentNow {
            epoch: 0,
            monotonic_us: read_us,
            wall_unix_ms: 0,
        };
        // Another identifier's indication, and one bound to an earlier primitive, stay
        // unattached; this exchange's own confirms it, at the time the host read it.
        let earlier = UnattachedItem {
            earlier_primitive: true,
            ..tx_done(0x7E0)
        };
        let own = UnattachedItem {
            read: Some(read_at(1_234)),
            ..tx_done(0x7E0)
        };
        log.exchange_with(silent(), vec![tx_done(0x7E1), earlier, own])
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
                // The first VCI time opens its clock's epoch (ADR-361).
                "clock_epoch",
                "unattached",
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
        assert_eq!(entries[0]["times"]["agent"]["tx_confirmed_us"], 1_234);
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
        assert_eq!(unattached, [0x7E1, 0x7E0, 0x7E0]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Evidence of a read of F190 that `end` ended, on a wait the VCI ended itself.
    fn vci_ended(end: exchange::End) -> exchange::Evidence {
        let mut evidence = evidence(endpoint(0x7E0), &[0x22, 0xF1, 0x90], end);
        evidence.vci_ended = true;
        evidence
    }

    /// Runs `exchanges` on one log, each with the unbound items read during it, and gives the
    /// exchange entries and the `unattached` events, in the order written.
    fn run_exchanges(
        exchanges: Vec<(exchange::Evidence, Vec<UnattachedItem>)>,
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        for (evidence, unbound) in exchanges {
            log.exchange_with(evidence, unbound).unwrap();
        }
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        std::fs::remove_dir_all(&dir).unwrap();
        let of = |event: &str| -> Vec<serde_json::Value> {
            lines
                .iter()
                .filter(|line| line["event"] == event)
                .cloned()
                .collect()
        };
        (of("exchange"), of("unattached"))
    }

    /// Design 8.11 `times`: each VCI time with its own clock and epoch. The times the host read
    /// from the primitive's own items stand, unless only an indication's time names a clock; an
    /// attached transmit indication gives the transmit done, and the latest attached start of
    /// message the response's start when it came after the last pending and the response spans
    /// frames; a time no item gives is left out. A transmit done the response carries confirms
    /// the send when no indication did (`sent_evidence` `tx_done_time`), and starts the first
    /// pending's interval.
    #[test]
    fn each_vci_time_comes_from_its_item_with_its_clock() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        // A response that spans frames, and one that fits one frame.
        let long = [0x62, 0xF1, 0x90, 1, 2, 3, 4, 5];
        let short = [0x62, 0xF1, 0x90, 1, 2, 3, 4];
        let answered = |response: &[u8], times: exchange::VciTimes| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::Positive(response.to_vec()),
            );
            evidence.times.vci = Some(times);
            evidence
        };
        let start_at = |us, clock| UnattachedItem {
            vci_us: us,
            clock: Some(clock),
            ..start_of_message(0x7E8)
        };
        let ended = |us| exchange::VciTimes {
            rx_end: Some(device(us)),
            ..exchange::VciTimes::default()
        };
        // 0: the indications give the transmit and the latest start.
        log.exchange_with(
            answered(&long, ended(50)),
            vec![
                tx_done(0x7E0),
                start_at(20, exchange::Clock::Device),
                start_at(30, exchange::Clock::Worker),
            ],
        )
        .unwrap();
        // 1: the response's own transmit done stands beside an attached indication.
        log.exchange_with(
            answered(
                &long,
                exchange::VciTimes {
                    tx_done: Some(exchange::VciTime::new(3, Some(exchange::Clock::Worker))),
                    ..exchange::VciTimes::default()
                },
            ),
            vec![tx_done(0x7E0)],
        )
        .unwrap();
        // 2: one with no clock yields to the indication's.
        log.exchange_with(
            answered(
                &long,
                exchange::VciTimes {
                    tx_done: Some(exchange::VciTime::new(3, None)),
                    ..exchange::VciTimes::default()
                },
            ),
            vec![tx_done(0x7E0)],
        )
        .unwrap();
        // 3: a start before a single-frame answer was an aborted transfer, not its start.
        log.exchange_with(
            answered(&short, ended(50)),
            vec![tx_done(0x7E0), start_at(20, exchange::Clock::Device)],
        )
        .unwrap();
        // 4: so is a start before the last pending.
        let mut after_pending = answered(&long, ended(90));
        after_pending.pending = vec![exchange::Pending {
            vci_us: Some(40),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 0,
            since_previous_ms: None,
        }];
        log.exchange_with(
            after_pending,
            vec![tx_done(0x7E0), start_at(20, exchange::Clock::Device)],
        )
        .unwrap();
        // 5: with no indication, the response's transmit done confirms the send and starts the
        // first pending's interval.
        let mut carried = answered(
            &long,
            exchange::VciTimes {
                tx_done: Some(device(1_000)),
                ..ended(9_000)
            },
        );
        // The agent reads the pending and the end after the VCI's times, on its own clock.
        carried.pending = vec![exchange::Pending {
            vci_us: Some(3_500),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 5_000,
            since_previous_ms: None,
        }];
        carried.times.agent.end_us = 10_000;
        log.exchange_with(carried, Vec::new()).unwrap();
        // 6: a pending whose clock is unknown leaves the order unknown, so no start is taken.
        let mut unclocked_pending = answered(&long, ended(90));
        unclocked_pending.pending = vec![exchange::Pending {
            vci_us: Some(10),
            clock: None,
            epoch: None,
            agent_us: 0,
            since_previous_ms: None,
        }];
        log.exchange_with(
            unclocked_pending,
            vec![tx_done(0x7E0), start_at(20, exchange::Clock::Device)],
        )
        .unwrap();
        // 7: with no time at all, the entry has no VCI times.
        log.exchange_with(answered(&long, exchange::VciTimes::default()), Vec::new())
            .unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let exchanges: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        let time =
            |us: u64, clock: &str| serde_json::json!({ "us": us, "clock": clock, "epoch": 0 });
        assert_eq!(
            exchanges[0]["times"]["vci"],
            serde_json::json!({
                "tx_done": time(5, "device"),
                "rx_start": time(30, "worker"),
                "rx_end": time(50, "device"),
            })
        );
        assert_eq!(
            exchanges[1]["times"]["vci"],
            serde_json::json!({ "tx_done": time(3, "worker") })
        );
        assert_eq!(
            exchanges[2]["times"]["vci"],
            serde_json::json!({ "tx_done": time(5, "device") })
        );
        for dropped in [3, 4, 6] {
            let vci = &exchanges[dropped]["times"]["vci"];
            assert!(vci.get("rx_start").is_none(), "{}", exchanges[dropped]);
            // The start still shows the ECU answered.
            assert_eq!(exchanges[dropped]["link_history"]["this_exchange"], true);
        }
        assert_eq!(
            exchanges[5]["sent_evidence"], "tx_done_time",
            "{}",
            exchanges[5]
        );
        assert_eq!(exchanges[5]["sent"], "confirmed", "{}", exchanges[5]);
        assert_eq!(exchanges[5]["pending"][0]["since_previous_ms"], 2.5);
        assert!(
            exchanges[7]["times"].get("vci").is_none(),
            "{}",
            exchanges[7]
        );
    }

    /// Design 8.11 "Timing judged" and `clock_epoch` (ADR-361): the log unwraps each clock's
    /// times, so a pending interval across a wrap is given; a reset opens a new epoch, written
    /// before the entry that uses it; and a reopened log goes on with new epoch numbers.
    #[test]
    fn vci_times_are_unwrapped_and_a_reset_opens_a_new_epoch() {
        let dir = dir();
        let setup = setup(&dir);
        let span = 1_u64 << 32;
        // An exchange with a transmit done and an end the response's item carries, and one
        // pending, each VCI time with the agent time when the host read it.
        let exchange = |tx_done: u64, pending: (u64, u64), end: (u64, u64)| {
            let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::Positive(vec![0x62, 0xF1, 0x90, 1, 2, 3, 4, 5]),
            );
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(tx_done)),
                rx_end: Some(device(end.0)),
                ..exchange::VciTimes::default()
            });
            evidence.times.agent.end_us = end.1;
            evidence.pending = vec![exchange::Pending {
                vci_us: Some(pending.0),
                clock: Some(exchange::Clock::Device),
                epoch: None,
                agent_us: pending.1,
                since_previous_ms: None,
            }];
            evidence
        };
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        // 0: the transmit 2 ms before the 32-bit time wraps, the pending 3 ms after it.
        log.exchange(exchange(
            span - 2_000,
            (3_000, 1_005_500),
            (50_000, 1_052_500),
        ))
        .unwrap();
        // 1: the VCI restarted: its times start again near 0, long before a wrap.
        log.exchange(exchange(500, (1_000, 1_100_000), (4_000, 1_103_000)))
            .unwrap();
        log.end("ok");
        drop(log);
        // A later run of the job opens new epochs after the ones the log holds.
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::Resume);
        // Its times are on the agent epoch it starts.
        let mut later = exchange(100, (200, 300), (400, 500));
        later.times.agent.epoch = 1;
        log.exchange(later).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let epochs: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "clock_epoch")
            .collect();
        let exchanges: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        // Each epoch is written just before the first entry that uses it.
        for (epoch, exchange) in epochs.iter().zip(&exchanges) {
            assert_eq!(
                epoch["seq"].as_u64().unwrap() + 1,
                exchange["seq"].as_u64().unwrap()
            );
        }
        assert_eq!(epochs.len(), 3);
        assert_eq!(
            *epochs[0],
            serde_json::json!({"seq": epochs[0]["seq"], "event": "clock_epoch",
                "clock": "device", "epoch": 0, "reason": "first", "agent_epoch": 0,
                "first": {"us": 3_000, "read_us": 1_005_500}})
        );
        assert_eq!(
            *epochs[1],
            serde_json::json!({"seq": epochs[1]["seq"], "event": "clock_epoch",
                "clock": "device", "epoch": 1, "reason": "backward_step", "agent_epoch": 0,
                "first": {"us": 1_000, "read_us": 1_100_000},
                "previous": {"epoch": 0, "us": 50_000, "read_us": 1_052_500}})
        );
        assert_eq!(
            (
                &epochs[2]["epoch"],
                &epochs[2]["reason"],
                &epochs[2]["agent_epoch"]
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!("first"),
                &serde_json::json!(1)
            )
        );
        // The raw times stay in the entry, each with its epoch; the interval is taken across
        // the wrap.
        let time =
            |us: u64, epoch: u32| serde_json::json!({"us": us, "clock": "device", "epoch": epoch});
        assert_eq!(
            exchanges[0]["times"]["vci"],
            serde_json::json!({"tx_done": time(span - 2_000, 0), "rx_end": time(50_000, 0)})
        );
        assert_eq!(exchanges[0]["pending"][0]["epoch"], 0);
        assert_eq!(exchanges[0]["pending"][0]["since_previous_ms"], 5.0);
        assert_eq!(
            exchanges[1]["times"]["vci"],
            serde_json::json!({"tx_done": time(500, 1), "rx_end": time(4_000, 1)})
        );
        assert_eq!(exchanges[1]["pending"][0]["since_previous_ms"], 0.5);
        assert_eq!(exchanges[2]["times"]["vci"]["rx_end"]["epoch"], 2);
    }

    /// ADR-361 item 3: each VCI time is placed in the order the host read it, so none of these
    /// opens a second epoch: an error seen during a wait the agent's deadline ended, beside the
    /// clock reading taken after it, on a clock with no epoch yet; an item read after the final
    /// response's; a start after a pending across a wrap; and a rejected result. A reconnect
    /// then opens one.
    #[test]
    fn vci_times_take_their_order_from_how_the_host_read_them() {
        let span = 1_u64 << 32;
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| AgentNow {
            epoch: 0,
            monotonic_us,
            wall_unix_ms: 0,
        };
        let request = [0x22, 0xF1, 0x90];
        let long = vec![0x62, 0xF1, 0x90, 1, 2, 3, 4, 5];
        let epochs = |lines: &[serde_json::Value]| -> Vec<(String, u64, String)> {
            lines
                .iter()
                .filter(|line| line["event"] == "clock_epoch")
                .map(|line| {
                    (
                        line["clock"].as_str().unwrap().to_owned(),
                        line["epoch"].as_u64().unwrap(),
                        line["reason"].as_str().unwrap().to_owned(),
                    )
                })
                .collect()
        };
        let first = |clock: &str| (clock.to_owned(), 0, "first".to_owned());

        // An error 5 s before the reading at the deadline, on a fresh clock.
        let dir = self::dir();
        let setup = self::setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let mut waited = evidence(endpoint(0x7E0), &request, exchange::End::NoResponse);
        waited.times.agent.end_us = 5_002_000;
        waited.times.vci = Some(exchange::VciTimes {
            error: Some(device(1_000)),
            deadline: Some(exchange::ClockReading {
                time: device(5_001_000),
                round_trip_us: 500,
            }),
            ..exchange::VciTimes::default()
        });
        log.exchange(waited).unwrap();
        log.end("ok");
        let lines = self::lines(&setup);
        assert_eq!(epochs(&lines), [first("device")]);
        let entry = lines
            .iter()
            .find(|line| line["event"] == "exchange")
            .unwrap();
        assert_eq!(entry["times"]["vci"]["error"]["epoch"], 0, "{entry}");
        assert_eq!(entry["times"]["vci"]["deadline"]["epoch"], 0, "{entry}");

        // An indication for another identifier read after the final response's item, before
        // the primitive's end; then a start after a pending across the wrap, and a rejected
        // result between them.
        let dir = self::dir();
        let setup = self::setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let mut answered = evidence(
            endpoint(0x7E0),
            &request,
            exchange::End::Positive(long.clone()),
        );
        answered.times.agent.end_us = 1_016_000;
        answered.times.vci = Some(exchange::VciTimes {
            rx_end: Some(device(1_010_000)),
            response_read_us: Some(1_011_000),
            ..exchange::VciTimes::default()
        });
        let other = UnattachedItem {
            vci_us: 1_013_000,
            read: Some(read(1_014_000)),
            ..start_of_message(0x7E9)
        };
        log.exchange_with(answered, vec![other]).unwrap();
        let mut wrapped = evidence(endpoint(0x7E0), &request, exchange::End::Positive(long));
        wrapped.times.vci = Some(exchange::VciTimes {
            rx_end: Some(device(20_000)),
            ..exchange::VciTimes::default()
        });
        // The VCI's clock is the agent's less 1 000 us: the pending 5 ms before the wrap.
        let agent_of = |vci_us: u64| vci_us + 1_000;
        wrapped.times.agent.before_us = agent_of(span - 6_000);
        wrapped.times.agent.end_us = agent_of(span + 25_000);
        wrapped.times.vci.as_mut().unwrap().response_read_us = Some(agent_of(span + 20_000));
        wrapped.pending = vec![exchange::Pending {
            vci_us: Some(span - 5_000),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: agent_of(span - 5_000),
            since_previous_ms: None,
        }];
        wrapped.rejected = vec![exchange::Rejected {
            rx_id: 0x7E8,
            bytes: vec![0x50, 0x03],
            vci_us: Some(3_000),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: agent_of(span + 3_000),
            reason: exchange::RejectedReason::WrongEcho,
        }];
        let start = UnattachedItem {
            vci_us: 10_000,
            read: Some(read(agent_of(span + 10_000))),
            ..start_of_message(0x7E8)
        };
        log.exchange_with(wrapped, vec![start]).unwrap();
        // The VCI connects again: its next time opens an epoch.
        log.vci_connected(None).unwrap();
        let mut again = evidence(endpoint(0x7E0), &request, exchange::End::NoResponse);
        again.times.agent.before_us = agent_of(span + 35_000);
        again.times.agent.end_us = agent_of(span + 40_000);
        again.times.vci = Some(exchange::VciTimes {
            rx_end: Some(device(30_000)),
            ..exchange::VciTimes::default()
        });
        log.exchange(again).unwrap();
        log.end("ok");
        let lines = self::lines(&setup);
        assert_eq!(
            epochs(&lines),
            [
                first("device"),
                ("device".to_owned(), 1, "reconnect".to_owned())
            ],
            "{lines:?}"
        );
        let exchanges: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        let entry = exchanges[1];
        assert_eq!(
            entry["times"]["vci"]["rx_start"],
            serde_json::json!({"us": 10_000, "clock": "device", "epoch": 0}),
            "{entry}"
        );
        assert_eq!(entry["rejected"][0]["epoch"], 0, "{entry}");
        assert_eq!(entry["pending"][0]["epoch"], 0, "{entry}");
        assert_eq!(exchanges[2]["times"]["vci"]["rx_end"]["epoch"], 1);
    }

    /// ADR-361 item 3: times read together keep their VCI order across a wrap, so two pendings
    /// read in one notification on each side of it give their intervals and open no epoch.
    #[test]
    fn times_read_together_keep_their_order_across_a_wrap() {
        let span = 1_u64 << 32;
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let mut answered = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::Positive(vec![0x62, 0xF1, 0x90, 1]),
        );
        let pending = |vci_us| exchange::Pending {
            vci_us: Some(vci_us),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 50_000,
            since_previous_ms: None,
        };
        answered.pending = vec![pending(span - 5_000), pending(3_000)];
        answered.times.agent.end_us = 60_000;
        answered.times.vci = Some(exchange::VciTimes {
            tx_done: Some(device(span - 6_000)),
            rx_end: Some(device(4_000)),
            response_read_us: Some(51_000),
            ..exchange::VciTimes::default()
        });
        log.exchange(answered).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let epochs = lines
            .iter()
            .filter(|line| line["event"] == "clock_epoch")
            .count();
        assert_eq!(epochs, 1, "{lines:?}");
        let entry = lines
            .iter()
            .find(|line| line["event"] == "exchange")
            .unwrap();
        assert_eq!(entry["pending"][0]["since_previous_ms"], 1.0, "{entry}");
        assert_eq!(entry["pending"][1]["since_previous_ms"], 8.0, "{entry}");
        assert_eq!(entry["times"]["vci"]["rx_end"]["epoch"], 0, "{entry}");
    }

    /// Design 8.11 "Timing judged": once `link_opened` has recorded the link's limits, an
    /// exchange's window is judged from its VCI times on one clock and epoch, and the rows that
    /// need it classify: a receive timeout early is `system`, also where a response had begun.
    #[test]
    fn windows_are_judged_against_the_link_s_limits() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        // P2 1 s, P2* 5 s, completion 25 s; the agent's 2 ms margin.
        log.link_opened(
            1,
            &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
            &[],
            crate::link::Indications {
                transmit: true,
                start_of_message: true,
            },
            Some(true),
        )
        .unwrap();
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let silent = |timeout_us: u64, before_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::NoResponse,
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = before_us;
            evidence.times.agent.end_us = before_us + timeout_us + 2_000;
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(before_us + 500)),
                rx_timeout: Some(device(before_us + 500 + timeout_us)),
                error: Some(device(before_us + 500 + timeout_us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        // A receive timeout where the ECU had begun a response (first, while no earlier
        // request is owed that could own the start).
        let start = UnattachedItem {
            vci_us: 10_700,
            read: Some(AgentNow {
                epoch: 0,
                monotonic_us: 11_000,
                wall_unix_ms: 0,
            }),
            ..start_of_message(0x7E8)
        };
        log.exchange_with(silent(900_000, 10_000), vec![start])
            .unwrap();
        // At P2 within the margin, then 100 ms early.
        log.exchange(silent(1_001_000, 2_000_000)).unwrap();
        log.exchange(silent(900_000, 4_000_000)).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let exchanges: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        // The timeout after the response began is the transport's, not judged against P2: the
        // start is judged instead, within P2.
        let begun = exchanges[0];
        assert_eq!(begun["outcome"]["result"], "response_incomplete", "{begun}");
        assert_eq!(begun["timing"]["window"], "within_p2", "{begun}");
        assert_eq!(
            begun["timing"]["measured_by"], "vci_tx_done_to_rx_start",
            "{begun}"
        );
        assert_eq!(begun["classification"], "undetermined", "{begun}");
        let at_limit = exchanges[1];
        assert_eq!(
            at_limit["timing"]["window"], "timeout_at_limit",
            "{at_limit}"
        );
        assert_eq!(at_limit["timing"]["p2_ms"], 1_000);
        assert_eq!(at_limit["timing"]["margin_ms"], 2);
        assert!(at_limit["timing"].get("not_judged_reason").is_none());
        let early = exchanges[2];
        assert_eq!(early["timing"]["window"], "timeout_early", "{early}");
        assert_eq!(early["timing"]["window_by_ms"], 100.0, "{early}");
        assert_eq!(early["classification"], "system", "{early}");
        assert!(
            early["text"]
                .as_str()
                .unwrap()
                .contains("100.0 ms before the limit"),
            "{early}"
        );
    }

    /// Design 8.11 "Timing judged" and "Limit in force": on UDS on CAN at 500 kbit/s a frame
    /// takes at most 320 µs, so a single-frame response's end, or a forwarded pending's, past
    /// P2 plus the margin by more than that is late; a multi-frame response's end is not judged.
    #[test]
    fn single_frame_ends_and_forwarded_pendings_are_judged_late() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        // P2 1 s, P2* 5 s, completion 25 s; the agent's 2 ms margin.
        log.link_opened(
            1,
            &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
            &[],
            crate::link::Indications {
                transmit: true,
                start_of_message: true,
            },
            Some(true),
        )
        .unwrap();
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let answered = |response: Vec<u8>, end_us: u64, before_us: u64, pendings: &[u64]| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::Positive(response),
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = before_us;
            evidence.times.agent.end_us = before_us + end_us + 2_000;
            evidence.pending = pendings
                .iter()
                .map(|&us| exchange::Pending {
                    vci_us: Some(before_us + 500 + us),
                    clock: Some(exchange::Clock::Device),
                    agent_us: before_us + us + 1_000,
                    epoch: None,
                    since_previous_ms: None,
                })
                .collect();
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(before_us + 500)),
                rx_end: Some(device(before_us + 500 + end_us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        log.exchange(answered(
            vec![0x62, 0xF1, 0x90, 0x01],
            1_002_321,
            10_000,
            &[],
        ))
        .unwrap();
        log.exchange(answered(
            vec![0x62, 0xF1, 0x90, 0x01],
            3_000_000,
            2_000_000,
            &[1_002_321],
        ))
        .unwrap();
        log.exchange(answered(
            [0x62, 0xF1, 0x90].into_iter().chain([0x41; 17]).collect(),
            1_500_000,
            8_000_000,
            &[],
        ))
        .unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let exchanges: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        let late_end = exchanges[0];
        assert_eq!(late_end["timing"]["window"], "late", "{late_end}");
        assert_eq!(late_end["timing"]["window_by_ms"], 2.001, "{late_end}");
        assert!(late_end["timing"].get("late_pending").is_none());
        assert_eq!(late_end["classification"], "system", "{late_end}");
        let late_pending = exchanges[1];
        assert_eq!(late_pending["timing"]["window"], "late", "{late_pending}");
        assert_eq!(late_pending["timing"]["late_pending"], 0, "{late_pending}");
        assert_eq!(late_pending["classification"], "system", "{late_pending}");
        assert!(
            late_pending["reason"]
                .as_str()
                .unwrap()
                .contains("did not end its wait"),
            "{late_pending}"
        );
        let multi_frame = exchanges[2];
        assert_eq!(
            multi_frame["timing"]["not_judged_reason"], "end_after_limit",
            "{multi_frame}"
        );
    }

    /// Design 8.11 `vci_did_not_end`: an exchange the agent's deadline ended is `system` once the
    /// VCI's device clock, read at that deadline less its round trip, is past the VCI's own
    /// limits by more than a single frame's transfer with no response started; otherwise it stays
    /// `undetermined`.
    #[test]
    fn the_vci_s_own_limits_past_at_the_deadline_are_system() {
        let opened = |start_of_message| {
            let dir = dir();
            let setup = setup(&dir);
            let log = CommLog::open(&setup).unwrap();
            log.begin(&facts(), RunKind::First);
            // P2 1 s; the agent's 2 ms margin; one frame 320 µs.
            log.link_opened(
                1,
                &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
                &[],
                crate::link::Indications {
                    transmit: true,
                    start_of_message,
                },
                Some(true),
            )
            .unwrap();
            (dir, setup, log)
        };
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        // The transmit done comes from the unbound transmit indication the host read, as it
        // does for an exchange the agent's deadline ended.
        let transmit = UnattachedItem {
            vci_us: 10_500,
            read: read(10_600),
            ..tx_done(0x7E0)
        };
        let waited = |reading_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::VciDidNotEnd,
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.times.agent.before_us = 10_000;
            evidence.times.agent.end_us = 10_000 + reading_us + 2_000;
            evidence.times.vci = Some(exchange::VciTimes {
                deadline: Some(exchange::ClockReading {
                    time: device(10_500 + reading_us),
                    round_trip_us: 700,
                }),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let classified =
            |start_of_message, evidence: exchange::Evidence, more: Vec<UnattachedItem>| {
                let (_dir, setup, log) = opened(start_of_message);
                let mut items = vec![transmit.clone()];
                items.extend(more);
                log.exchange_with(evidence, items).unwrap();
                log.end("ok");
                let lines = lines(&setup);
                lines
                    .iter()
                    .find(|line| line["event"] == "exchange")
                    .unwrap()
                    .clone()
            };
        // Past by 1 µs: 1 s + 2 ms + 320 µs + 700 µs from the deadline to the read, plus 1.
        let past = classified(true, waited(1_003_021), Vec::new());
        assert_eq!(past["outcome"]["result"], "vci_did_not_end", "{past}");
        assert_eq!(past["sent"], "confirmed", "{past}");
        assert_eq!(past["classification"], "system", "{past}");
        // At the bound, or with no start-of-message indications on the link: undetermined.
        let at_bound = classified(true, waited(1_003_020), Vec::new());
        assert_eq!(at_bound["classification"], "undetermined", "{at_bound}");
        let no_indications = classified(false, waited(9_000_000), Vec::new());
        assert_eq!(
            no_indications["classification"], "undetermined",
            "{no_indications}"
        );
        // A response that may have started: an attached start, one that names no identifier,
        // one bound to the primitive, or a result the agent rejected.
        let start = UnattachedItem {
            vci_us: 900_000,
            read: read(900_500),
            ..start_of_message(0x7E8)
        };
        let started = classified(true, waited(9_000_000), vec![start.clone()]);
        assert_eq!(started["classification"], "undetermined", "{started}");
        let mut anonymous = start;
        if let UnattachedData::Result { identifier, .. } = &mut anonymous.data {
            *identifier = None;
        }
        let unnamed = classified(true, waited(9_000_000), vec![anonymous]);
        assert_eq!(unnamed["classification"], "undetermined", "{unnamed}");
        let mut bound = waited(9_000_000);
        bound.bound_start = true;
        let bound = classified(true, bound, Vec::new());
        assert_eq!(bound["classification"], "undetermined", "{bound}");
        let mut rejected = waited(9_000_000);
        rejected.rejected = vec![exchange::Rejected {
            rx_id: 0x7E8,
            bytes: vec![0x62, 0xF1, 0x91],
            vci_us: Some(500_000),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 500_100,
            reason: exchange::RejectedReason::WrongEcho,
        }];
        let rejected = classified(true, rejected, Vec::new());
        assert_eq!(rejected["classification"], "undetermined", "{rejected}");
    }

    /// Design 8.11 "Settlement", `late_response` (ADR-365): a response that comes after the VCI
    /// ended its exchange settles that exchange when it is the one owed request that could own
    /// it, judged by its time against that exchange's limit in force; otherwise it stays
    /// `unattached`.
    #[test]
    fn a_late_response_settles_the_one_exchange_that_could_own_it() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        // A request sent at `at_us` (VCI and agent time alike), its transmit indication read
        // 100 µs later, and the VCI's receive timeout at P2 (1 s) after it.
        let silent = |request: &[u8], at_us: u64| {
            let mut evidence = evidence(endpoint(0x7E0), request, exchange::End::NoResponse);
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 1_001_000;
            evidence.times.vci = Some(exchange::VciTimes {
                rx_timeout: Some(device(at_us + 1_000_000)),
                error: Some(device(at_us + 1_000_000)),
                ..exchange::VciTimes::default()
            });
            let transmit = UnattachedItem {
                vci_us: u32::try_from(at_us).unwrap(),
                read: read(at_us + 100),
                ..tx_done(0x7E0)
            };
            (evidence, transmit)
        };
        let response = |bytes: &[u8], vci_us: u64| UnattachedItem {
            link: 1,
            earlier_primitive: true,
            vci_us: u32::try_from(vci_us).unwrap(),
            clock: Some(exchange::Clock::Device),
            read: read(vci_us + 100),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes: bytes.to_vec(),
            },
        };
        // The exchanges run: (request, the late items read during it), each 3 s after the one
        // before, from 10 ms; the log's lines that are not the header's.
        // A request, with the late responses (bytes, VCI time) read during its exchange.
        type Exchange<'a> = (&'a [u8], Vec<(&'a [u8], u64)>);
        let run = |exchanges: Vec<Exchange<'_>>| {
            let dir = dir();
            let setup = setup(&dir);
            let log = CommLog::open(&setup).unwrap();
            log.begin(&facts(), RunKind::First);
            log.link_opened(
                1,
                &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
                &[],
                crate::link::Indications {
                    transmit: true,
                    start_of_message: true,
                },
                Some(true),
            )
            .unwrap();
            for (index, (request, late)) in exchanges.into_iter().enumerate() {
                let at_us = 10_000 + 3_000_000 * index as u64;
                let (evidence, transmit) = silent(request, at_us);
                let mut items = vec![transmit];
                items.extend(
                    late.into_iter()
                        .map(|(bytes, vci_us)| response(bytes, vci_us)),
                );
                log.exchange_with(evidence, items).unwrap();
            }
            log.end("ok");
            lines(&setup)
        };
        let of = |lines: &[serde_json::Value], event: &str| -> Vec<serde_json::Value> {
            lines
                .iter()
                .filter(|line| line["event"] == event)
                .cloned()
                .collect()
        };
        let read_f190: &[u8] = &[0x22, 0xF1, 0x90];
        let read_f191: &[u8] = &[0x22, 0xF1, 0x91];
        let answer: &[u8] = &[0x62, 0xF1, 0x90, 0x41];
        // Within P2 less the margin (998 ms after the transmit at 10 ms): `system`.
        let lines = run(vec![
            (read_f190, Vec::new()),
            (read_f191, vec![(answer, 10_000 + 998_000)]),
        ]);
        let settled = of(&lines, "late_response");
        assert_eq!(settled.len(), 1, "{lines:?}");
        let first_seq = of(&lines, "exchange")[0]["seq"].clone();
        assert_eq!(settled[0]["refers_to"], serde_json::json!([first_seq]));
        assert_eq!(settled[0]["classification"], "system", "{}", settled[0]);
        assert_eq!(settled[0]["exchanges"][0]["seq"], first_seq);
        assert_eq!(
            settled[0]["exchanges"][0]["request"]["sid"], "0x22",
            "{}",
            settled[0]
        );
        assert!(of(&lines, "unattached").is_empty(), "{lines:?}");
        // One frame past P2 plus the margin and its 320 µs: `vehicle`. Between: `undetermined`.
        for (vci_us, classification) in [
            (10_000 + 1_002_321, "vehicle"),
            (10_000 + 1_002_320, "undetermined"),
            (10_000 + 999_000, "undetermined"),
        ] {
            let lines = run(vec![
                (read_f190, Vec::new()),
                (read_f191, vec![(answer, vci_us)]),
            ]);
            let settled = of(&lines, "late_response");
            assert_eq!(
                settled[0]["classification"], classification,
                "{vci_us}: {}",
                settled[0]
            );
        }
        // Two owed requests that could own it, or the exchange in flight awaiting the same
        // response: `unattached`, settling nothing.
        for exchanges in [
            vec![
                (read_f190, Vec::new()),
                (read_f190, Vec::new()),
                (read_f191, vec![(answer, 6_500_000)]),
            ],
            vec![
                (read_f190, Vec::new()),
                (read_f190, vec![(answer, 2_500_000)]),
            ],
        ] {
            let lines = run(exchanges);
            assert!(of(&lines, "late_response").is_empty(), "{lines:?}");
            assert_eq!(of(&lines, "unattached").len(), 1, "{lines:?}");
        }
        // A late pending is no late response.
        let lines = run(vec![
            (read_f190, Vec::new()),
            (read_f191, vec![(&[0x7F, 0x22, 0x78], 500_000)]),
        ]);
        assert!(of(&lines, "late_response").is_empty(), "{lines:?}");
        // A settled exchange is owed no more: a second response settles nothing.
        let lines = run(vec![
            (read_f190, Vec::new()),
            (read_f191, vec![(answer, 500_000), (answer, 600_000)]),
        ]);
        assert_eq!(of(&lines, "late_response").len(), 1, "{lines:?}");
        assert_eq!(of(&lines, "unattached").len(), 1, "{lines:?}");
        // A response that names no identifier is matched against every owed request; the
        // exchange's own entry is followed by a settling entry with the response's fields.
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let (first, transmit) = silent(read_f190, 10_000);
        log.exchange_with(first, vec![transmit]).unwrap();
        let (second, transmit) = silent(read_f191, 3_010_000);
        let mut unnamed = response(answer, 500_000);
        if let UnattachedData::Result { identifier, .. } = &mut unnamed.data {
            *identifier = None;
        }
        log.exchange_with(second, vec![transmit, unnamed]).unwrap();
        // An exchange the agent's deadline ended is settled by `primitive_ended`, not here.
        let (mut third, transmit) = silent(read_f190, 6_010_000);
        third.vci_ended = false;
        third.end = exchange::End::VciDidNotEnd;
        log.exchange_with(third, vec![transmit]).unwrap();
        let (fourth, transmit) = silent(read_f191, 9_010_000);
        log.exchange_with(fourth, vec![transmit, response(answer, 6_500_000)])
            .unwrap();
        log.end("ok");
        let lines = self::lines(&setup);
        let settled = of(&lines, "late_response");
        assert_eq!(settled.len(), 1, "{lines:?}");
        assert_eq!(settled[0]["item"], "received", "{}", settled[0]);
        assert_eq!(settled[0]["primitive"], "earlier", "{}", settled[0]);
        assert!(settled[0].get("identifier").is_none(), "{}", settled[0]);
        assert!(settled[0]["message"].is_object(), "{}", settled[0]);
        assert_eq!(settled[0]["epoch"], 0, "{}", settled[0]);
        assert_eq!(of(&lines, "unattached").len(), 1, "{lines:?}");
    }

    /// Design 8.11 "Settlement", `primitive_ended` (ADR-366): the end of a primitive the agent
    /// stopped waiting for settles its exchange as if the agent had waited for it, except that
    /// an end within the limit in force is `system`; the agent's cancel settles nothing.
    #[test]
    fn a_primitive_that_ends_at_job_end_settles_its_exchange() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        // P2 1 s, the margin 2 ms; the request's transmit indication at 10.5 ms.
        let settled = |ended: PrimitiveEnded| {
            let dir = dir();
            let setup = setup(&dir);
            let log = CommLog::open(&setup).unwrap();
            log.begin(&facts(), RunKind::First);
            log.link_opened(
                1,
                &crate::link::LinkConfig::iso15765(0x7E0, 0x7E8),
                &[],
                crate::link::Indications {
                    transmit: true,
                    start_of_message: true,
                },
                Some(true),
            )
            .unwrap();
            let mut waited = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::VciDidNotEnd,
            );
            waited.pending_visibility = exchange::PendingVisibility::Forwarded;
            waited.times.agent.before_us = 10_000;
            waited.times.agent.end_us = 9_000_000;
            let transmit = UnattachedItem {
                vci_us: 10_500,
                read: Some(AgentNow {
                    epoch: 0,
                    monotonic_us: 10_600,
                    wall_unix_ms: 0,
                }),
                ..tx_done(0x7E0)
            };
            let seq = log.exchange_with(waited, vec![transmit]).unwrap();
            let written = log.primitive_ended(seq, ended);
            log.end("ok");
            let lines = lines(&setup);
            let entry = lines
                .iter()
                .find(|line| line["event"] == "primitive_ended")
                .cloned();
            assert_eq!(written.is_some(), entry.is_some());
            (seq, entry)
        };
        let response = |end_us: u64| PrimitiveEnded {
            end: Some(exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41])),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_end: Some(device(10_500 + end_us)),
                ..exchange::VciTimes::default()
            },
            vci_ended: false,
        };
        // A response within P2: the job gave up on an exchange that ended in time.
        let (seq, entry) = settled(response(500_000));
        let entry = entry.unwrap();
        assert_eq!(entry["refers_to"], serde_json::json!([seq]), "{entry}");
        assert_eq!(entry["cancelled_by_agent"], false, "{entry}");
        assert_eq!(entry["outcome"]["result"], "positive", "{entry}");
        assert_eq!(entry["timing"]["window"], "within_p2", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        assert!(
            entry["exchanges"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("gave up"),
            "{entry}"
        );
        assert_eq!(entry["exchanges"][0]["request"]["sid"], "0x22", "{entry}");
        // A single-frame response past P2 plus the margin and its transfer: the late row.
        let (_, entry) = settled(response(1_002_321));
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "late", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A receive timeout at P2: in time, so `system`.
        let timeout = PrimitiveEnded {
            end: Some(exchange::End::NoResponse),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_timeout: Some(device(10_500 + 1_000_000)),
                error: Some(device(10_500 + 1_000_000)),
                ..exchange::VciTimes::default()
            },
            vci_ended: true,
        };
        let (_, entry) = settled(timeout);
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "timeout_at_limit", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A lost VCI keeps its own row.
        let lost = PrimitiveEnded {
            end: Some(exchange::End::VciLost {
                refused_after_loss: false,
            }),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
        };
        let (_, entry) = settled(lost);
        assert_eq!(entry.unwrap()["classification"], "undetermined");
        // A response whose time cannot be judged: `system` too, since late it would be.
        let mut unclocked = response(500_000);
        unclocked.vci.rx_end = Some(exchange::VciTime::new(510_500, None));
        let (_, entry) = settled(unclocked);
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "not_judged", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A pending, then a link error: no owed request of its own.
        let pending_then_error = PrimitiveEnded {
            end: Some(exchange::End::LinkError(exchange::LinkErrorKind::Receive)),
            detail: exchange::Detail::default(),
            pending: vec![exchange::Pending {
                vci_us: Some(10_500 + 400_000),
                clock: Some(exchange::Clock::Device),
                epoch: None,
                agent_us: 0,
                since_previous_ms: None,
            }],
            vci: exchange::VciTimes {
                error: Some(device(10_500 + 900_000)),
                ..exchange::VciTimes::default()
            },
            vci_ended: false,
        };
        let (_, entry) = settled(pending_then_error);
        let entry = entry.unwrap();
        assert!(
            !entry["exchanges"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("earlier request"),
            "{entry}"
        );
        assert!(
            entry["pending"][0]["since_previous_ms"].is_number(),
            "{entry}"
        );
        // The agent's cancel settles nothing: the exchange keeps its classification.
        let cancelled = PrimitiveEnded {
            end: None,
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
        };
        let (_, entry) = settled(cancelled);
        let entry = entry.unwrap();
        assert_eq!(entry["cancelled_by_agent"], true, "{entry}");
        assert!(entry.get("outcome").is_none(), "{entry}");
        assert_eq!(entry["classification"], "undetermined", "{entry}");
    }

    /// No `primitive_ended` entry for an exchange the log keeps nothing of.
    #[test]
    fn a_primitive_end_of_no_kept_exchange_writes_nothing() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let ended = PrimitiveEnded {
            end: None,
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
        };
        assert_eq!(log.primitive_ended(42, ended), None);
        log.end("ok");
        assert!(
            !lines(&setup)
                .iter()
                .any(|line| line["event"] == "primitive_ended")
        );
    }

    /// A start-of-message indication for `identifier`, read during the exchange.
    fn start_of_message(identifier: u32) -> UnattachedItem {
        UnattachedItem {
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0x02],
                identifier: Some(identifier),
                bytes: Vec::new(),
            },
            ..tx_done(identifier)
        }
    }

    /// Design 8.11 "Link settings for the log": an unbound start-of-message indication is the
    /// first frame of the response of the exchange it was read in when it carries that
    /// exchange's response identifier and no owed request on the link could own it. The exchange
    /// then has an answer of the ECU (so its send is confirmed by `answer`), and one that ended
    /// without a response is `response_incomplete`; otherwise the indication is `unattached`.
    #[test]
    fn an_unbound_start_of_message_starts_its_exchange_s_response_unless_another_could_own_it() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let silent = || vci_ended(exchange::End::NoResponse);
        // Another identifier's indication, one bound to an earlier primitive and a transmit
        // indication stay unattached; the one for the response identifier attaches.
        let earlier = UnattachedItem {
            earlier_primitive: true,
            ..start_of_message(0x7E8)
        };
        log.exchange_with(
            silent(),
            vec![
                start_of_message(0x7E9),
                earlier,
                tx_done(0x7E8),
                start_of_message(0x7E8),
            ],
        )
        .unwrap();
        // The first exchange is owed now (sent on the bus, no final response): an indication
        // for the same response identifier could be its late one, so it is unattached and this
        // exchange's send stays unknown.
        log.exchange_with(silent(), vec![start_of_message(0x7E8)])
            .unwrap();
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let events = events(&lines);
        assert_eq!(
            &events[2..events.len() - 1],
            [
                // The first VCI time opens its clock's epoch (ADR-361).
                "clock_epoch",
                "unattached",
                "unattached",
                "unattached",
                "exchange",
                "unattached",
                "exchange"
            ]
        );
        let entries: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        assert_eq!(entries[0]["outcome"]["result"], "response_incomplete");
        assert_eq!(entries[0]["sent"], "confirmed");
        assert_eq!(entries[0]["sent_evidence"], "answer");
        assert_eq!(entries[0]["classification"], "undetermined");
        assert_eq!(entries[0]["link_history"]["this_exchange"], true);
        assert_eq!(entries[1]["outcome"]["result"], "no_response");
        assert_eq!(entries[1]["sent"], "unknown");
        assert_eq!(entries[1]["link_history"]["this_exchange"], false);
        let unattached: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "unattached")
            .map(|line| {
                (
                    line["item"].as_str().unwrap(),
                    line["identifier"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            unattached,
            [
                ("start_of_message", 0x7E9),
                ("start_of_message", 0x7E8),
                ("transmit_indication", 0x7E8),
                ("start_of_message", 0x7E8)
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A second start for the same response identifier in one exchange (the ECU aborted and began
    /// again) has no other owner: it is consumed with the first.
    #[test]
    fn every_start_of_message_for_the_response_identifier_in_one_exchange_attaches() {
        let (entries, unattached) = run_exchanges(vec![(
            vci_ended(exchange::End::NoResponse),
            vec![
                start_of_message(0x7E8),
                start_of_message(0x7E9),
                start_of_message(0x7E8),
            ],
        )]);
        assert_eq!(entries[0]["outcome"]["result"], "response_incomplete");
        assert_eq!(unattached.len(), 1);
        assert_eq!(unattached[0]["identifier"], 0x7E9);
    }

    /// Only an exchange the VCI ended converts a start into `response_incomplete`. The agent's
    /// own deadline (`vci_did_not_end`, `host::deadline_end`) keeps its outcome, since the
    /// response may still be arriving, and so does a primitive that ended with no response and
    /// no receive timeout; the start still is an answer there. The start is no bound answer, so
    /// H2 stays open (ADR-354); the pending rows are checked on a forwarding link in
    /// `a_start_alone_takes_neither_the_pending_rows_nor_the_rows_without_an_answer`.
    #[test]
    fn a_start_of_message_does_not_make_an_agent_deadline_a_response_incomplete() {
        let (entries, unattached) = run_exchanges(vec![(
            evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::NoResponse,
            ),
            vec![start_of_message(0x7E8)],
        )]);
        assert!(unattached.is_empty());
        assert_eq!(entries[0]["outcome"]["result"], "no_response");
        assert_eq!(entries[0]["sent"], "confirmed");
        assert_eq!(entries[0]["sent_evidence"], "answer");
        assert_eq!(entries[0]["classification"], "undetermined");
        let reason = entries[0]["reason"].as_str().unwrap();
        assert!(
            !reason.contains("answered with a pending and then"),
            "{reason}"
        );
        assert!(reason.contains("(H2)"), "{reason}");

        let (entries, unattached) = run_exchanges(vec![(
            evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::VciDidNotEnd,
            ),
            vec![start_of_message(0x7E8)],
        )]);
        assert!(unattached.is_empty());
        assert_eq!(entries[0]["outcome"]["result"], "vci_did_not_end");
        assert_eq!(entries[0]["sent"], "confirmed");
        assert_eq!(entries[0]["sent_evidence"], "answer");
        assert_eq!(entries[0]["classification"], "undetermined");
    }

    /// The rows of a pending seen need a pending (ADR-354): on a link that forwards pendings,
    /// with a judged window, a start alone ended with no response and no receive timeout is no
    /// pending answer.
    /// A request whose positive response is suppressed still has an answer of the ECU in the
    /// start, so it does not take the rows for no answer of the ECU.
    #[test]
    fn a_start_alone_takes_neither_the_pending_rows_nor_the_rows_without_an_answer() {
        let mut forwarded = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::NoResponse,
        );
        forwarded.pending_visibility = exchange::PendingVisibility::Forwarded;
        forwarded.timing.window = exchange::Window::WithinP2;
        let mut suppressed = evidence(
            endpoint(0x7E0),
            &[0x31, 0x81, 0xFF, 0x00],
            exchange::End::NoResponse,
        );
        suppressed.suppressed = true;
        for (case, evidence) in [("forwarded", forwarded), ("suppressed", suppressed)] {
            let (entries, _) = run_exchanges(vec![(evidence, vec![start_of_message(0x7E8)])]);
            let entry = &entries[0];
            assert_eq!(entry["sent_evidence"], "answer", "{case}");
            assert_eq!(entry["link_history"]["this_exchange"], true, "{case}");
            assert_ne!(entry["classification"], "vehicle", "{case}");
            let reason = entry["reason"].as_str().unwrap();
            assert!(
                !reason.contains("answered with a pending and then"),
                "{case}: {reason}"
            );
            assert!(
                !reason.contains("No response was expected"),
                "{case}: {reason}"
            );
        }
    }

    /// What does not attach: an exchange a refusal ended, and an item whose RxFlag also says it
    /// is a transmit indication or a loopback. What attaches changes no outcome the exchange
    /// already has, and no send evidence a transmit indication gave.
    #[test]
    fn a_start_of_message_changes_no_outcome_and_skips_what_cannot_own_it() {
        let refused = exchange::End::NotSent(exchange::RefusalCause::EarlierResponse);
        let lost = exchange::End::VciLost {
            refused_after_loss: true,
        };
        let mixed = |flags| UnattachedItem {
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, flags],
                identifier: Some(0x7E8),
                bytes: Vec::new(),
            },
            ..tx_done(0x7E8)
        };
        let one = |evidence, unbound| {
            let (mut entries, unattached) = run_exchanges(vec![(evidence, unbound)]);
            (entries.remove(0), unattached.len())
        };
        // The refused exchanges take nothing.
        for end in [refused, lost] {
            let (entry, unattached) = one(vci_ended(end), vec![start_of_message(0x7E8)]);
            assert_eq!(entry["sent"], "not_sent");
            assert_eq!(unattached, 1);
        }
        // START with TX_INDICATION, and with TX_MSG_TYPE, are not starts.
        let (entry, unattached) = one(
            vci_ended(exchange::End::NoResponse),
            vec![mixed(0x0A), mixed(0x03)],
        );
        assert_eq!(entry["outcome"]["result"], "no_response");
        assert_eq!(entry["sent"], "unknown");
        assert_eq!(unattached, 2);
        // A final response keeps its outcome and classification.
        let (entry, unattached) = one(
            vci_ended(exchange::End::Positive(vec![0x62, 0xF1, 0x90])),
            vec![start_of_message(0x7E8)],
        );
        assert_eq!(entry["outcome"]["result"], "positive");
        let (without, _) = one(
            vci_ended(exchange::End::Positive(vec![0x62, 0xF1, 0x90])),
            Vec::new(),
        );
        assert_eq!(entry["classification"], without["classification"]);
        assert_eq!(unattached, 0);
        // A transmit indication stays the send's evidence; the start is the response's.
        let (entry, unattached) = one(
            vci_ended(exchange::End::NoResponse),
            vec![tx_done(0x7E0), start_of_message(0x7E8)],
        );
        assert_eq!(entry["sent_evidence"], "tx_indication");
        assert_eq!(entry["outcome"]["result"], "response_incomplete");
        assert_eq!(unattached, 0);
        let (entry, unattached) = one(
            vci_ended(exchange::End::VciDidNotEnd),
            vec![start_of_message(0x7E8)],
        );
        assert_eq!(entry["outcome"]["result"], "vci_did_not_end");
        assert_eq!(entry["sent"], "confirmed");
        assert_eq!(unattached, 0);
        let (entry, _) = one(
            vci_ended(exchange::End::LinkError(
                exchange::LinkErrorKind::VciHardwareFault,
            )),
            vec![start_of_message(0x7E8)],
        );
        assert_eq!(entry["outcome"]["result"], "link_error");
    }

    /// A response start alone is an answer but not a matched answer (ADR-354): the later
    /// exchange counts it as answered, keeps H2 among its readings, and the started exchange's
    /// own `response_incomplete` reason names H2. A final response is a matched answer.
    #[test]
    fn a_started_response_is_answered_but_not_matched_for_the_later_exchanges() {
        let (entries, _) = run_exchanges(vec![
            (
                vci_ended(exchange::End::NoResponse),
                vec![start_of_message(0x7E8)],
            ),
            (vci_ended(exchange::End::NoResponse), Vec::new()),
        ]);
        assert_eq!(entries[0]["outcome"]["result"], "response_incomplete");
        assert_eq!(entries[0]["sent_evidence"], "answer");
        let started_reason = entries[0]["reason"].as_str().unwrap();
        assert!(started_reason.contains("(H2)"), "{started_reason}");
        let history = &entries[1]["link_history"];
        assert_eq!(history["answered"], 1);
        assert_eq!(history["answered_same_match"], 0);
        let later_reason = entries[1]["reason"].as_str().unwrap();
        assert!(later_reason.contains("(H2)"), "{later_reason}");

        // With a final response the next exchange has a matched answer, which excludes H2 for
        // a later start alone too.
        let (entries, _) = run_exchanges(vec![
            (
                vci_ended(exchange::End::Positive(vec![0x62, 0xF1, 0x90])),
                Vec::new(),
            ),
            (
                vci_ended(exchange::End::NoResponse),
                vec![start_of_message(0x7E8)],
            ),
        ]);
        assert_eq!(entries[1]["link_history"]["answered_same_match"], 1);
        assert_eq!(entries[1]["outcome"]["result"], "response_incomplete");
        let matched_reason = entries[1]["reason"].as_str().unwrap();
        assert!(!matched_reason.contains("(H2)"), "{matched_reason}");

        // A pending in the incomplete exchange itself excludes H2 too.
        let mut with_pending = vci_ended(exchange::End::NoResponse);
        with_pending.pending_visibility = exchange::PendingVisibility::Forwarded;
        with_pending.pending = vec![exchange::Pending {
            vci_us: None,
            clock: None,
            epoch: None,
            agent_us: 10,
            since_previous_ms: None,
        }];
        let (entries, _) = run_exchanges(vec![(with_pending, vec![start_of_message(0x7E8)])]);
        assert_eq!(entries[0]["outcome"]["result"], "response_incomplete");
        let pending_reason = entries[0]["reason"].as_str().unwrap();
        assert!(!pending_reason.contains("(H2)"), "{pending_reason}");
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

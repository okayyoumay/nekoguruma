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
use serde::{Deserialize, Serialize};

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
    /// The opening lines, a lost VCI's record (`vci_loss`) or the `job_end` could not be made
    /// durable.
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
    LateItem(LateItemEntry<'a>),
    PrimitiveEnded(Box<PrimitiveEndedEntry>),
    ClockEpoch(clocks::ClockEpoch),
    VciLoss(VciLoss),
    VciLossSettled(Box<VciLossSettledEntry>),
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
    /// An error event, with its extra error information as the worker gave it (ADR-351), and the
    /// kind of link error it is, when it is one (design 8.11 "Link error": not a receive
    /// timeout, which is an outcome, nor a lost VCI).
    Error {
        event: String,
        extra_error_info: Option<u32>,
        extra_error_info_name: Option<String>,
        link_error: Option<exchange::LinkErrorKind>,
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

impl UnattachedItem {
    /// The late indication this is (design 8.11 `late_item`, ADR-370), with the identifier it
    /// carries: an unbound transmit indication or loopback echo, by its request identifier, or
    /// an unbound start-of-message indication, by its response identifier. One bound to an
    /// earlier primitive is that primitive's (ADR-367), and one with no identifier names no
    /// exchange.
    fn late_indication(&self) -> Option<(LateIndication, u32)> {
        let UnattachedData::Result {
            rx_flag,
            identifier: Some(identifier),
            ..
        } = &self.data
        else {
            return None;
        };
        let flags = *rx_flag.get(3)?;
        let kind = if self.earlier_primitive {
            return None;
        } else if flags & RX_FLAG_TX_INDICATION != 0 {
            LateIndication::Transmit(exchange::TransmitEvidence::TxIndication)
        } else if flags & RX_FLAG_TX_MSG_TYPE != 0 {
            LateIndication::Transmit(exchange::TransmitEvidence::Loopback)
        } else if flags & RX_FLAG_START_OF_MESSAGE != 0 {
            LateIndication::Start
        } else if is_received(rx_flag) && self.pending_bytes().is_some() {
            LateIndication::Pending
        } else {
            return None;
        };
        Some((kind, *identifier))
    }

    /// The bytes of this item when it is a received negative response with the response pending
    /// code (`7F`, a service identifier, 0x78), which a worker forwards: a late item, not a late
    /// response (design 8.11 `late_item`).
    fn pending_bytes(&self) -> Option<&[u8]> {
        match &self.data {
            UnattachedData::Result { bytes, .. }
                if bytes.len() >= 3
                    && bytes[0] == exchange::SID_NEGATIVE
                    && bytes[2] == exchange::NRC_PENDING =>
            {
                Some(bytes)
            }
            _ => None,
        }
    }
}

/// What a late indication shows of the exchange it settles (design 8.11 `late_item`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LateIndication {
    /// Its request went out, as this evidence shows.
    Transmit(exchange::TransmitEvidence),
    /// Its response began.
    Start,
    /// The ECU answered with a pending, forwarded by the worker and bound to no primitive.
    Pending,
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
pub(crate) const SINGLE_FRAME_MAX: usize = 7;

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
    /// Two or more exchanges could own the response, so it is the `unattached` event logged as
    /// this `seq`, and no exchange is chosen as its owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    unattached: Option<u64>,
    exchanges: Vec<Settled>,
    /// The latest later exchange that had left the window ([`exchange::WINDOW`]) and is not
    /// reclassified.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_reclassified_through: Option<u64>,
    text: String,
    classification: exchange::Classification,
}

/// A `late_item` settling entry for an indication (design 8.11 "Settlement", ADR-370): the item,
/// as an `unattached` event would give it, the exchange it settles with that exchange's `sent`
/// and outcome restated, and the later exchanges its answer reclassifies.
#[derive(Serialize)]
struct LateItemEntry<'a> {
    refers_to: Vec<u64>,
    #[serde(flatten)]
    item: UnattachedEntry<'a>,
    /// Two or more exchanges could own the item, so it is the `unattached` event logged as this
    /// `seq`, and no exchange is chosen as its owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    unattached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent: Option<exchange::Sent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent_evidence: Option<exchange::SentEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<exchange::Outcome>,
    exchanges: Vec<Settled>,
    /// As in `late_response`.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_reclassified_through: Option<u64>,
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
    /// The link errors it reported at job end beside its end.
    pub link_errors: Vec<exchange::LinkErrorKind>,
    /// The transmit and start-of-message indications of no primitive read at job end, which
    /// may be its exchange's.
    pub indications: Vec<UnattachedItem>,
    /// The time of a transmit indication bound to the primitive read at job end, which confirms
    /// its send (design 8.11 `sent_evidence` `tx_indication`).
    pub tx_indication: Option<exchange::VciTime>,
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
    /// As in `late_response`.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_reclassified_through: Option<u64>,
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

impl Settled {
    /// A later exchange reclassified with the link error the settling entry places before it.
    fn after_link_error(later: exchange::Reclassified) -> Self {
        Self {
            seq: later.seq,
            link: later.link,
            origin: later.origin,
            request: later.request,
            classification: later.classification,
            reason: format!(
                "Reclassified with the link error this entry places before it, which ends the matched answers from before it: {}",
                later.reason
            ),
        }
    }

    /// A later exchange reclassified with the answer the settling entry adds to the exchange
    /// logged as `owner`.
    fn reclassified(owner: u64, later: exchange::Reclassified) -> Self {
        Self {
            seq: later.seq,
            link: later.link,
            origin: later.origin,
            request: later.request,
            classification: later.classification,
            reason: format!(
                "Reclassified with the answer this entry adds to the exchange logged as {owner}: {}",
                later.reason
            ),
        }
    }
}

/// The text a settling entry adds when a later exchange had left the window and is not
/// reclassified.
fn not_reclassified_text(through: Option<u64>) -> String {
    through.map_or_else(String::new, |through| {
        format!(
            " Later exchanges logged up to {through} are past the last {} the log keeps and are not reclassified.",
            exchange::WINDOW
        )
    })
}

/// Reclassifies the later exchanges on a link whose earlier exchange `seq` a settling entry
/// gave `answer`: first, when that answer accepts its ECUReset or session change or is an
/// unusable positive response to one (`restart`, [`exchange::ExchangeEntry::late_restart`]), the
/// end of the evidence from before it, as in time; then the answer itself. Gives each later
/// exchange changed, in `seq` order, and the largest `seq` of one past the window.
fn settle_later(
    state: &mut exchange::LinkState,
    seq: u64,
    restart: Option<(bool, u64)>,
    answer: &exchange::SettledAnswer,
) -> (Vec<exchange::Reclassified>, Option<u64>) {
    let (mut changed, through) = restart.map_or((Vec::new(), None), |(accepting, earliest)| {
        state.reset_after(seq, earliest, accepting)
    });
    let (answered, answered_through) = state.reclassify_after(answer);
    for later in answered {
        changed.retain(|earlier| earlier.seq != later.seq);
        changed.push(later);
    }
    changed.sort_by_key(|later| later.seq);
    (changed, through.max(answered_through))
}

/// A lost VCI's record (`vci_loss`, design 8.11 "Settlement", ADR-373): only what its
/// settlement needs, since the agent does not reconnect within a run and the run that reconnects
/// reads this record alone from the log. Written at the loss, and again, whole, after every
/// request the worker refuses afterwards on the offline link; the last one counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VciLoss {
    /// The `seq` of the loss's first record, which its later records restate.
    loss: u64,
    /// The agent epoch of the run the VCI was lost in.
    agent_epoch: u32,
    /// The exchanges the loss ended, each with its classification.
    ended: Vec<LossExchange>,
    /// The requests the worker refused afterwards on the offline link, naming the loss.
    refused: Vec<LossExchange>,
    /// Each clock's last time before the loss, with when the host read it.
    clocks: Vec<LastTime>,
    /// The VCI's serial number, when the API gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serial_number: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct LossExchange {
    seq: u64,
    classification: exchange::Classification,
}

/// An exchange `vci_loss_settled` settles: its `seq` and classification with the reason. The
/// record keeps no link, origin or request, which the exchange's own entry gives (ADR-373).
#[derive(Serialize)]
struct LossSettled {
    seq: u64,
    classification: exchange::Classification,
    reason: String,
}

/// A clock's last time before the loss: raw, with its epoch and when the host read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct LastTime {
    clock: exchange::Clock,
    #[serde(flatten)]
    time: clocks::EndedEpoch,
}

/// The first reading of the VCI's clock after it connected, which the runner takes when the log
/// holds a loss to settle: the time with the clock the worker names, and when the agent read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectReading {
    pub time: exchange::VciTime,
    pub read: AgentNow,
}

/// Whether the VCI kept its power across the loss (`vci_loss_settled`'s `power_kept`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerKept {
    Yes,
    No,
    Unknown,
}

impl Serialize for PowerKept {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Yes => serializer.serialize_bool(true),
            Self::No => serializer.serialize_bool(false),
            Self::Unknown => serializer.serialize_str("unknown"),
        }
    }
}

/// A time of the comparison: raw, with its clock, epoch, and the agent's time when read.
#[derive(Serialize)]
struct LossTime {
    clock: exchange::Clock,
    epoch: u32,
    us: u64,
    read_us: u64,
    agent_epoch: u32,
}

/// The inputs and the comparison of `vci_loss_settled`, so a reader can redo it.
#[derive(Serialize)]
struct LossClock {
    /// The VCI's own clock's last time before the loss.
    last_before: Option<LossTime>,
    /// The first reading of the VCI's clock after the reconnect.
    reading: Option<LossTime>,
    /// The gap the agent's clock measured between the two; `null` across agent epochs.
    agent_gap_us: Option<u64>,
    expected_us: Option<u64>,
    tolerance_us: Option<u64>,
    drift_ppm: u32,
    drift_from: &'static str,
    clock_margin_us: u32,
    clock_margin_from: &'static str,
    fits_carried_on: Option<bool>,
    fits_restarted: Option<bool>,
}

/// `vci_loss_settled` (design 8.11 "Settlement", ADR-373).
#[derive(Serialize)]
struct VciLossSettledEntry {
    /// The `seq` of the loss's first record.
    loss: u64,
    refers_to: Vec<u64>,
    exchanges: Vec<LossSettled>,
    clock: LossClock,
    /// The serial numbers before and after match; `null` when the API gave none on either.
    serial_match: Option<bool>,
    power_kept: PowerKept,
    text: String,
    classification: exchange::Classification,
    reason: String,
}

const SUPPLY_NOT_EXCLUDED: &str = " a drop of the vehicle's supply is not excluded.";

/// Whether the power was kept, and why, from the comparison: whether a time of the VCI's own
/// clock, as the worker names it (ADR-353), was read on both sides of the loss, the `fit` when
/// the gap could be measured, and whether the serial numbers match. Nothing here depends on the
/// worker's API (design principle 6): a worker whose clock reading is not the VCI's own time
/// base names it `worker`, and one that gives no serial number gives none.
fn power_kept(
    readings: bool,
    fit: Option<clocks::LossFit>,
    serial_match: Option<bool>,
) -> (PowerKept, String) {
    let unknown = |why: &str| (PowerKept::Unknown, format!("{why};{SUPPLY_NOT_EXCLUDED}"));
    if !readings {
        return unknown(
            "No time of the VCI's own clock was read both before the loss and after the reconnect",
        );
    }
    let Some(fit) = fit else {
        return unknown(
            "The VCI reconnected in a later run of the agent, whose clock cannot measure the time since the loss",
        );
    };
    match (fit.carried_on, fit.restarted, serial_match) {
        (true, false, Some(true)) => (
            PowerKept::Yes,
            "The VCI kept its power across the loss, so the vehicle's supply did not drop: the loss was the PC's connection to the VCI.".to_owned(),
        ),
        (true, false, _) => unknown(
            "The VCI's clock carried on, but its serial number does not show that the same VCI reconnected",
        ),
        (false, true, _) => (
            PowerKept::No,
            format!("The VCI's clock started again during the loss, so it was powered off;{SUPPLY_NOT_EXCLUDED}"),
        ),
        (true, true, _) => unknown(
            "The VCI's clock reads what both a clock that carried on and one that started again would read",
        ),
        (false, false, _) => unknown(
            "The VCI's clock reads neither what a clock that carried on nor what one that started again would read",
        ),
    }
}

/// The text a settling entry adds when the answer it adds ends the evidence from before its
/// exchange ([`settle_later`]).
fn restart_text(restart: Option<(bool, u64)>) -> &'static str {
    match restart {
        Some((true, _)) => {
            " The response accepts that exchange's session change or ECUReset: the matched answers and the owed requests from before it end, as they would have in time."
        }
        Some((false, _)) => {
            " The response to that exchange's session change or ECUReset is positive but unusable: the matched answers from before it end, as they would have in time."
        }
        None => "",
    }
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
                ..
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
    /// When the host last read an item of each link, by link: a link error whose time cannot
    /// be placed may have come during the exchanges written since (design 8.11 `late_item`).
    link_reads: Vec<(u32, AgentNow)>,
    /// This run's lost VCI as its last `vci_loss` record gave it (ADR-373).
    loss: Option<VciLoss>,
    /// A loss an earlier run recorded that no `vci_loss_settled` settled yet: the next
    /// `vci_connected` settles it.
    unsettled: Option<VciLoss>,
    /// The serial number of the run's VCI, from `vci_connected`, when the API gave one.
    serial_number: Option<u32>,
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
            link_reads: Vec::new(),
            loss: None,
            unsettled: scan.unsettled,
            serial_number: None,
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
        // When the host read each item, for the link errors whose time cannot be placed.
        let mut batch_reads: Vec<(u32, AgentNow)> = unbound
            .iter()
            .filter_map(|item| item.read.map(|read| (item.link, read)))
            .collect();
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
        // An unbound indication the one ended exchange that could own it, and not this one,
        // owns is that exchange's, read late (design 8.11 `late_item`, ADR-370). This exchange
        // was in flight with the identifier unless the indication's VCI time comes before its
        // own transmit done (bound to it or carried by its response), on one clock and epoch:
        // nothing it sent or received comes before its transmit (the maintainer's decisions,
        // 2026-10-28). Settled first, so this exchange then takes its own indications as it
        // would have.
        let own_starts: Vec<(exchange::Clock, clocks::Placed)> = vci
            .tx_done
            .and_then(|time| time.placed())
            .into_iter()
            .collect();
        let awaits = !matches!(
            evidence.end,
            exchange::End::NotSent(_)
                | exchange::End::VciLost {
                    refused_after_loss: true
                }
        );
        let in_flight_tx = match evidence.target {
            exchange::Target::Endpoint { tx_id, .. } if awaits => Some(tx_id),
            _ => None,
        };
        let in_flight_rx = awaits.then_some(evidence.rx_id);
        let in_flight_expected = evidence.expected.clone();
        let in_flight = |item: &UnattachedItem, placed, kind, id| {
            let uses = match kind {
                LateIndication::Transmit(_) => in_flight_tx == Some(id),
                LateIndication::Start => in_flight_rx == Some(id),
                LateIndication::Pending => {
                    in_flight_rx == Some(id)
                        && item
                            .pending_bytes()
                            .is_some_and(|bytes| in_flight_expected.pending(bytes))
                }
            };
            uses && !before_starts(&own_starts, item.clock, placed)
        };
        let mut index = 0;
        while index < unbound.len() {
            let Some((kind, id)) = unbound[index].late_indication() else {
                index += 1;
                continue;
            };
            let (owners, all) =
                late_indication_owners(&inner.links[at].1, &unbound[index], kind, id);
            if in_flight(&unbound[index], unbound_placed[index], kind, id)
                || !all
                || owners.len() != 1
            {
                index += 1;
                continue;
            }
            let item = unbound.remove(index);
            let placed = unbound_placed.remove(index);
            inner.late_item(at, owners[0], kind, &item, placed)?;
        }
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
        // A link error of no primitive read during the exchange is a fault seen in it (design
        // 8.11 "Link error"): the worker gives link-level errors no primitive handle. One whose
        // time comes before the exchange's earliest placed VCI time is an earlier exchange's, read late,
        // and stays an item of its own; one whose time cannot be set against the exchange's is
        // taken as the exchange's, since it was read during it.
        // The exchange's span starts at its transmit done or the start of its response, the
        // earliest on each clock and epoch: its end times do not start it (the maintainer's
        // decision, 2026-10-27).
        let starts: Vec<(exchange::Clock, clocks::Placed)> = [vci.tx_done, vci.rx_start]
            .into_iter()
            .flatten()
            .filter_map(|time| time.placed())
            .collect();
        let response_read_us = vci.response_read_us;
        // A refused exchange sent nothing that a link error could have hit.
        let refused = matches!(
            evidence.end,
            exchange::End::NotSent(_)
                | exchange::End::VciLost {
                    refused_after_loss: true
                }
        );
        let mut at_item = 0;
        // Whether this block named the event in `detail`, so a later one it takes names it
        // instead: the last error seen beside the end is the one named.
        let mut named_here = false;
        let mut own_unplaced: Vec<(UnattachedItem, exchange::LinkErrorKind)> = Vec::new();
        while at_item < unbound.len() && !refused {
            let item = &unbound[at_item];
            let (kind, event, extra_error_info, extra_error_info_name) = match &item.data {
                UnattachedData::Error {
                    link_error: Some(kind),
                    event,
                    extra_error_info,
                    extra_error_info_name,
                } if !item.earlier_primitive => (
                    *kind,
                    event.clone(),
                    *extra_error_info,
                    extra_error_info_name.clone(),
                ),
                _ => {
                    at_item += 1;
                    continue;
                }
            };
            let placed = unbound_placed[at_item];
            let before_span = placed.is_some_and(|placed| {
                starts
                    .iter()
                    .filter(|(clock, start)| {
                        item.clock == Some(*clock) && start.epoch == placed.epoch
                    })
                    .map(|(_, start)| start.unwrapped)
                    .min()
                    .is_some_and(|start| placed.unwrapped < start)
            });
            if before_span {
                at_item += 1;
                continue;
            }
            // One whose time cannot be set against the exchange's, on no clock or on a clock and
            // epoch none of its own times are on, may also have come during the exchanges
            // written since the link's items were last read (design 8.11 `late_item`).
            let set_against = placed.is_some_and(|placed| {
                starts
                    .iter()
                    .any(|(clock, start)| item.clock == Some(*clock) && start.epoch == placed.epoch)
            });
            if !set_against {
                own_unplaced.push((item.clone(), kind));
            }
            // The answer counts as one since the error only when the host read the error
            // before it, as for an error bound to the primitive.
            let read_before_answer = matches!(
                (item.read, response_read_us),
                (Some(read), Some(answer)) if read.monotonic_us < answer
            );
            evidence.link_errors_before_answer = read_before_answer
                && (evidence.link_errors.is_empty() || evidence.link_errors_before_answer);
            evidence.link_errors.push(kind);
            // Named, with its extra error information and its time, as the last error bound to
            // the primitive is when no event ended the exchange (ADR-351 item 2).
            if named_here || evidence.detail.error_event.is_none() {
                named_here = true;
                evidence.detail.error_event = Some(event);
                evidence.detail.extra_error_info = extra_error_info;
                evidence.detail.extra_error_info_name = extra_error_info_name;
                vci.error =
                    Some(exchange::VciTime::new(u64::from(item.vci_us), item.clock).at(placed));
            }
            unbound.remove(at_item);
            unbound_placed.remove(at_item);
        }
        // The items bound to the primitive were read too: its pendings, its transmit
        // confirmation, its response, and its end. A refused exchange read none.
        let agent = &evidence.times.agent;
        let bound_reads = evidence
            .pending
            .iter()
            .map(|pending| pending.agent_us)
            .chain(agent.tx_confirmed_us)
            .chain(vci.response_read_us)
            .chain([agent.end_us])
            .filter(|_| !refused);
        batch_reads.extend(bound_reads.map(|monotonic_us| {
            (
                evidence.link,
                AgentNow {
                    epoch: agent.epoch,
                    monotonic_us,
                    wall_unix_ms: 0,
                },
            )
        }));
        // A link error the exchange did not take came before it, or belongs to an earlier
        // primitive: it is a `late_item` (design 8.11 `late_item`, ADR-372), as is one the
        // exchange took whose time cannot be placed, for the exchanges before it.
        let mut at_item = 0;
        while at_item < unbound.len() {
            let UnattachedData::Error {
                link_error: Some(kind),
                ..
            } = unbound[at_item].data
            else {
                at_item += 1;
                continue;
            };
            let since = inner.read_since(&unbound[at_item], &batch_reads);
            if inner.late_link_error(&unbound[at_item], unbound_placed[at_item], kind, since)? {
                unbound.remove(at_item);
                unbound_placed.remove(at_item);
            } else {
                at_item += 1;
            }
        }
        for (item, kind) in own_unplaced {
            let since = inner.read_since(&item, &batch_reads);
            inner.late_link_error(&item, None, kind, since)?;
        }
        inner.note_reads(&batch_reads);
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
        // A response read during the exchange that it or an earlier exchange could own leaves
        // its own classification at least `undetermined` (design 8.11 `late_response`).
        let mut may_own_unattached = false;
        // An indication read during it that it or an earlier exchange could own does the same
        // (design 8.11 `late_item`, ADR-370), when the indication would change it: a transmit
        // indication while its own send stays unknown, a start while it has no answer of its own
        // (the maintainer's decision, 2026-10-28).
        let mut may_own_transmit = false;
        let mut may_own_start = false;
        for (item, placed) in unbound.iter().zip(unbound_placed) {
            let owner = late_response_owner(&inner.links[at].1, item, &evidence);
            if let Some(LateOwner::One(owner)) = owner {
                inner.late_response(at, owner, item, placed)?;
                continue;
            }
            let event = Event::Unattached(UnattachedEntry::of(item, placed));
            let unattached = inner.append_with(|seq, line| {
                serde_json::to_writer(line, &Line { seq, event: &event })
            })?;
            if let Some(LateOwner::Several(owners, in_flight)) = owner {
                may_own_unattached |= in_flight;
                if !owners.is_empty() {
                    inner.late_response_unowned(at, &owners, item, placed, unattached)?;
                }
            }
            if let Some((kind, id)) = item.late_indication() {
                let (owners, all) = late_indication_owners(&inner.links[at].1, item, kind, id);
                let in_flight = in_flight(item, placed, kind, id);
                if owners.len() + usize::from(in_flight) + usize::from(!all) >= 2 {
                    match kind {
                        LateIndication::Transmit(_) => may_own_transmit |= in_flight,
                        LateIndication::Start | LateIndication::Pending => {
                            may_own_start |= in_flight
                        }
                    }
                    if !owners.is_empty() {
                        inner.late_item_unowned(at, &owners, item, placed, unattached)?;
                    }
                }
            }
        }
        let history = inner.links[at].1.history_for(&evidence);
        let clears = inner.links[at].1.clears();
        let rx_id = evidence.rx_id;
        let start_unattached = unbound
            .iter()
            .any(|item| item.is_start_indication_for(rx_id));
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
            start_unattached,
            late_start: None,
            late_pendings: Vec::new(),
            floor: None,
            clears,
            matched: false,
        };
        let mut entry = exchange::ExchangeEntry::build(inner.next_seq, evidence, history);
        let may_own_indication = (may_own_transmit && entry.sent == exchange::Sent::Unknown)
            || (may_own_start && !entry.link_history.this_exchange);
        let floor_reason = match (may_own_unattached, may_own_indication) {
            (true, true) => Some(format!("{MAY_OWN_UNATTACHED}{MAY_OWN_INDICATION}")),
            (true, false) => Some(MAY_OWN_UNATTACHED.to_owned()),
            (false, true) => Some(MAY_OWN_INDICATION.to_owned()),
            (false, false) => None,
        };
        if let Some(floor_reason) = &floor_reason
            && entry.classification < exchange::Classification::Undetermined
        {
            entry.classification = exchange::Classification::Undetermined;
            entry.reason.push_str(floor_reason);
        }
        let seq = inner.append_with(|_, line| serde_json::to_writer(line, &entry))?;
        inner.links[at].1.record(&entry);
        inner.links[at].1.log(&entry, kept.evidence.clone(), clears);
        // A reclassification from its history alone does not clear that.
        if let Some(floor_reason) = &floor_reason {
            inner.links[at]
                .1
                .floor(seq, exchange::Classification::Undetermined, floor_reason);
        }
        let clears = inner.links[at].1.clears();
        let lost = match kept.evidence.end {
            exchange::End::VciLost { refused_after_loss } => Some(refused_after_loss),
            _ => None,
        };
        // Only an exchange the VCI ended can be settled by a late response (ADR-365), and one
        // the agent's deadline ended by its primitive's end (ADR-366).
        if kept.evidence.vci_ended || matches!(kept.evidence.end, exchange::End::VciDidNotEnd) {
            inner.links[at].1.keep(
                seq,
                exchange::Kept {
                    classification: entry.classification,
                    sent: entry.sent,
                    clears,
                    matched: entry.matched_answer(),
                    floor: floor_reason
                        .map(|reason| (exchange::Classification::Undetermined, reason)),
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
        if let Some(refused) = lost {
            inner.note_loss(seq, entry.classification, refused)?;
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
    /// primitive the agent stopped waiting for, from how it ended at job end or before the
    /// link's next request (design 8.11 "Settlement", ADR-366). The entry's `seq`; `None` when
    /// the log keeps no such exchange, or once the log has failed.
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
                    reason: "The agent cancelled the primitive before it ended; this settles nothing, and the exchange keeps its classification.".to_owned(),
                }],
                not_reclassified_through: None,
                text: format!(
                    "The agent cancelled the primitive of the exchange logged as {seq}."
                ),
                classification: kept.classification,
            }));
            return inner.append_with(|seq, line| {
                serde_json::to_writer(line, &Line { seq, event: &event })
            });
        };
        let mut observed = kept.evidence.vci_limits_past_by_us.is_some();
        let mut evidence = kept.evidence.clone();
        // A link error read with the end, or that ended it, has no known order against an
        // answer read with it: it ends that answer's matched answer, as `record` applies it.
        let error_with_end = match &end {
            exchange::End::LinkError(kind) => Some(*kind),
            _ => ended.link_errors.first().copied(),
        };
        let lost = matches!(end, exchange::End::VciLost { .. });
        evidence.end = end;
        evidence.detail = ended.detail;
        evidence.vci_ended = ended.vci_ended;
        evidence.link_errors.extend(ended.link_errors);
        evidence.vci_limits_past_by_us = None;
        // An indication of no primitive read at job end is the exchange's, as it would have
        // been during it, when no other exchange on the link could own it (design 8.11 "Link
        // settings for the log").
        let state = &inner.links[at].1;
        let rx_id = evidence.rx_id;
        let late_start = if state.may_own_start_indication_besides(rx_id, seq) {
            None
        } else {
            ended
                .indications
                .iter()
                .rev()
                .find(|item| item.is_start_indication_for(rx_id))
                .cloned()
        };
        if late_start.is_some() {
            evidence.response_started = true;
        }
        // A transmit indication bound to the primitive is the exchange's own; one of no
        // primitive is taken only without it.
        let bound_transmit = ended
            .tx_indication
            .filter(|_| evidence.transmit_evidence.is_none());
        let late_transmit = match evidence.target {
            exchange::Target::Endpoint { tx_id, .. }
                if evidence.transmit_evidence.is_none()
                    && bound_transmit.is_none()
                    && !state.may_own_tx_indication_besides(kept.sent) =>
            {
                ended
                    .indications
                    .iter()
                    .find(|item| item.is_tx_indication_for(tx_id))
                    .cloned()
            }
            _ => None,
        };
        // A response that began and did not complete, as an exchange's end gives it.
        if evidence.vci_ended
            && evidence.response_started
            && matches!(evidence.end, exchange::End::NoResponse)
        {
            evidence.end = exchange::End::ResponseIncomplete;
        }
        // The pendings, read before the end, are placed first, after the kept ones.
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
        // The end's times, read at job end, are placed nearest their clock's latest time.
        let mut vci = ended.vci;
        let response_tx_done = vci.tx_done.is_some();
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
        if let Some(time) = bound_transmit {
            let placed = inner.place_unread(time.clock, time.us, now)?;
            vci.tx_done = Some(time.at(placed))
                .into_iter()
                .chain(vci.tx_done)
                .reduce(exchange::VciTime::prefer);
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxIndication);
        }
        if let Some(indication) = late_transmit {
            let placed = inner.place_unread(indication.clock, u64::from(indication.vci_us), now)?;
            vci.tx_done = Some(
                exchange::VciTime::new(u64::from(indication.vci_us), indication.clock).at(placed),
            )
            .into_iter()
            .chain(vci.tx_done)
            .reduce(exchange::VciTime::prefer);
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxIndication);
        }
        // A transmit done the response's own item carries confirms the send when no indication
        // did, as for an exchange (design 8.11 `sent_evidence` `tx_done_time`).
        if response_tx_done && evidence.transmit_evidence.is_none() {
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxDoneTime);
        }
        // The reading at the deadline, which the kept `system` rests on.
        let reading = kept.evidence.times.vci.and_then(|vci| vci.deadline);
        let reading_at = reading.and_then(|reading| reading.time.placed());
        // The `system` the reading at the deadline gave rested on no response under way then:
        // a start read at job end that may be the response's (whichever exchange owns it, or
        // naming no identifier) at or before that reading, or one whose order with it is
        // unknown, refutes it (design 8.11 "Settlement").
        let after_reading = |time: exchange::VciTime| match (time.placed(), reading_at) {
            (Some((clock, start)), Some((own, reading))) => {
                clock == own && start.epoch == reading.epoch && start.unwrapped > reading.unwrapped
            }
            _ => false,
        };
        for item in ended
            .indications
            .iter()
            .filter(|item| item.may_be_start_indication_for(rx_id))
        {
            let placed = inner.place_unread(item.clock, u64::from(item.vci_us), now)?;
            if !after_reading(exchange::VciTime::new(u64::from(item.vci_us), item.clock).at(placed))
            {
                observed = false;
            }
        }
        // The latest start: one read at job end, else the one the exchange had attached.
        let latest_start = match late_start {
            Some(start) => {
                let placed = inner.place_unread(start.clock, u64::from(start.vci_us), now)?;
                Some(exchange::VciTime::new(u64::from(start.vci_us), start.clock).at(placed))
            }
            None => kept.evidence.times.vci.and_then(|vci| vci.rx_start),
        };
        // The latest start is the response's when it came after the last pending, the job
        // end's included, and the response spans frames, as for an exchange.
        if let Some(time) = latest_start {
            let after_pending = match (pendings.last(), time.placed()) {
                (None, _) => true,
                (Some(Some((clock, at))), Some((own, start))) => {
                    *clock == own && at.epoch == start.epoch && start.unwrapped >= at.unwrapped
                }
                _ => false,
            };
            let spans_frames = evidence
                .end
                .response()
                .is_none_or(|response| response.len() > SINGLE_FRAME_MAX);
            if after_pending && spans_frames {
                vci.rx_start = Some(time)
                    .into_iter()
                    .chain(vci.rx_start)
                    .reduce(exchange::VciTime::prefer);
            }
        }
        // The transmit done the response's item carries, on its clock, over the indication's
        // the exchange had attached, as for an exchange (`VciTime::prefer`).
        let transmit = vci.tx_done.and_then(|time| time.placed()).or(kept.transmit);
        exchange::pending_intervals(&mut evidence.pending, &placed_all, transmit);
        if let Some(limits) = kept.limits {
            // A start with the response's identifier that another exchange could own still
            // means this response may have begun, as for an exchange.
            let response_may_have_begun = evidence.response_started
                || !limits.start_indications
                || kept.start_unattached
                || ended
                    .indications
                    .iter()
                    .any(|item| item.is_start_indication_for(rx_id));
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
            // A pending read at job end from at or before the reading, or one whose order with
            // it is unknown, may extend the limit past it: the kept `system` stands only while
            // the VCI's limits are still past by the reading with every such pending.
            if observed {
                let at_reading: Vec<_> = pendings
                    .iter()
                    .copied()
                    .filter(|pending| match (pending, reading_at) {
                        (Some((clock, at)), Some((own, reading))) => {
                            !(*clock == own
                                && at.epoch == reading.epoch
                                && at.unwrapped > reading.unwrapped)
                        }
                        _ => true,
                    })
                    .collect();
                observed = reading.is_some_and(|reading| {
                    timing::past_vci_limits(
                        limits,
                        evidence.absorbing(),
                        transmit,
                        &at_reading,
                        reading_at,
                        reading.round_trip_us,
                    )
                    .is_some()
                });
            }
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
        let settled_evidence = evidence.clone();
        let entry = exchange::ExchangeEntry::build(seq, evidence, history);
        let (classification, reason) = exchange::settle_primitive_ended(&entry, observed);
        // The answers its end adds count for the later exchanges on the link.
        let state = &mut inner.links[at].1;
        state.settled(
            seq,
            classification,
            &reason,
            settled_evidence,
            final_response,
        );
        let mut answer = entry.settled_answer(&kept);
        if error_with_end.is_some() {
            answer.no_matched_answer();
        }
        let restart = entry.late_restart();
        let (later, not_reclassified) = settle_later(state, seq, restart, &answer);
        let refers_to = std::iter::once(seq)
            .chain(later.iter().map(|later| later.seq))
            .collect();
        let top = later
            .iter()
            .map(|later| later.classification)
            .fold(classification, exchange::Classification::max);
        let event = Event::PrimitiveEnded(Box::new(PrimitiveEndedEntry {
            refers_to,
            link,
            cancelled_by_agent: false,
            sent: Some(entry.sent),
            sent_evidence: Some(entry.sent_evidence),
            text: format!(
                "The primitive of the exchange logged as {seq} ended after the agent had stopped waiting for it. {}{}{}",
                entry.text,
                restart_text(restart),
                not_reclassified_text(not_reclassified)
            ),
            outcome: Some(entry.outcome),
            answers: entry.answers,
            vci: times,
            pending: entry.pending,
            timing: Some(entry.timing),
            detail: entry.detail,
            exchanges: std::iter::once(Settled {
                seq,
                link,
                origin: kept.origin,
                request: kept.request,
                classification,
                reason,
            })
            .chain(
                later
                    .into_iter()
                    .map(|later| Settled::reclassified(seq, later)),
            )
            .collect(),
            not_reclassified_through: not_reclassified,
            classification: top,
        }));
        let written = inner
            .append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))?;
        // A link error read with the end came on the channel all the run's targets share, after
        // their entries: it ends their matched answers as one an exchange saw does (design 8.11
        // "Link error").
        if let Some(kind) = error_with_end {
            for (_, state) in &mut inner.links {
                state.channel_link_error(written, kind);
            }
        }
        // A primitive the loss ended: the exchange is one the loss ended, settled as this
        // entry classifies it.
        if lost {
            inner.note_loss(seq, classification, false)?;
        }
        Some(written)
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
    /// the worker gave it. A loss an earlier run recorded is then settled against `reading`
    /// (`vci_loss_settled`, ADR-373). The event's `seq`; `None` once the log has failed.
    pub(crate) fn vci_connected(
        &self,
        version: Option<&vci_service_interface::VersionData>,
        reading: Option<ConnectReading>,
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
        // A worker that has no serial number to give leaves it zero (design 8.11
        // `vci_loss_settled`).
        inner.serial_number = version
            .map(|version| version.hw_serial_number)
            .filter(|&number| number != 0);
        let connected = inner
            .append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))?;
        // A loss an earlier run recorded is settled at once (ADR-373).
        if let Some(loss) = inner.unsettled.take() {
            inner.settle_loss(loss, reading)?;
        }
        Some(connected)
    }

    /// Whether the log holds a loss an earlier run recorded that the next `vci_connected`
    /// settles, so the runner reads the VCI's clock for it first (ADR-373).
    pub(crate) fn loss_to_settle(&self) -> bool {
        self.lock().unsettled.is_some()
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

/// What an owed exchange's reason adds when a response delivered after it ended, logged
/// `unattached`, may be its own or another exchange's.
const AMBIGUOUS_OWNER: &str =
    " A response delivered after it ended may be its own or another exchange's: undetermined.";

/// What an exchange's reason adds when a response read during it, logged `unattached`, may be
/// its own or an earlier exchange's.
const MAY_OWN_UNATTACHED: &str = " A response read during this exchange, logged unattached, may be this exchange's or an earlier one's: undetermined.";

/// What an exchange's reason adds when an indication read during it, logged `unattached`, may be
/// its own or an earlier exchange's.
const MAY_OWN_INDICATION: &str = " An indication read during this exchange, logged unattached, may be this exchange's or an earlier one's: undetermined.";

/// What an owed exchange's reason adds when an indication read after it ended, logged
/// `unattached`, may be its own or another exchange's.
const AMBIGUOUS_INDICATION: &str =
    " An indication read after it ended may be its own or another exchange's: undetermined.";

/// Whether a time on `clock`, placed at `placed`, comes before every start in `starts` on its
/// clock and epoch: an exchange whose earliest VCI times those are was not yet in flight then.
/// A time that cannot be set against them does not.
fn before_starts(
    starts: &[(exchange::Clock, clocks::Placed)],
    clock: Option<exchange::Clock>,
    placed: Option<clocks::Placed>,
) -> bool {
    placed.is_some_and(|placed| {
        starts
            .iter()
            .filter(|(own, start)| clock == Some(*own) && start.epoch == placed.epoch)
            .map(|(_, start)| start.unwrapped)
            .min()
            .is_some_and(|start| placed.unwrapped < start)
    })
}

/// The kept exchanges a late indication of `kind` with identifier `id` could belong to, and
/// whether they are every ended exchange on the link that could (design 8.11 `late_item`).
fn late_indication_owners(
    state: &exchange::LinkState,
    item: &UnattachedItem,
    kind: LateIndication,
    id: u32,
) -> (Vec<u64>, bool) {
    match kind {
        LateIndication::Transmit(_) => state.late_transmit_owners(id),
        LateIndication::Start => state.late_start_owners(id),
        LateIndication::Pending => item.pending_bytes().map_or((Vec::new(), true), |bytes| {
            state.late_pending_owners(id, bytes)
        }),
    }
}

/// Who could own `item`, read during the exchange of `evidence`, as a late response (design 8.11
/// `late_response`).
#[derive(Debug, PartialEq, Eq)]
enum LateOwner {
    /// The one owed exchange the VCI ended that could own it.
    One(u64),
    /// Two or more could (owed exchanges, or one of them and the exchange in flight, which
    /// awaits the same response): the owed ones the log keeps, and whether the exchange in
    /// flight is one of them; each is at least `undetermined`.
    Several(Vec<u64>, bool),
}

/// [`LateOwner`] of `item`: a message another node sent, not a pending, that an owed exchange
/// could own; `None` for any other item, or one no kept exchange could own.
fn late_response_owner(
    state: &exchange::LinkState,
    item: &UnattachedItem,
    evidence: &exchange::Evidence,
) -> Option<LateOwner> {
    let UnattachedData::Result {
        rx_flag,
        identifier,
        bytes,
    } = &item.data
    else {
        return None;
    };
    // A pending is a late item (`late_item`), not a late response.
    if bytes.is_empty() || !is_received(rx_flag) || item.pending_bytes().is_some() {
        return None;
    }
    let in_flight =
        identifier.is_none_or(|id| id == evidence.rx_id) && evidence.expected.matches(bytes);
    let owners = state.late_response_owners(*identifier, bytes);
    // A pending is a late item, not a late response.
    let pending = |kept: &exchange::Kept| {
        let negative = &kept.evidence.expected.negative_pattern;
        !negative.is_empty()
            && bytes.starts_with(negative)
            && bytes.get(negative.len()) == Some(&exchange::NRC_PENDING)
    };
    if let ([owner], false) = (&owners[..], in_flight) {
        let kept = state.kept(*owner)?;
        return (!pending(kept) && kept.evidence.vci_ended).then_some(LateOwner::One(*owner));
    }
    if owners.len() + usize::from(in_flight) < 2 {
        return None;
    }
    let kept: Vec<u64> = owners
        .into_iter()
        .filter(|owner| state.kept(*owner).is_some_and(|kept| !pending(kept)))
        .collect();
    (!kept.is_empty() || in_flight).then_some(LateOwner::Several(kept, in_flight))
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
                &kept.limit_pendings(),
                time,
                bytes.len(),
            )
        });
        let (classification, reason) =
            exchange::settle_late_response(&kept, &answered, window, &history);
        // The answer it adds counts for the later exchanges on the link.
        let answered_evidence = answered.clone();
        let entry = exchange::ExchangeEntry::build(owner, answered, history);
        let answer = entry.settled_answer(&kept);
        let restart = entry.late_restart();
        state.settled(owner, classification, &reason, answered_evidence, true);
        let (later, not_reclassified) = settle_later(state, owner, restart, &answer);
        let top = later
            .iter()
            .map(|later| later.classification)
            .fold(classification, exchange::Classification::max);
        let event = Event::LateResponse(LateResponseEntry {
            refers_to: std::iter::once(owner)
                .chain(later.iter().map(|later| later.seq))
                .collect(),
            response: UnattachedEntry::of(item, placed),
            unattached: None,
            exchanges: std::iter::once(Settled {
                seq: owner,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request.clone(),
                classification,
                reason,
            })
            .chain(
                later
                    .into_iter()
                    .map(|later| Settled::reclassified(owner, later)),
            )
            .collect(),
            not_reclassified_through: not_reclassified,
            text: format!(
                "The ECU's response to the exchange logged as {owner} was delivered after that exchange had ended.{}{}",
                restart_text(restart),
                not_reclassified_text(not_reclassified)
            ),
            classification: top,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| ())
    }

    /// Writes the `late_response` settling entry of `item`, a response two or more exchanges
    /// could own, logged `unattached` as line `unattached`: each owed exchange of `owners` on
    /// link `at` is at least `undetermined`, and stays owed (design 8.11 `late_response`).
    fn late_response_unowned(
        &mut self,
        at: usize,
        owners: &[u64],
        item: &UnattachedItem,
        placed: Option<clocks::Placed>,
        unattached: u64,
    ) -> Option<()> {
        let state = &mut self.links[at].1;
        let mut exchanges = Vec::new();
        for owner in owners {
            let Some(kept) = state.kept(*owner).cloned() else {
                continue;
            };
            let classification = kept
                .classification
                .max(exchange::Classification::Undetermined);
            let reason = if classification == kept.classification {
                "A response was delivered after the exchange ended that it or another exchange could own; the exchange keeps its classification, which is at least undetermined.".to_owned()
            } else {
                "A response was delivered after the exchange ended that it or another exchange could own, so whether the ECU answered this request is not decided: undetermined.".to_owned()
            };
            state.raise(
                *owner,
                exchange::Classification::Undetermined,
                &reason,
                AMBIGUOUS_OWNER,
            );
            exchanges.push(Settled {
                seq: *owner,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request,
                classification,
                reason,
            });
        }
        let Some(classification) = exchanges.iter().map(|settled| settled.classification).max()
        else {
            return Some(());
        };
        let event = Event::LateResponse(LateResponseEntry {
            refers_to: owners.to_vec(),
            response: UnattachedEntry::of(item, placed),
            unattached: Some(unattached),
            exchanges,
            not_reclassified_through: None,
            text: format!(
                "A response was delivered that two or more exchanges could own (logged unattached as {unattached}); none is chosen as its owner."
            ),
            classification,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| ())
    }

    /// Writes the `late_item` settling entry of `item`, an indication of the owed exchange
    /// `owner` on link `at`, read after its entry (design 8.11 `late_item`, ADR-370): the
    /// exchange's `sent` and answers restated with it, and its classification again. It ends
    /// no owed request: an indication is no final response.
    fn late_item(
        &mut self,
        at: usize,
        owner: u64,
        kind: LateIndication,
        item: &UnattachedItem,
        placed: Option<clocks::Placed>,
    ) -> Option<()> {
        let state = &mut self.links[at].1;
        let Some(kept) = state.kept(owner).cloned() else {
            return Some(());
        };
        let mut evidence = kept.evidence.clone();
        let mut vci = evidence.times.vci.unwrap_or_default();
        let time = exchange::VciTime::new(u64::from(item.vci_us), item.clock).at(placed);
        let mut late_start = None;
        let what = match kind {
            LateIndication::Transmit(transmit) => {
                evidence.transmit_evidence = Some(transmit);
                vci.tx_done = Some(time)
                    .into_iter()
                    .chain(vci.tx_done)
                    .reduce(exchange::VciTime::prefer);
                "Its request went out."
            }
            LateIndication::Start => {
                evidence.response_started = true;
                // The response's start when it came after the last pending and the response
                // spans frames, as for an exchange.
                let after_pending = match (kept.all_pendings().last(), time.placed()) {
                    (None, _) => true,
                    (Some(Some((clock, at))), Some((own, start))) => {
                        *clock == own && at.epoch == start.epoch && start.unwrapped >= at.unwrapped
                    }
                    _ => false,
                };
                let spans_frames = evidence
                    .end
                    .response()
                    .is_none_or(|response| response.len() > SINGLE_FRAME_MAX);
                // The latest start is the response's: the ECU aborted and began again, so it
                // replaces a start the exchange had, unless it comes before that one on its
                // clock and epoch.
                let earlier = match (vci.rx_start.and_then(|start| start.placed()), time.placed()) {
                    (Some((clock, had)), Some((own, start))) => {
                        clock == own && had.epoch == start.epoch && start.unwrapped < had.unwrapped
                    }
                    _ => false,
                };
                if after_pending && spans_frames && !earlier {
                    vci.rx_start = Some(time);
                }
                if evidence.vci_ended && matches!(evidence.end, exchange::End::NoResponse) {
                    evidence.end = exchange::End::ResponseIncomplete;
                }
                "Its response began."
            }
            LateIndication::Pending => {
                // Its answer, placed as the exchange's other pendings are, read when the host
                // read it.
                let agent_us = item
                    .read
                    .filter(|read| read.epoch == evidence.times.agent.epoch)
                    .map_or(evidence.times.agent.end_us, |read| read.monotonic_us);
                evidence.pending.push(exchange::Pending {
                    vci_us: Some(u64::from(item.vci_us)),
                    clock: item.clock,
                    agent_us,
                    epoch: placed.map(|placed| placed.epoch),
                    since_previous_ms: None,
                });
                "The ECU answered it with a pending."
            }
        };
        // The pendings the timing is judged with are the ones its entry saw: a pending that came
        // after the VCI ended the exchange is judged on its own, below, and the late-pending row
        // does not apply to it (ADR-371).
        let pendings = kept.pendings.clone();
        // The limit in force for what came after the exchange ended.
        let limit_pendings = kept.limit_pendings();
        // The timing judged again with what the item adds, as the exchange's entry would have
        // judged it: a timeout judged on no response having begun is refuted by a start, and a
        // transmit done places the response's times (design 8.11 "Settlement").
        let transmit = vci.tx_done.and_then(|time| time.placed()).or(kept.transmit);
        let mut all_pendings = kept.all_pendings();
        if kind == LateIndication::Pending {
            all_pendings.push(item.clock.zip(placed));
        }
        // The intervals, when every pending the exchange has is placed in order; on a link whose
        // limits the log does not know, its entry's pendings are not kept, and their intervals
        // stand.
        if all_pendings.len() == evidence.pending.len() {
            let pending_placed: Vec<_> = all_pendings
                .iter()
                .map(|pending| pending.map(|(_, placed)| placed))
                .collect();
            exchange::pending_intervals(&mut evidence.pending, &pending_placed, transmit);
        }
        if let Some(limits) = kept.limits {
            let response_may_have_begun =
                evidence.response_started || !limits.start_indications || kept.start_unattached;
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
            // A start is judged like a late response by its time, against the limit in force:
            // before the limit less the margin, or after the limit plus the margin (design 8.11
            // "Settlement"). The latest start is judged: one that replaces an earlier start
            // ends that one's judgement.
            if kind == LateIndication::Start
                && matches!(evidence.end, exchange::End::ResponseIncomplete)
                && let Some(start) = vci.rx_start
            {
                late_start = match timing::judge_late_start(
                    limits,
                    evidence.absorbing(),
                    transmit,
                    &limit_pendings,
                    start.placed(),
                ) {
                    timing::LateWindow::InTime { at_us } => {
                        Some(exchange::LateStart::InTime { at_us })
                    }
                    timing::LateWindow::Late { by_us } => Some(exchange::LateStart::Late {
                        by_us,
                        // A start after the timeout does not refute it: an earlier late start
                        // that recorded it keeps it, since judging the timing again with the
                        // response begun dropped the timeout.
                        at_limit: kept.evidence.timing.window == exchange::Window::TimeoutAtLimit
                            || matches!(
                                kept.late_start,
                                Some(exchange::LateStart::Late { at_limit: true, .. })
                            ),
                    }),
                    timing::LateWindow::Open => None,
                };
            }
        }
        // A forwarded pending that came after the VCI ended the exchange is judged like a late
        // response, against the limit in force before it; on a link whose VCI absorbs pendings
        // it is not judged (design 8.11 "Settlement" `late_item`, ADR-371).
        let pending_window =
            (kind == LateIndication::Pending).then(|| match (kept.limits, item.pending_bytes()) {
                (Some(limits), Some(bytes)) if !kept.evidence.absorbing() => timing::judge_late(
                    limits,
                    false,
                    transmit,
                    &limit_pendings,
                    item.clock.zip(placed),
                    bytes.len(),
                ),
                _ => timing::LateWindow::Open,
            });
        evidence.times.vci = Some(vci);
        let history = state.history_settling(&evidence, owner);
        let entry = exchange::ExchangeEntry::build(owner, evidence.clone(), history);
        let mut classification = entry.classification;
        let mut reason = entry.reason.clone();
        // The pending's judgement, with what the exchange's own rows give, as a late response's
        // (`settle_late_response`): it stands as a floor, since a later item cannot refute a time
        // the VCI observed.
        let pending_floor = pending_window.map(|window| {
            let (verdict, text) =
                exchange::settle_late_pending(&kept, window, classification, &reason);
            classification = verdict;
            reason = text;
            let late_pending = match window {
                timing::LateWindow::InTime { .. } => exchange::LatePending::InTime,
                timing::LateWindow::Late { .. } => exchange::LatePending::Late,
                timing::LateWindow::Open => exchange::LatePending::Open,
            };
            (late_pending, verdict)
        });
        let late_start = match kind {
            LateIndication::Start => Some(late_start),
            LateIndication::Transmit(_) | LateIndication::Pending => None,
        };
        // While the response does not complete, `undetermined` stands (design 8.11
        // "Settlement").
        match late_start.unwrap_or(kept.late_start) {
            Some(exchange::LateStart::InTime { at_us }) => {
                classification = classification.max(exchange::Classification::Undetermined);
                reason.push_str(&format!(
                    " The response started {:.1} ms after the transmit, within the limit in force less the margin: once it completes, the system missed a response that started in time.",
                    at_us as f64 / 1000.0
                ));
            }
            Some(exchange::LateStart::Late { by_us, .. }) => {
                classification = classification.max(exchange::Classification::Undetermined);
                reason.push_str(&format!(
                    " The response started at least {:.1} ms after the limit in force plus the margin: the ECU started late, and once the response completes it is judged as a late response.",
                    by_us as f64 / 1000.0
                ));
            }
            None => {}
        }
        // A floor an ambiguous item set stands. A `system` the exchange's own entry gave rested
        // on its timing, which is judged again here: the log keeps no exchange whose `system`
        // the agent observed otherwise (design 8.11 "Settlement"). Its own classification sets
        // no floor, so a later item can refute what it inferred.
        if let Some((floor, suffix)) = state.floor_of(owner)
            && classification < floor
        {
            classification = floor;
            reason.push_str(&suffix);
        }
        state.settled_from_evidence(owner, classification, &reason, evidence.clone());
        state.update_kept(owner, evidence, entry.sent, transmit, late_start);
        if let Some((late_pending, verdict)) = pending_floor {
            state.add_kept_late_pending(owner, item.clock.zip(placed), late_pending);
            if verdict > entry.classification
                && state
                    .floor_of(owner)
                    .is_none_or(|(floor, _)| floor < verdict)
            {
                state.raise(owner, verdict, &reason, exchange::LATE_PENDING_FLOOR);
            }
        }
        let answer = entry.settled_answer(&kept);
        let (later, not_reclassified) = settle_later(state, owner, None, &answer);
        let top = later
            .iter()
            .map(|later| later.classification)
            .fold(classification, exchange::Classification::max);
        let event = Event::LateItem(LateItemEntry {
            refers_to: std::iter::once(owner)
                .chain(later.iter().map(|later| later.seq))
                .collect(),
            item: UnattachedEntry::of(item, placed),
            unattached: None,
            sent: Some(entry.sent),
            sent_evidence: Some(entry.sent_evidence),
            outcome: Some(entry.outcome),
            exchanges: std::iter::once(Settled {
                seq: owner,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request.clone(),
                classification,
                reason,
            })
            .chain(
                later
                    .into_iter()
                    .map(|later| Settled::reclassified(owner, later)),
            )
            .collect(),
            not_reclassified_through: not_reclassified,
            text: format!(
                "An indication of the exchange logged as {owner} was read after that exchange had ended. {what}{}",
                not_reclassified_text(not_reclassified)
            ),
            classification: top,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| ())
    }

    /// Writes the `late_item` settling entry of `item`, an indication two or more exchanges
    /// could own, logged `unattached` as line `unattached`: each owed exchange of `owners` on
    /// link `at` is at least `undetermined` (design 8.11 `late_item`, ADR-370).
    /// The agent epoch and time the host last read an item of `item`'s link before `item`:
    /// another item of `reads`, read with it, or one an earlier exchange read.
    fn read_since(&self, item: &UnattachedItem, reads: &[(u32, AgentNow)]) -> (u32, Option<u64>) {
        // With no read time, the latest read of its link stands for it.
        let Some(read) = item.read else {
            return self
                .link_reads
                .iter()
                .find(|(link, _)| *link == item.link)
                .map_or((0, None), |(_, last)| (last.epoch, Some(last.monotonic_us)));
        };
        let earlier = |at: &AgentNow| at.epoch == read.epoch && at.monotonic_us < read.monotonic_us;
        let since = reads
            .iter()
            .chain(&self.link_reads)
            .filter(|(link, at)| *link == item.link && earlier(at))
            .map(|(_, at)| at.monotonic_us)
            .max();
        (read.epoch, since)
    }

    /// The host read `reads`: each link's latest read stands for the link errors read later.
    fn note_reads(&mut self, reads: &[(u32, AgentNow)]) {
        for (link, read) in reads {
            match self.link_reads.iter_mut().find(|(own, _)| own == link) {
                Some((_, last))
                    if last.epoch != read.epoch || last.monotonic_us < read.monotonic_us =>
                {
                    *last = *read;
                }
                Some(_) => {}
                None => self.link_reads.push((*link, *read)),
            }
        }
    }

    /// Writes the `late_item` of link error `item`, of `kind`, read after the exchanges it may
    /// have come during (design 8.11 `late_item`, "Link error", ADR-372). Placed at `placed` on
    /// its clock, it is within the exchange whose span holds it on each target's link state, a
    /// fault seen in it on that exchange's own link and H3 evidence on another, or between
    /// exchanges; it ends the matched answers and the answers since an error from before it,
    /// for every target on the physical channel. One that cannot be placed (`placed` `None`) is
    /// a possible link error, H3 evidence, during each exchange whose entry was written since
    /// `since`, the agent epoch and time the link's items were last read before it, and ends the
    /// matched answers from before the first of them.
    ///
    /// Gives whether it wrote a line: one that cannot be placed and that no exchange could have
    /// come during writes none.
    fn late_link_error(
        &mut self,
        item: &UnattachedItem,
        placed: Option<clocks::Placed>,
        kind: exchange::LinkErrorKind,
        since: (u32, Option<u64>),
    ) -> Option<bool> {
        let written = self.next_seq;
        let mut during: Vec<(exchange::Reclassified, bool)> = Vec::new();
        let mut later: Vec<exchange::Reclassified> = Vec::new();
        let mut not_reclassified: Option<u64> = None;
        let at = item.clock.zip(placed);
        let mut text = String::new();
        // On each target's link state, the exchanges it came during and the one whose answer it
        // came after (`None` before the window).
        // Placed when an exchange on some target's link state has a time on its clock and
        // epoch.
        let at = at.filter(|at| {
            self.links
                .iter()
                .any(|(_, state)| state.place_error(*at) != exchange::ErrorPoint::NotPlaced)
        });
        let points: Vec<(Vec<u64>, Option<u64>)> = self
            .links
            .iter()
            .map(|(_, state)| match at.map(|at| state.place_error(at)) {
                Some(exchange::ErrorPoint::Within(seq)) => (vec![seq], Some(seq)),
                Some(exchange::ErrorPoint::After(seq)) => (Vec::new(), Some(seq)),
                Some(exchange::ErrorPoint::BeforeWindow { within }) => {
                    (within.into_iter().collect(), None)
                }
                // Another target's exchanges on no shared clock: after its last one.
                Some(exchange::ErrorPoint::NotPlaced) => (Vec::new(), state.window_last()),
                None => {
                    // Before the first exchange it may have come during; with none, after the
                    // last.
                    let owners = state.written_since(since.0, since.1);
                    let after = match owners.first() {
                        Some(first) => Some(first - 1),
                        None => state.window_last(),
                    };
                    (owners, after)
                }
            })
            .collect();
        if at.is_none() && points.iter().all(|(owners, _)| owners.is_empty()) {
            return Some(false);
        }
        for ((_, state), (owners, after)) in self.links.iter_mut().zip(points) {
            for owner in owners {
                during.extend(state.error_in(owner, kind, item.link, at.is_some()));
            }
            let (changed, through) = state.error_after(after, written, kind);
            later.extend(changed);
            not_reclassified = not_reclassified.max(through);
        }
        // An exchange it came during that the counts then reclassified again is given as it
        // now stands.
        later.retain(|changed| {
            let Some((owner, owner_changed)) = during
                .iter_mut()
                .find(|(owner, _)| owner.seq == changed.seq)
            else {
                return true;
            };
            owner.classification = changed.classification;
            owner.reason.clone_from(&changed.reason);
            *owner_changed = true;
            false
        });
        let mut refers_to: Vec<u64> = during.iter().map(|(owner, _)| owner.seq).collect();
        later.sort_by_key(|changed| changed.seq);
        refers_to.extend(later.iter().map(|changed| changed.seq));
        let classification = during
            .iter()
            .filter(|(_, changed)| *changed)
            .map(|(owner, _)| owner.classification)
            .chain(later.iter().map(|changed| changed.classification))
            .max()
            .unwrap_or(exchange::Classification::None);
        let owned: Vec<String> = during
            .iter()
            .map(|(owner, _)| owner.seq.to_string())
            .collect();
        text.push_str(&match (at, owned.as_slice()) {
            (Some(_), []) => {
                "A link error was read after the exchanges before it had ended; its time falls between exchanges, so it ends the matched answers from before it.".to_owned()
            }
            (Some(_), _) => format!(
                "A link error was read after the exchange logged as {} had ended; its time falls within that exchange.",
                owned.join(", ")
            ),
            (None, _) => format!(
                "A link error was read whose time cannot be placed on one clock with the exchanges; it may have come during each exchange logged as {} since the link's items were last read.",
                owned.join(", ")
            ),
        });
        let exchanges = during
            .into_iter()
            .map(|(owner, _)| Settled {
                seq: owner.seq,
                link: owner.link,
                origin: owner.origin,
                request: owner.request,
                classification: owner.classification,
                reason: owner.reason,
            })
            .chain(later.into_iter().map(Settled::after_link_error))
            .collect();
        let event = Event::LateItem(LateItemEntry {
            refers_to,
            item: UnattachedEntry::of(item, placed),
            unattached: None,
            sent: None,
            sent_evidence: None,
            outcome: None,
            exchanges,
            not_reclassified_through: not_reclassified,
            text,
            classification,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| true)
    }

    fn late_item_unowned(
        &mut self,
        at: usize,
        owners: &[u64],
        item: &UnattachedItem,
        placed: Option<clocks::Placed>,
        unattached: u64,
    ) -> Option<()> {
        let state = &mut self.links[at].1;
        let mut exchanges = Vec::new();
        for owner in owners {
            let Some(kept) = state.kept(*owner).cloned() else {
                continue;
            };
            // One already raised for an ambiguous item, and no lower now, gains nothing: an
            // exchange ended with its send unknown would otherwise be named after every later
            // exchange.
            if kept.classification >= exchange::Classification::Undetermined
                && state
                    .floor_of(*owner)
                    .is_some_and(|(floor, _)| floor >= exchange::Classification::Undetermined)
            {
                continue;
            }
            let classification = kept
                .classification
                .max(exchange::Classification::Undetermined);
            let reason = if classification == kept.classification {
                "An indication was read after the exchange ended that it or another exchange could own; the exchange keeps its classification, which is at least undetermined.".to_owned()
            } else {
                "An indication was read after the exchange ended that it or another exchange could own, so what it shows of this exchange is not decided: undetermined.".to_owned()
            };
            state.raise(
                *owner,
                exchange::Classification::Undetermined,
                &reason,
                AMBIGUOUS_INDICATION,
            );
            exchanges.push(Settled {
                seq: *owner,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request,
                classification,
                reason,
            });
        }
        let Some(classification) = exchanges.iter().map(|settled| settled.classification).max()
        else {
            return Some(());
        };
        let event = Event::LateItem(LateItemEntry {
            refers_to: owners.to_vec(),
            item: UnattachedEntry::of(item, placed),
            unattached: Some(unattached),
            sent: None,
            sent_evidence: None,
            outcome: None,
            exchanges,
            not_reclassified_through: None,
            text: format!(
                "An indication was read that two or more exchanges could own (logged unattached as {unattached}); none is chosen as its owner."
            ),
            classification,
        });
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
            .map(|_| ())
    }

    /// Takes the exchange logged as `seq`, which the lost VCI ended (`refused` false) or which
    /// the worker refused afterwards on the offline link, into the run's loss, and writes its
    /// record again, whole, and syncs it (design 8.11 "Settlement", ADR-373). The first record
    /// keeps each clock's last time before the loss. `None` once the log has failed.
    fn note_loss(
        &mut self,
        seq: u64,
        classification: exchange::Classification,
        refused: bool,
    ) -> Option<()> {
        let mut loss = match self.loss.take() {
            Some(loss) => loss,
            None => VciLoss {
                loss: self.next_seq,
                agent_epoch: u32::try_from(self.epoch).unwrap_or(u32::MAX),
                ended: Vec::new(),
                refused: Vec::new(),
                clocks: [exchange::Clock::Device, exchange::Clock::Worker]
                    .into_iter()
                    .filter_map(|clock| {
                        Some(LastTime {
                            clock,
                            time: self.clocks.last(clock)?,
                        })
                    })
                    .collect(),
                serial_number: self.serial_number,
            },
        };
        let taken = LossExchange {
            seq,
            classification,
        };
        if refused {
            loss.refused.push(taken);
        } else {
            loss.ended.push(taken);
        }
        let written = self.append_with(|seq, line| {
            serde_json::to_writer(
                line,
                &Line {
                    seq,
                    event: &Event::VciLoss(loss.clone()),
                },
            )
        });
        self.loss = Some(loss);
        written?;
        self.sync();
        self.failed.is_none().then_some(())
    }

    /// Writes the `vci_loss_settled` of the loss an earlier run recorded, against `reading`,
    /// the first reading of the VCI's clock after it connected (design 8.11 "Settlement",
    /// ADR-373). `None` once the log has failed.
    fn settle_loss(&mut self, loss: VciLoss, reading: Option<ConnectReading>) -> Option<u64> {
        // The reading on its clock, which opens the clock's first epoch of this run.
        let placed = match reading {
            Some(reading) => self
                .place_read(reading.time.clock, reading.time.us, Some(reading.read))?
                .zip(reading.time.clock)
                .map(|(placed, clock)| LossTime {
                    clock,
                    epoch: placed.epoch,
                    us: reading.time.us,
                    read_us: reading.read.monotonic_us,
                    agent_epoch: reading.read.epoch,
                }),
            None => None,
        };
        let last = loss
            .clocks
            .iter()
            .find(|last| last.clock == exchange::Clock::Device)
            .map(|last| LossTime {
                clock: last.clock,
                epoch: last.time.epoch,
                us: last.time.us,
                read_us: last.time.read_us,
                agent_epoch: loss.agent_epoch,
            });
        // The VCI's own clock on both sides of the loss.
        let reading = placed;
        let device = reading
            .as_ref()
            .filter(|reading| reading.clock == exchange::Clock::Device);
        let readings = last.is_some() && device.is_some();
        let gap = last
            .as_ref()
            .zip(device)
            .filter(|(last, reading)| last.agent_epoch == reading.agent_epoch)
            .map(|(last, reading)| reading.read_us.saturating_sub(last.read_us));
        let (drift_ppm, drift_from) = self.margins.drift_bound();
        let (clock_margin_us, clock_margin_from) = self.margins.clock_margin();
        let fit = last
            .as_ref()
            .zip(device)
            .zip(gap)
            .map(|((last, reading), gap)| {
                clocks::loss_fit(last.us, reading.us, gap, drift_ppm, clock_margin_us)
            });
        let serial_match = loss
            .serial_number
            .zip(self.serial_number)
            .map(|(before, after)| before == after);
        let (power_kept, reason) = power_kept(readings, fit, serial_match);
        let mut exchanges: Vec<LossSettled> = loss
            .ended
            .iter()
            .chain(&loss.refused)
            .map(|exchange| match power_kept {
                PowerKept::Yes => LossSettled {
                    seq: exchange.seq,
                    classification: exchange::Classification::System,
                    reason: reason.clone(),
                },
                // What the loss leaves open keeps the exchange as it was.
                PowerKept::No | PowerKept::Unknown => LossSettled {
                    seq: exchange.seq,
                    classification: exchange.classification,
                    reason: format!("{reason} The exchange keeps its classification."),
                },
            })
            .collect();
        exchanges.sort_by_key(|exchange| exchange.seq);
        // The loss's own, or the most severe of the exchanges' (design 8.11 "Settlement").
        let classification = exchanges
            .iter()
            .map(|exchange| exchange.classification)
            .fold(
                match power_kept {
                    PowerKept::Yes => exchange::Classification::System,
                    PowerKept::No | PowerKept::Unknown => exchange::Classification::Undetermined,
                },
                exchange::Classification::max,
            );
        let text = match power_kept {
            PowerKept::Yes => {
                "The VCI reconnected and its time base carried on across the loss, so it kept its power: the PC's connection to the VCI was lost, not the vehicle's supply."
            }
            PowerKept::No => {
                "The VCI reconnected, and its time base had started again: it was powered off during the loss."
            }
            PowerKept::Unknown => {
                "The VCI reconnected; whether it kept its power across the loss is unknown."
            }
        };
        let event = Event::VciLossSettled(Box::new(VciLossSettledEntry {
            loss: loss.loss,
            refers_to: exchanges.iter().map(|exchange| exchange.seq).collect(),
            exchanges,
            clock: LossClock {
                last_before: last,
                reading,
                agent_gap_us: gap,
                expected_us: fit.map(|fit| fit.expected_us),
                tolerance_us: fit.map(|fit| fit.tolerance_us),
                drift_ppm,
                drift_from,
                clock_margin_us,
                clock_margin_from,
                fits_carried_on: fit.map(|fit| fit.carried_on),
                fits_restarted: fit.map(|fit| fit.restarted),
            },
            serial_match,
            power_kept,
            text: text.to_owned(),
            classification,
            reason,
        }));
        self.append_with(|seq, line| serde_json::to_writer(line, &Line { seq, event: &event }))
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
    /// The last `vci_loss` record, unless a `vci_loss_settled` names its loss (ADR-373).
    unsettled: Option<VciLoss>,
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
    let mut last_loss: Option<VciLoss> = None;
    let mut settled_losses: Vec<u64> = Vec::new();
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
                // A record that does not read is no loss the log can settle.
                if event == Some("vci_loss")
                    && let Some(loss) = parsed
                        .as_ref()
                        .and_then(|value| VciLoss::deserialize(value).ok())
                {
                    last_loss = Some(loss);
                }
                if event == Some("vci_loss_settled")
                    && let Some(loss) = object.and_then(|object| object.get("loss")?.as_u64())
                {
                    settled_losses.push(loss);
                }
                None
            }
        };
        if let Some(epoch) = epoch {
            largest_epoch = Some(largest_epoch.map_or(epoch, |largest| largest.max(epoch)));
        }
    }
    scan.consistent = since_opening == Some(true);
    scan.unsettled = last_loss.filter(|loss| !settled_losses.contains(&loss.loss));
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

    /// A directory of the test's own, which no earlier process used: a log found in one would
    /// be continued (`CommLog::open`), and a process ID is reused once its process has ended.
    fn dir() -> PathBuf {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        loop {
            let dir = std::env::temp_dir().join(format!(
                "ngr-comm-log-{}-{started}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("{}: {error}", dir.display()),
            }
        }
    }

    /// The test directory is new and empty, also when an earlier process left one behind
    /// under the same process ID.
    #[test]
    fn a_test_directory_is_new_and_empty() {
        let stale = std::env::temp_dir().join(format!(
            "ngr-comm-log-{}-{}",
            std::process::id(),
            NEXT_DIR.load(Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("left-behind"), b"").unwrap();
        let fresh = dir();
        assert_ne!(fresh, stale);
        assert!(std::fs::read_dir(&fresh).unwrap().next().is_none());
        std::fs::remove_dir_all(&stale).unwrap();
        std::fs::remove_dir_all(&fresh).unwrap();
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
                    link_error: None,
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
                link_error: None,
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
        log.vci_connected(None, None).unwrap();
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
        // response: `unattached`, choosing no owner, and each owed one is at least
        // `undetermined` and stays owed (design 8.11 `late_response`).
        for (exchanges, owners) in [
            (
                vec![
                    (read_f190, Vec::new()),
                    (read_f190, Vec::new()),
                    (read_f191, vec![(answer, 6_500_000)]),
                ],
                2,
            ),
            (
                vec![
                    (read_f190, Vec::new()),
                    (read_f190, vec![(answer, 2_500_000)]),
                ],
                1,
            ),
        ] {
            let lines = run(exchanges);
            let unattached = of(&lines, "unattached");
            assert_eq!(unattached.len(), 1, "{lines:?}");
            let settled = of(&lines, "late_response");
            assert_eq!(settled.len(), 1, "{lines:?}");
            let exchanges = of(&lines, "exchange");
            let owed: Vec<_> = exchanges[..owners]
                .iter()
                .map(|exchange| exchange["seq"].clone())
                .collect();
            assert_eq!(
                settled[0]["refers_to"],
                serde_json::json!(owed),
                "{}",
                settled[0]
            );
            assert_eq!(
                settled[0]["unattached"], unattached[0]["seq"],
                "{}",
                settled[0]
            );
            for exchange in settled[0]["exchanges"].as_array().unwrap() {
                assert_eq!(exchange["classification"], "undetermined", "{exchange}");
            }
            // The exchange in flight that could own it is at least `undetermined` too.
            assert_eq!(
                exchanges.last().unwrap()["classification"],
                "undetermined",
                "{lines:?}"
            );
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

    /// Design 8.11 "Link error": a link error of no primitive read during an exchange is a fault
    /// seen in it, not an `unattached` item; one whose time comes before the exchange's transmit
    /// is an earlier exchange's and stays `unattached`.
    #[test]
    fn a_link_error_of_no_primitive_read_during_an_exchange_is_its_own() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        // A read sent at 1 s, with its transmit indication then unless `transmit` is false, and
        // its response at 1.2 s, read at once; then a read of the same DID, silent and sent.
        // The link error's VCI time and when the host read it; the log's lines.
        let run = |error_us: u64, read_us: u64, transmit: bool| {
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
            let mut answered = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41]),
            );
            answered.pending_visibility = exchange::PendingVisibility::Forwarded;
            answered.times.agent.before_us = 1_000_000;
            answered.times.agent.end_us = 1_300_000;
            answered.times.vci = Some(exchange::VciTimes {
                rx_end: Some(device(1_200_000)),
                response_read_us: Some(1_200_100),
                ..exchange::VciTimes::default()
            });
            let indication = UnattachedItem {
                vci_us: 1_000_000,
                read: read(1_000_100),
                ..tx_done(0x7E0)
            };
            let error = UnattachedItem {
                link: 1,
                earlier_primitive: false,
                vci_us: u32::try_from(error_us).unwrap(),
                clock: Some(exchange::Clock::Device),
                read: read(read_us),
                data: UnattachedData::Error {
                    event: "PDU_ERR_EVT_FRAME_STRUCT".to_owned(),
                    extra_error_info: Some(9),
                    extra_error_info_name: None,
                    link_error: Some(exchange::LinkErrorKind::Frame),
                },
            };
            let items = if transmit {
                vec![indication, error]
            } else {
                vec![error]
            };
            log.exchange_with(answered, items).unwrap();
            let mut silent = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::NoResponse,
            );
            silent.pending_visibility = exchange::PendingVisibility::Forwarded;
            silent.vci_ended = true;
            silent.transmit_evidence = Some(exchange::TransmitEvidence::TxIndication);
            silent.times.agent.before_us = 4_000_000;
            silent.times.agent.end_us = 5_001_000;
            log.exchange_with(silent, Vec::new()).unwrap();
            log.end("ok");
            let lines = lines(&setup);
            std::fs::remove_dir_all(&dir).unwrap();
            lines
        };
        let exchanges = |lines: &[serde_json::Value]| -> Vec<serde_json::Value> {
            lines
                .iter()
                .filter(|line| line["event"] == "exchange")
                .cloned()
                .collect()
        };
        let unattached = |lines: &[serde_json::Value]| {
            lines
                .iter()
                .filter(|line| line["event"] == "unattached")
                .count()
        };
        // During the exchange, read after its answer: the exchange's own fault (H3), named with
        // its extra error information and its time; the answer is no answer since the error.
        let lines = run(1_100_000, 1_250_000, true);
        assert_eq!(unattached(&lines), 0, "{lines:?}");
        let [first, second] = &exchanges(&lines)[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(first["classification"], "undetermined", "{first}");
        assert_eq!(
            first["detail"]["error_event"], "PDU_ERR_EVT_FRAME_STRUCT",
            "{first}"
        );
        assert_eq!(first["detail"]["extra_error_info"], 9, "{first}");
        assert_eq!(first["times"]["vci"]["error"]["us"], 1_100_000, "{first}");
        assert_eq!(
            second["link_history"]["answered_since_error"], 0,
            "{second}"
        );
        // Read before the answer: the answer counts as one since the error.
        let lines = run(1_100_000, 1_150_000, true);
        let [_, second] = &exchanges(&lines)[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            second["link_history"]["answered_since_error"], 1,
            "{second}"
        );
        // Before its transmit: an earlier exchange's error, read late (read first, so its time
        // stays on the clock's epoch); with no exchange before it, it settles none and stays an
        // item of its own (ADR-372).
        let lines = run(900_000, 1_000_050, true);
        assert_eq!(unattached(&lines), 1, "{lines:?}");
        assert!(of_event(&lines, "late_item").is_empty(), "{lines:?}");
        assert_eq!(exchanges(&lines)[0]["classification"], "none", "{lines:?}");
        // With no transmit done or start, nothing starts the span: before the answer's end,
        // the error is still the exchange's.
        let lines = run(1_100_000, 1_250_000, false);
        assert_eq!(unattached(&lines), 0, "{lines:?}");
        assert_eq!(
            exchanges(&lines)[0]["classification"],
            "undetermined",
            "{lines:?}"
        );
    }

    /// ADR-372: a link error read after the exchanges it may have come during is a `late_item`.
    /// Within an exchange, it is that exchange's fault; between exchanges, it ends the matched
    /// answers from before it; one whose time cannot be placed is a possible link error during
    /// each exchange written since the link's items were last read. The owed requests survive
    /// it.
    #[test]
    fn a_late_link_error_settles_the_exchanges_it_may_have_come_during() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        // A read answered at 1.2 s after its transmit at 1 s.
        let mut answered = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41]),
        );
        answered.pending_visibility = exchange::PendingVisibility::Forwarded;
        answered.times.agent.before_us = 1_000_000;
        answered.times.agent.end_us = 1_300_000;
        answered.times.vci = Some(exchange::VciTimes {
            rx_end: Some(device(1_200_000)),
            response_read_us: Some(1_200_100),
            ..exchange::VciTimes::default()
        });
        let transmit = UnattachedItem {
            vci_us: 1_000_000,
            read: read(1_000_100),
            ..tx_done(0x7E0)
        };
        // The same read sent at 2 s, whose transmit indication carries no time, and which the
        // VCI ended with no response: the matched answer before it shows the address and the
        // filter.
        let mut silent = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::NoResponse,
        );
        silent.pending_visibility = exchange::PendingVisibility::Forwarded;
        silent.vci_ended = true;
        silent.times.agent.before_us = 2_000_000;
        silent.times.agent.end_us = 3_001_000;
        let untimed = UnattachedItem {
            clock: None,
            read: read(2_000_100),
            ..tx_done(0x7E0)
        };
        // A later exchange, of another read, during which the error is read; then one during
        // which the silent read's late response is.
        let later = |did: u8, at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::Positive(vec![0x62, 0xF1, did, 0x41]),
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 300_000;
            // Its transmit done, which its response carries, read after the items read during
            // it.
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(at_us)),
                rx_end: Some(device(at_us + 100_000)),
                response_read_us: Some(at_us + 200_000),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let error = |vci_us: u32, clock| UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us,
            clock,
            read: read(4_000_500),
            data: UnattachedData::Error {
                event: "PDU_ERR_EVT_FRAME_STRUCT".to_owned(),
                extra_error_info: None,
                extra_error_info_name: None,
                link_error: Some(exchange::LinkErrorKind::Frame),
            },
        };
        let response = UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us: 5_000_000,
            clock: Some(exchange::Clock::Device),
            read: read(5_000_500),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes: vec![0x62, 0xF1, 0x90, 0x42],
            },
        };
        let run = |error: Option<UnattachedItem>| {
            run_on_opened_link(vec![
                (answered.clone(), vec![transmit.clone()]),
                (silent.clone(), vec![untimed.clone()]),
                (later(0x91, 4_000_000), error.into_iter().collect()),
                (later(0x92, 5_000_000), vec![response.clone()]),
            ])
        };
        let lines = run(None);
        let exchanges = of_event(&lines, "exchange");
        let (first, second) = (&exchanges[0]["seq"], &exchanges[1]["seq"]);
        assert_ne!(exchanges[1]["classification"], "undetermined", "{lines:?}");
        // Within the answered read (on its clock's epoch, within the margin of its end): its
        // own fault, and the silent read after it has no matched answer from before the error.
        let lines = run(Some(error(1_199_500, Some(exchange::Clock::Device))));
        assert!(of_event(&lines, "unattached").is_empty(), "{lines:?}");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            late["refers_to"],
            serde_json::json!([first, second]),
            "{late}"
        );
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        assert_eq!(
            late["exchanges"][1]["classification"], "undetermined",
            "{late}"
        );
        assert_eq!(late["classification"], "undetermined", "{late}");
        // The silent read is still owed: the late response settles it.
        let [settled] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            settled["refers_to"],
            serde_json::json!([second]),
            "{settled}"
        );
        // Between them: the answered read keeps its classification, the silent read loses its
        // matched answer.
        let lines = run(Some(error(1_500_000, Some(exchange::Clock::Device))));
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["refers_to"], serde_json::json!([second]), "{late}");
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        assert_eq!(of_event(&lines, "late_response").len(), 1, "{lines:?}");
        // On no clock: the exchange during which it is read takes it, and the silent read,
        // written since the link's items were last read (its transmit indication), may have
        // seen it; the answered read, written before that read, is not named.
        let lines = run(Some(error(0, None)));
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["refers_to"], serde_json::json!([second]), "{late}");
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        let exchanges = of_event(&lines, "exchange");
        assert_eq!(exchanges[2]["classification"], "undetermined", "{lines:?}");
        assert_eq!(of_event(&lines, "late_response").len(), 1, "{lines:?}");
    }

    /// ADR-372: a link error whose time cannot be set against the exchanges may have come during
    /// each exchange written since the link's items were last read, items bound to a primitive
    /// too; a refused exchange sent nothing it could hit.
    #[test]
    fn an_unplaced_link_error_names_the_exchanges_written_since_the_last_read() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        // A read answered 50 ms after its transmit at `at_us`, the response read 10 ms later.
        let answered = |did: u8, at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::Positive(vec![0x62, 0xF1, did, 0x41]),
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 100_000;
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(at_us)),
                rx_end: Some(device(at_us + 50_000)),
                response_read_us: Some(at_us + 60_000),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let mut refused = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x93],
            exchange::End::NotSent(exchange::RefusalCause::Policy),
        );
        refused.times.agent.before_us = 3_000_000;
        refused.times.agent.end_us = 3_000_100;
        // Read during the last exchange, before its response, on no clock or on the worker's,
        // which none of the exchanges' times are on.
        let error_of = |kind, clock, read_us| UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us: 7,
            clock,
            read: read(read_us),
            data: UnattachedData::Error {
                event: "PDU_ERR_EVT_FRAME_STRUCT".to_owned(),
                extra_error_info: None,
                extra_error_info_name: None,
                link_error: Some(kind),
            },
        };
        let error = |clock, read_us| error_of(exchange::LinkErrorKind::Frame, clock, read_us);
        for clock in [None, Some(exchange::Clock::Worker)] {
            let lines = run_on_opened_link(vec![
                (answered(0x90, 1_000_000), Vec::new()),
                (answered(0x91, 2_000_000), Vec::new()),
                (refused.clone(), Vec::new()),
                (answered(0x92, 4_000_000), vec![error(clock, 4_010_000)]),
            ]);
            let exchanges = of_event(&lines, "exchange");
            let [late] = &of_event(&lines, "late_item")[..] else {
                panic!("{lines:?}");
            };
            // The second read's response and end were read after the first had ended; the
            // refused one is passed over.
            assert_eq!(
                late["refers_to"],
                serde_json::json!([exchanges[1]["seq"]]),
                "{late}"
            );
            // Its own answer stands against a possible link error on its channel.
            assert_eq!(late["exchanges"][0]["classification"], "none", "{late}");
            assert_eq!(exchanges[3]["classification"], "undetermined", "{lines:?}");
        }
        // A fault of the VCI itself is the exchange in flight's own, and only possible evidence
        // (H3) of the exchange before it: not a fault seen in that one.
        let lines = run_on_opened_link(vec![
            (answered(0x90, 1_000_000), Vec::new()),
            (
                answered(0x91, 2_000_000),
                vec![error_of(
                    exchange::LinkErrorKind::VciHardwareFault,
                    None,
                    2_010_000,
                )],
            ),
        ]);
        let exchanges = of_event(&lines, "exchange");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["exchanges"][0]["classification"], "none", "{late}");
        assert_eq!(exchanges[1]["classification"], "system", "{lines:?}");
        // The window's oldest exchange the first it may have come during: the later ones are
        // counted again, and none is named as not reclassified.
        let lines = run_on_opened_link(vec![
            (answered(0x90, 1_000_000), Vec::new()),
            (answered(0x91, 2_000_000), vec![error(None, 2_010_000)]),
        ]);
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert!(late.get("not_reclassified_through").is_none(), "{late}");
    }

    /// ADR-372: a late link error within an owed exchange that has left the window settles it
    /// from what the log keeps of it, and reclassifies none of the window's exchanges, which it
    /// names as not reclassified.
    #[test]
    fn a_late_link_error_settles_a_kept_exchange_beyond_the_window() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let positive = |did: u8, at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::Positive(vec![0x62, 0xF1, did, 0x41]),
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 100_000;
            evidence
        };
        // A matched answer, then the same read sent at 1 s, which the VCI ended at P2 (1 s)
        // with no response: `vehicle`, and owed.
        let mut silent = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::NoResponse,
        );
        silent.pending_visibility = exchange::PendingVisibility::Forwarded;
        silent.vci_ended = true;
        silent.times.agent.before_us = 1_000_000;
        silent.times.agent.end_us = 2_001_000;
        silent.times.vci = Some(exchange::VciTimes {
            rx_timeout: Some(device(2_000_000)),
            error: Some(device(2_000_000)),
            ..exchange::VciTimes::default()
        });
        let transmit = UnattachedItem {
            vci_us: 1_000_000,
            read: read(1_000_100),
            ..tx_done(0x7E0)
        };
        let mut exchanges = vec![
            (positive(0x90, 500_000), Vec::new()),
            (silent, vec![transmit]),
        ];
        // Then a window's worth of other reads, with no VCI times, and one during which an
        // error read late carries a time within the silent read.
        for at in 0..exchange::WINDOW as u64 {
            exchanges.push((positive(0x91, 3_000_000 + at * 200_000), Vec::new()));
        }
        // That one's transmit done, which its response carries, is read after the error.
        let mut last = positive(0x92, 7_000_000);
        last.times.vci = Some(exchange::VciTimes {
            tx_done: Some(device(7_000_000)),
            rx_end: Some(device(7_050_000)),
            response_read_us: Some(7_060_000),
            ..exchange::VciTimes::default()
        });
        exchanges.push((
            last,
            vec![UnattachedItem {
                link: 1,
                earlier_primitive: false,
                vci_us: 1_999_500,
                clock: Some(exchange::Clock::Device),
                read: read(7_000_500),
                data: UnattachedData::Error {
                    event: "PDU_ERR_EVT_FRAME_STRUCT".to_owned(),
                    extra_error_info: None,
                    extra_error_info_name: None,
                    link_error: Some(exchange::LinkErrorKind::Frame),
                },
            }],
        ));
        let lines = run_on_opened_link(exchanges);
        let logged = of_event(&lines, "exchange");
        assert_eq!(logged[1]["classification"], "vehicle", "{lines:?}");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            late["refers_to"],
            serde_json::json!([logged[1]["seq"]]),
            "{late}"
        );
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        assert_eq!(
            late["not_reclassified_through"],
            logged[logged.len() - 2]["seq"],
            "{late}"
        );
    }

    /// ADR-369: a late response that accepts a timed-out session change ends the evidence from
    /// before it as it would have in time. The silent read after the session change loses the
    /// matched answer from before it, in the same entry, and so does the exchange in flight.
    #[test]
    fn a_late_acceptance_of_a_session_change_reclassifies_the_exchanges_after_it() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
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
            evidence
        };
        let run = |accepting: bool| {
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
            // A matched answer for F190, then a session change the ECU does not answer in time,
            // then a silent F190 read: `vehicle` on that matched answer.
            let mut answered = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41]),
            );
            answered.pending_visibility = exchange::PendingVisibility::Forwarded;
            answered.times.agent.before_us = 10_000;
            answered.times.agent.end_us = 50_000;
            log.exchange_with(answered, vec![transmit(10_000)]).unwrap();
            log.exchange_with(silent(&[0x10, 0x03], 1_010_000), vec![transmit(1_010_000)])
                .unwrap();
            log.exchange_with(
                silent(&[0x22, 0xF1, 0x90], 3_010_000),
                vec![transmit(3_010_000)],
            )
            .unwrap();
            // The session change's answer, read during a fourth exchange: its acceptance, or a
            // negative response, which ends nothing.
            let bytes = if accepting {
                vec![0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]
            } else {
                vec![0x7F, 0x10, 0x22]
            };
            let late = UnattachedItem {
                link: 1,
                earlier_primitive: true,
                vci_us: 2_500_000,
                clock: Some(exchange::Clock::Device),
                read: read(6_010_500),
                data: UnattachedData::Result {
                    rx_flag: vec![0, 0, 0, 0],
                    identifier: Some(0x7E8),
                    bytes,
                },
            };
            log.exchange_with(
                silent(&[0x22, 0xF1, 0x90], 6_010_000),
                vec![transmit(6_010_000), late],
            )
            .unwrap();
            log.end("ok");
            let lines = lines(&setup);
            std::fs::remove_dir_all(&dir).unwrap();
            lines
        };
        let find = |lines: &[serde_json::Value], event: &str| -> Vec<serde_json::Value> {
            lines
                .iter()
                .filter(|line| line["event"] == event)
                .cloned()
                .collect()
        };
        let lines = run(true);
        let [settling] = &find(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        let exchanges = find(&lines, "exchange");
        let (change, third, fourth) = (&exchanges[1], &exchanges[2], &exchanges[3]);
        assert_eq!(third["classification"], "vehicle", "{third}");
        assert_eq!(
            settling["refers_to"],
            serde_json::json!([change["seq"], third["seq"]]),
            "{settling}"
        );
        assert_eq!(
            settling["exchanges"][1]["classification"], "undetermined",
            "{settling}"
        );
        assert!(
            settling["text"]
                .as_str()
                .is_some_and(|text| text.contains("accepts that exchange's session change")),
            "{settling}"
        );
        assert_eq!(fourth["link_history"]["answered_same_match"], 0, "{fourth}");
        assert_eq!(fourth["classification"], "undetermined", "{fourth}");
        // A negative response to it ends nothing: the exchange in flight keeps the matched
        // answer.
        let lines = run(false);
        let exchanges = find(&lines, "exchange");
        assert_eq!(
            exchanges[3]["link_history"]["answered_same_match"], 1,
            "{lines:?}"
        );
    }

    /// Design 8.11 "Settlement": a late response that ends its exchange as an owed request
    /// reclassifies a later exchange whose answer that request could own, in the same entry,
    /// and that exchange's standing classification is then the settling entry's.
    #[test]
    fn a_late_response_reclassifies_a_later_exchange_its_request_could_own() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
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
            evidence
        };
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
        // A silent read of F190, then a read of F191 answered by a negative response the
        // first, still owed, could own: `undetermined`.
        let first = log
            .exchange_with(silent(&[0x22, 0xF1, 0x90], 10_000), vec![transmit(10_000)])
            .unwrap();
        let mut negative = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x91],
            exchange::End::Negative(vec![0x7F, 0x22, 0x31]),
        );
        negative.pending_visibility = exchange::PendingVisibility::Forwarded;
        negative.times.agent.before_us = 3_010_000;
        negative.times.agent.end_us = 3_050_000;
        let second = log
            .exchange_with(negative, vec![transmit(3_010_000)])
            .unwrap();
        // The first one's response, read during a third exchange, is its alone.
        let late = UnattachedItem {
            link: 1,
            earlier_primitive: true,
            vci_us: 500_000,
            clock: Some(exchange::Clock::Device),
            read: read(6_010_500),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes: vec![0x62, 0xF1, 0x90, 0x41],
            },
        };
        log.exchange_with(
            silent(&[0x22, 0xF1, 0x92], 6_010_000),
            vec![transmit(6_010_000), late],
        )
        .unwrap();
        // The second is owed no more, now that its answer is its alone: a positive response to
        // F191, which only it of the owed requests could own, has no owner. And the first's
        // answer counts for the exchanges after it: a silent F190 read, its send confirmed, is
        // `vehicle`.
        let mut again = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x91],
            exchange::End::Positive(vec![0x62, 0xF1, 0x91, 0x41]),
        );
        again.pending_visibility = exchange::PendingVisibility::Forwarded;
        again.times.agent.before_us = 9_010_000;
        again.times.agent.end_us = 9_050_000;
        let fourth = log.exchange_with(again, vec![transmit(9_010_000)]).unwrap();
        let fifth = log
            .exchange_with(
                silent(&[0x22, 0xF1, 0x90], 12_010_000),
                vec![transmit(12_010_000)],
            )
            .unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let of = |seq: u64| {
            lines
                .iter()
                .find(|line| line["seq"] == seq)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            of(second)["classification"],
            "undetermined",
            "{}",
            of(second)
        );
        assert_eq!(
            of(fourth)["link_history"]["owed_matching"],
            0,
            "{}",
            of(fourth)
        );
        assert_eq!(
            of(fifth)["link_history"]["answered_same_match"],
            1,
            "{}",
            of(fifth)
        );
        assert_eq!(of(fifth)["classification"], "vehicle", "{}", of(fifth));
        let settled: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "late_response")
            .collect();
        assert_eq!(settled.len(), 1, "{lines:?}");
        let settled = settled[0];
        assert_eq!(
            settled["refers_to"],
            serde_json::json!([first, second]),
            "{settled}"
        );
        let later = &settled["exchanges"][1];
        assert_eq!(later["seq"], second, "{settled}");
        assert_eq!(later["request"]["sid"], "0x22", "{settled}");
        // No owed request can own its answer now: its own rows decide (requestOutOfRange
        // keeps `undetermined`, for a reason of its own).
        assert_ne!(later["reason"], of(second)["reason"], "{settled}");
        assert!(
            !later["reason"].as_str().unwrap().contains("owed"),
            "{settled}"
        );
        assert!(
            later["reason"].as_str().unwrap().starts_with(&format!(
                "Reclassified with the answer this entry adds to the exchange logged as {first}"
            )),
            "{settled}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Design 8.11 "Settlement", `primitive_ended` (ADR-366): the end of a primitive the agent
    /// stopped waiting for settles its exchange as if the agent had waited for it, except that
    /// an end within the limit in force is `system`; the agent's cancel settles nothing.
    /// Design 8.11 "Matched answer", "Link error": a pending a `primitive_ended` adds with a
    /// link error in no known order with it is no matched answer for the exchanges after it, and
    /// that error ends the matched answers from before it, as `record` applies an error after an
    /// answer.
    #[test]
    fn a_pending_ended_by_a_link_error_counts_as_no_matched_answer() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
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
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: Some(AgentNow {
                epoch: 0,
                monotonic_us: at_us + 100,
                wall_unix_ms: 0,
            }),
            ..tx_done(0x7E0)
        };
        // An answered read of F190 first: a matched answer the link error then ends.
        let mut answered = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41]),
        );
        answered.pending_visibility = exchange::PendingVisibility::Forwarded;
        answered.times.agent.before_us = 5_000;
        answered.times.agent.end_us = 8_000;
        log.exchange_with(answered, vec![transmit(5_000)]).unwrap();
        let mut waited = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x87],
            exchange::End::VciDidNotEnd,
        );
        waited.pending_visibility = exchange::PendingVisibility::Forwarded;
        waited.times.agent.before_us = 10_000;
        waited.times.agent.end_us = 9_000_000;
        let seq = log.exchange_with(waited, vec![transmit(10_000)]).unwrap();
        log.primitive_ended(
            seq,
            PrimitiveEnded {
                tx_indication: None,
                end: Some(exchange::End::LinkError(exchange::LinkErrorKind::Receive)),
                detail: exchange::Detail::default(),
                pending: vec![exchange::Pending {
                    vci_us: Some(410_000),
                    clock: Some(exchange::Clock::Device),
                    epoch: None,
                    agent_us: 0,
                    since_previous_ms: None,
                }],
                vci: exchange::VciTimes {
                    error: Some(device(910_000)),
                    ..exchange::VciTimes::default()
                },
                vci_ended: false,
                link_errors: Vec::new(),
                indications: Vec::new(),
            },
        )
        .unwrap();
        let mut silent = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::NoResponse,
        );
        silent.pending_visibility = exchange::PendingVisibility::Forwarded;
        silent.vci_ended = true;
        silent.times.agent.before_us = 9_100_000;
        silent.times.agent.end_us = 10_101_000;
        let later = log
            .exchange_with(silent, vec![transmit(9_100_000)])
            .unwrap();
        let mut same = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x87],
            exchange::End::NoResponse,
        );
        same.pending_visibility = exchange::PendingVisibility::Forwarded;
        same.vci_ended = true;
        same.times.agent.before_us = 12_100_000;
        same.times.agent.end_us = 13_101_000;
        let same = log.exchange_with(same, vec![transmit(12_100_000)]).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        for seq in [later, same] {
            let later = lines.iter().find(|line| line["seq"] == seq).unwrap();
            assert_eq!(later["link_history"]["answered_same_match"], 0, "{later}");
            assert_ne!(later["classification"], "vehicle", "{later}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            tx_indication: None,
            end: Some(exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41])),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_end: Some(device(10_500 + end_us)),
                ..exchange::VciTimes::default()
            },
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
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
        // A link error read beside the response stays in the reason.
        let mut with_error = response(500_000);
        with_error.link_errors = vec![exchange::LinkErrorKind::Receive];
        let (_, entry) = settled(with_error);
        let entry = entry.unwrap();
        assert_eq!(entry["classification"], "system", "{entry}");
        assert!(
            entry["exchanges"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("receive error"),
            "{entry}"
        );
        // A single-frame response past P2 plus the margin and its transfer: the late row.
        let (_, entry) = settled(response(1_002_321));
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "late", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A receive timeout at P2: in time, so `system`.
        let timeout = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::NoResponse),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_timeout: Some(device(10_500 + 1_000_000)),
                error: Some(device(10_500 + 1_000_000)),
                ..exchange::VciTimes::default()
            },
            vci_ended: true,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let (_, entry) = settled(timeout);
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "timeout_at_limit", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A lost VCI keeps its own row.
        let lost = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::VciLost {
                refused_after_loss: false,
            }),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let (_, entry) = settled(lost.clone());
        assert_eq!(entry.unwrap()["classification"], "undetermined");
        // Also after a receive timeout at P2: the loss ended it, so the timeout gives no
        // `system`.
        let mut lost_after_timeout = lost;
        lost_after_timeout.vci.rx_timeout = Some(device(10_500 + 1_000_000));
        let (_, entry) = settled(lost_after_timeout);
        let entry = entry.unwrap();
        assert_eq!(entry["outcome"]["result"], "vci_lost", "{entry}");
        assert_eq!(entry["timing"]["window"], "timeout_at_limit", "{entry}");
        assert_eq!(entry["classification"], "undetermined", "{entry}");
        // A response whose time cannot be judged: `system` too, since late it would be.
        let mut unclocked = response(500_000);
        unclocked.vci.rx_end = Some(exchange::VciTime::new(510_500, None));
        let (_, entry) = settled(unclocked);
        let entry = entry.unwrap();
        assert_eq!(entry["timing"]["window"], "not_judged", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        // A pending, then a link error: no owed request of its own.
        let pending_then_error = PrimitiveEnded {
            tx_indication: None,
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
            link_errors: Vec::new(),
            indications: Vec::new(),
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
        // A VCI fault reported at job end beside the end: the most severe takes it.
        let mut faulted = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::LinkError(exchange::LinkErrorKind::Receive)),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let (_, entry) = settled(faulted.clone());
        assert_eq!(entry.unwrap()["classification"], "undetermined");
        faulted.link_errors = vec![exchange::LinkErrorKind::VciHardwareFault];
        let (_, entry) = settled(faulted);
        assert_eq!(entry.unwrap()["classification"], "system");
        // The agent's cancel settles nothing: the exchange keeps its classification.
        let cancelled = PrimitiveEnded {
            tx_indication: None,
            end: None,
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let (_, entry) = settled(cancelled);
        let entry = entry.unwrap();
        assert_eq!(entry["cancelled_by_agent"], true, "{entry}");
        assert!(entry.get("outcome").is_none(), "{entry}");
        assert_eq!(entry["classification"], "undetermined", "{entry}");
    }

    /// Design 8.11 `primitive_ended`: a transmit or start-of-message indication of no primitive
    /// read at job end is the exchange's, as during it, when no other exchange could own it.
    #[test]
    fn indications_read_at_job_end_settle_with_the_primitive_s_end() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
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
        // No transmit indication during the exchange: `sent` `unknown`.
        let mut waited = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::VciDidNotEnd,
        );
        waited.pending_visibility = exchange::PendingVisibility::Forwarded;
        waited.times.agent.before_us = 10_000;
        waited.times.agent.end_us = 9_000_000;
        let seq = log.exchange_with(waited, Vec::new()).unwrap();
        let transmit = UnattachedItem {
            vci_us: 10_500,
            ..tx_done(0x7E0)
        };
        let start = UnattachedItem {
            vci_us: 600_000,
            ..start_of_message(0x7E8)
        };
        let ended = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::NoResponse),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_timeout: Some(device(1_700_000)),
                error: Some(device(1_700_000)),
                ..exchange::VciTimes::default()
            },
            vci_ended: true,
            link_errors: Vec::new(),
            indications: vec![transmit, start],
        };
        log.primitive_ended(seq, ended).unwrap();
        log.end("ok");
        let lines = lines(&setup);
        let exchange = lines
            .iter()
            .find(|line| line["event"] == "exchange")
            .unwrap();
        assert_eq!(exchange["sent"], "unknown", "{exchange}");
        let entry = lines
            .iter()
            .find(|line| line["event"] == "primitive_ended")
            .unwrap();
        assert_eq!(entry["sent"], "confirmed", "{entry}");
        assert_eq!(entry["sent_evidence"], "tx_indication", "{entry}");
        assert_eq!(entry["outcome"]["result"], "response_incomplete", "{entry}");
        // The start's own time is the response's start.
        assert_eq!(
            entry["timing"]["measured_by"], "vci_tx_done_to_rx_start",
            "{entry}"
        );
    }

    /// Design 8.11 "Settlement": the `system` a `vci_did_not_end` entry took from the VCI's
    /// clock past its own limits rested on no response under way; a start read at job end from
    /// before that reading refutes it, one after it does not.
    #[test]
    fn a_start_before_the_deadline_reading_refutes_the_inferred_system() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let settled = |indications: Vec<UnattachedItem>, pending: Vec<exchange::Pending>| {
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
            // Sent at 10.5 ms; the VCI's clock read 5 s later, past its own limits.
            let mut waited = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::VciDidNotEnd,
            );
            waited.pending_visibility = exchange::PendingVisibility::Forwarded;
            waited.times.agent.before_us = 10_000;
            waited.times.agent.end_us = 5_100_000;
            waited.times.vci = Some(exchange::VciTimes {
                deadline: Some(exchange::ClockReading {
                    time: device(5_010_500),
                    round_trip_us: 0,
                }),
                ..exchange::VciTimes::default()
            });
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
            let ended = PrimitiveEnded {
                tx_indication: None,
                end: Some(exchange::End::LinkError(exchange::LinkErrorKind::Receive)),
                detail: exchange::Detail::default(),
                pending,
                vci: exchange::VciTimes {
                    error: Some(device(5_500_000)),
                    ..exchange::VciTimes::default()
                },
                vci_ended: false,
                link_errors: Vec::new(),
                indications,
            };
            log.primitive_ended(seq, ended).unwrap();
            log.end("ok");
            let lines = lines(&setup);
            let exchange = lines
                .iter()
                .find(|line| line["event"] == "exchange")
                .unwrap()
                .clone();
            assert_eq!(exchange["classification"], "system", "{exchange}");
            lines
                .iter()
                .find(|line| line["event"] == "primitive_ended")
                .unwrap()
                .clone()
        };
        let start = |vci_us| UnattachedItem {
            vci_us,
            ..start_of_message(0x7E8)
        };
        let pending = |vci_us| exchange::Pending {
            vci_us: Some(vci_us),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 0,
            since_previous_ms: None,
        };
        let refuted = settled(vec![start(1_000_000)], Vec::new());
        assert_eq!(refuted["classification"], "undetermined", "{refuted}");
        let after = settled(vec![start(5_200_000)], Vec::new());
        assert_eq!(after["classification"], "system", "{after}");
        // Every start that may be the response's refutes it, not only the latest: an earlier
        // one a later start replaces, and one that names no identifier.
        let replaced = settled(vec![start(1_000_000), start(5_200_000)], Vec::new());
        assert_eq!(replaced["classification"], "undetermined", "{replaced}");
        let mut unnamed = start(1_000_000);
        if let UnattachedData::Result { identifier, .. } = &mut unnamed.data {
            *identifier = None;
        }
        let unnamed = settled(vec![unnamed], Vec::new());
        assert_eq!(unnamed["classification"], "undetermined", "{unnamed}");
        // A pending within P2 from before the reading moves the limit past it (P2* from
        // 0.5 s): no longer past, so the end takes its own row. One after the reading changes
        // nothing.
        let refuted = settled(Vec::new(), vec![pending(500_000)]);
        assert_eq!(refuted["classification"], "undetermined", "{refuted}");
        let after = settled(Vec::new(), vec![pending(5_200_000)]);
        assert_eq!(after["classification"], "system", "{after}");
    }

    /// Design 8.11 `primitive_ended` (ADR-366 item 3): what the host reads at job end completes
    /// what the exchange had read, as the exchange's own evidence would have: the start it had
    /// attached, a start after a pending read at job end, the transmit done the response
    /// carries, and a start another exchange could own, which still means the response may
    /// have begun.
    #[test]
    fn job_end_evidence_completes_what_the_exchange_read() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        // `owed`: an earlier exchange still owed its response; `transmit`: the request's
        // transmit indication at 10.5 ms, read during the exchange, with `during`.
        let settled =
            |owed: bool, transmit: bool, during: Vec<UnattachedItem>, ended: PrimitiveEnded| {
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
                if owed {
                    log.exchange_with(vci_ended(exchange::End::NoResponse), vec![tx_done(0x7E0)])
                        .unwrap();
                }
                let mut waited = evidence(
                    endpoint(0x7E0),
                    &[0x22, 0xF1, 0x90],
                    exchange::End::VciDidNotEnd,
                );
                waited.pending_visibility = exchange::PendingVisibility::Forwarded;
                waited.times.agent.before_us = 10_000;
                waited.times.agent.end_us = 9_000_000;
                let mut items = Vec::new();
                if transmit {
                    items.push(UnattachedItem {
                        vci_us: 10_500,
                        read: Some(AgentNow {
                            epoch: 0,
                            monotonic_us: 10_600,
                            wall_unix_ms: 0,
                        }),
                        ..tx_done(0x7E0)
                    });
                }
                items.extend(during);
                let seq = log.exchange_with(waited, items).unwrap();
                log.primitive_ended(seq, ended).unwrap();
                log.end("ok");
                lines(&setup)
                    .iter()
                    .find(|line| line["event"] == "primitive_ended")
                    .unwrap()
                    .clone()
            };
        let start = |vci_us| UnattachedItem {
            vci_us,
            ..start_of_message(0x7E8)
        };
        let timeout = |vci_us, indications| PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::NoResponse),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                rx_timeout: Some(device(vci_us)),
                error: Some(device(vci_us)),
                ..exchange::VciTimes::default()
            },
            vci_ended: true,
            link_errors: Vec::new(),
            indications,
        };
        // The start the exchange had attached is the response's start.
        let entry = settled(
            false,
            true,
            vec![start(600_000)],
            timeout(1_700_000, Vec::new()),
        );
        assert_eq!(entry["outcome"]["result"], "response_incomplete", "{entry}");
        assert_eq!(
            entry["timing"]["measured_by"], "vci_tx_done_to_rx_start",
            "{entry}"
        );
        // A pending read at job end, then a start after it: the start is the response's.
        let mut after_pending = timeout(1_700_000, vec![start(600_000)]);
        after_pending.pending = vec![exchange::Pending {
            vci_us: Some(300_000),
            clock: Some(exchange::Clock::Device),
            epoch: None,
            agent_us: 0,
            since_previous_ms: None,
        }];
        let entry = settled(false, true, Vec::new(), after_pending);
        assert_eq!(
            entry["timing"]["measured_by"], "vci_tx_done_to_rx_start",
            "{entry}"
        );
        // The transmit done the response's item carries confirms the send.
        let response = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41])),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                tx_done: Some(device(10_500)),
                rx_end: Some(device(510_500)),
                ..exchange::VciTimes::default()
            },
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let entry = settled(false, false, Vec::new(), response);
        assert_eq!(entry["sent"], "confirmed", "{entry}");
        assert_eq!(entry["sent_evidence"], "tx_done_time", "{entry}");
        // A start an owed request could own stays unattached, but the response may have
        // begun: a receive timeout at P2 is not one at the limit.
        let entry = settled(
            true,
            true,
            Vec::new(),
            timeout(10_500 + 1_000_000, vec![start(600_000)]),
        );
        assert_eq!(entry["outcome"]["result"], "no_response", "{entry}");
        assert_ne!(entry["timing"]["window"], "timeout_at_limit", "{entry}");
        assert_ne!(entry["classification"], "system", "{entry}");
        // So does one read during the exchange, which the exchange kept unattached.
        let entry = settled(
            true,
            true,
            vec![start(600_000)],
            timeout(10_500 + 1_000_000, Vec::new()),
        );
        assert_ne!(entry["timing"]["window"], "timeout_at_limit", "{entry}");
        assert_ne!(entry["classification"], "system", "{entry}");
        // The transmit done the response carries is the transmit, over the indication the
        // exchange had attached: 994.5 ms from it is within P2, 1004.5 ms from the indication
        // would not be.
        let response = PrimitiveEnded {
            tx_indication: None,
            end: Some(exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41])),
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes {
                tx_done: Some(device(20_500)),
                rx_end: Some(device(1_015_000)),
                ..exchange::VciTimes::default()
            },
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
        };
        let entry = settled(false, true, Vec::new(), response);
        assert_eq!(entry["timing"]["window"], "within_p2", "{entry}");
    }

    /// No `primitive_ended` entry for an exchange the log keeps nothing of.
    #[test]
    fn a_primitive_end_of_no_kept_exchange_writes_nothing() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&facts(), RunKind::First);
        let ended = PrimitiveEnded {
            tx_indication: None,
            end: None,
            detail: exchange::Detail::default(),
            pending: Vec::new(),
            vci: exchange::VciTimes::default(),
            vci_ended: false,
            link_errors: Vec::new(),
            indications: Vec::new(),
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
        // for the same response identifier could be its late one, so it is unattached, this
        // exchange's send stays unknown, and each is at least undetermined (ADR-370).
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
                "late_item",
                "exchange"
            ]
        );
        let late = lines
            .iter()
            .find(|line| line["event"] == "late_item")
            .unwrap();
        let ambiguous = lines
            .iter()
            .filter(|line| line["event"] == "unattached")
            .nth(3)
            .unwrap();
        assert_eq!(late["unattached"], ambiguous["seq"], "{late}");
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        let entries: Vec<_> = lines
            .iter()
            .filter(|line| line["event"] == "exchange")
            .collect();
        assert_eq!(entries[1]["classification"], "undetermined");
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

    /// Runs `exchanges` on one log with link 1 opened (its limits and indications recorded),
    /// each with the unbound items read during it, and gives every line written.
    fn run_on_opened_link(
        exchanges: Vec<(exchange::Evidence, Vec<UnattachedItem>)>,
    ) -> Vec<serde_json::Value> {
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
        for (evidence, unbound) in exchanges {
            log.exchange_with(evidence, unbound).unwrap();
        }
        log.end("ok");
        let lines = lines(&setup);
        assert_consecutive(&lines);
        std::fs::remove_dir_all(&dir).unwrap();
        lines
    }

    fn of_event(lines: &[serde_json::Value], event: &str) -> Vec<serde_json::Value> {
        lines
            .iter()
            .filter(|line| line["event"] == event)
            .cloned()
            .collect()
    }

    /// ADR-370: an unbound transmit indication or loopback echo read during a later exchange,
    /// whose VCI time comes before that exchange's own transmit done, is the late one of the one
    /// ended exchange whose send is still unknown: a `late_item` confirms that exchange's send.
    /// At or after the later exchange's transmit done, or with no time to set against it, or with
    /// two ended exchanges that could own it, it is `unattached` and each possible owner is at
    /// least `undetermined`.
    #[test]
    fn a_late_transmit_indication_confirms_the_send_of_its_ended_exchange() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        // A read the VCI ended with no response and no transmit seen: its send is unknown.
        let unknown = |did: u8| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::NoResponse,
            );
            evidence.vci_ended = true;
            evidence
        };
        // A later read whose response carries its own transmit done at `tx_us`.
        let later = |tx_us: Option<u64>| {
            let mut evidence = unknown(0x92);
            evidence.times.vci = tx_us.map(|us| exchange::VciTimes {
                tx_done: Some(device(us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let indication = |flags: u8, vci_us: u32| UnattachedItem {
            vci_us,
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, flags],
                identifier: Some(0x7E0),
                bytes: vec![0x22, 0xF1, 0x90],
            },
            ..tx_done(0x7E0)
        };
        for (flags, sent_evidence) in [(0x09, "tx_indication"), (0x01, "loopback")] {
            let lines = run_on_opened_link(vec![
                (unknown(0x90), Vec::new()),
                (later(Some(3_000_000)), vec![indication(flags, 1_000_000)]),
            ]);
            assert!(of_event(&lines, "unattached").is_empty(), "{lines:?}");
            let exchanges = of_event(&lines, "exchange");
            assert_eq!(exchanges[0]["sent"], "unknown", "{lines:?}");
            let [late] = &of_event(&lines, "late_item")[..] else {
                panic!("{lines:?}");
            };
            assert_eq!(late["refers_to"], serde_json::json!([exchanges[0]["seq"]]));
            assert_eq!(late["sent"], "confirmed", "{late}");
            assert_eq!(late["sent_evidence"], sent_evidence, "{late}");
            assert_eq!(late["exchanges"][0]["seq"], exchanges[0]["seq"], "{late}");
            // Written before the later exchange's entry, whose send its own transmit done shows.
            assert!(
                late["seq"].as_u64() < exchanges[1]["seq"].as_u64(),
                "{lines:?}"
            );
            assert_eq!(exchanges[1]["sent_evidence"], "tx_done_time", "{lines:?}");
        }
        // At or after the later exchange's transmit done, or with no time of its own to set
        // against: either exchange could own it.
        for tx_us in [Some(500_000), None] {
            let lines = run_on_opened_link(vec![
                (unknown(0x90), Vec::new()),
                (later(tx_us), vec![indication(0x09, 1_000_000)]),
            ]);
            let unattached = of_event(&lines, "unattached");
            let [late] = &of_event(&lines, "late_item")[..] else {
                panic!("{lines:?}");
            };
            assert_eq!(late["unattached"], unattached[0]["seq"], "{late}");
            assert!(late.get("sent").is_none(), "{late}");
            assert_eq!(late["exchanges"][0]["classification"], "undetermined");
            let exchanges = of_event(&lines, "exchange");
            assert_eq!(exchanges[1]["classification"], "undetermined", "{lines:?}");
        }
        // Two ended exchanges whose send is unknown: neither is chosen.
        let lines = run_on_opened_link(vec![
            (unknown(0x90), Vec::new()),
            (unknown(0x91), Vec::new()),
            (later(Some(3_000_000)), vec![indication(0x09, 1_000_000)]),
        ]);
        let exchanges = of_event(&lines, "exchange");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            late["refers_to"],
            serde_json::json!([exchanges[0]["seq"], exchanges[1]["seq"]]),
            "{late}"
        );
        assert_eq!(of_event(&lines, "unattached").len(), 1, "{lines:?}");
    }

    /// ADR-370: an unbound start-of-message indication read during a later exchange, before
    /// that exchange's own transmit done, starts the response of the one owed exchange that
    /// awaits a response on its identifier. That exchange ended with no response, so it is
    /// `response_incomplete`; the start came within the limit less the margin, so once a late
    /// response completes it, the system missed a response that started in time.
    #[test]
    fn a_late_start_of_message_starts_the_response_of_its_ended_exchange() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
        let silent = |did: u8, at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::NoResponse,
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 1_001_000;
            evidence
        };
        // A later exchange whose response carries its own transmit done.
        let own_transmit = |did: u8, at_us: u64| {
            let mut evidence = silent(did, at_us);
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(at_us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let start = UnattachedItem {
            vci_us: 20_000,
            read: read(3_010_500),
            ..start_of_message(0x7E8)
        };
        // The response that completes it spans frames, as a response with a start of its own
        // does.
        let response = |bytes: Vec<u8>| UnattachedItem {
            link: 1,
            earlier_primitive: true,
            vci_us: 6_000_000,
            clock: Some(exchange::Clock::Device),
            read: read(6_010_500),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes,
            },
        };
        let long = [0x62, 0xF1, 0x90]
            .into_iter()
            .chain([0x41; 9])
            .collect::<Vec<u8>>();
        let run_with = |starts: Vec<UnattachedItem>, bytes: Vec<u8>| {
            run_on_opened_link(vec![
                (silent(0x90, 10_000), vec![transmit(10_000)]),
                (own_transmit(0x91, 3_010_000), starts),
                (own_transmit(0x92, 6_010_000), vec![response(bytes)]),
            ])
        };
        let run = |bytes: Vec<u8>| run_with(vec![start.clone()], bytes);
        // The ECU aborted a transfer that began in time and began again after the limit: the
        // latest start is the response's, so the completed response did not start in time.
        let again = UnattachedItem {
            vci_us: 1_500_000,
            read: read(3_010_600),
            ..start_of_message(0x7E8)
        };
        let lines = run_with(vec![start.clone(), again], long.clone());
        let completed = of_event(&lines, "late_response");
        assert_ne!(
            completed[0]["exchanges"][0]["classification"], "system",
            "{lines:?}"
        );
        let lines = run(long);
        let exchanges = of_event(&lines, "exchange");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert!(of_event(&lines, "unattached").is_empty(), "{lines:?}");
        assert_eq!(late["refers_to"][0], exchanges[0]["seq"], "{late}");
        assert_eq!(late["outcome"]["result"], "response_incomplete", "{late}");
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        assert!(
            late["exchanges"][0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("started 10.0 ms after the transmit")),
            "{late}"
        );
        let [completed] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            completed["exchanges"][0]["classification"], "system",
            "{completed}"
        );
        assert!(
            completed["exchanges"][0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("started in time")),
            "{completed}"
        );
        // A response that fits one frame has no start of its own: the earlier start was a
        // transfer the ECU aborted, so it does not make the response one that started in time.
        let lines = run(vec![0x62, 0xF1, 0x90, 0x41]);
        let [completed] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert_ne!(
            completed["exchanges"][0]["classification"], "system",
            "{completed}"
        );
    }

    /// Design 8.11 "Settlement" `late_item`: a late start-of-message indication after the limit
    /// plus the margin of an exchange whose VCI ended its wait at the limit records that the ECU
    /// started late; the exchange is `undetermined` while the response does not complete, and
    /// `vehicle` once a late response completes it.
    #[test]
    fn a_late_start_after_the_limit_makes_the_completed_response_vehicle() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
        let silent = |did: u8, at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, did],
                exchange::End::NoResponse,
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 1_001_000;
            evidence
        };
        // The VCI ended its wait at P2 (1 s) after the transmit.
        let mut timed_out = silent(0x90, 10_000);
        timed_out.times.vci = Some(exchange::VciTimes {
            rx_timeout: Some(device(1_010_000)),
            error: Some(device(1_010_000)),
            ..exchange::VciTimes::default()
        });
        let own_transmit = |did: u8, at_us: u64| {
            let mut evidence = silent(did, at_us);
            evidence.times.vci = Some(exchange::VciTimes {
                tx_done: Some(device(at_us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let start = UnattachedItem {
            vci_us: 1_500_000,
            read: read(3_010_500),
            ..start_of_message(0x7E8)
        };
        let response = UnattachedItem {
            link: 1,
            earlier_primitive: true,
            vci_us: 6_000_000,
            clock: Some(exchange::Clock::Device),
            read: read(6_010_500),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes: [0x62, 0xF1, 0x90].into_iter().chain([0x41; 9]).collect(),
            },
        };
        let run = |starts: Vec<UnattachedItem>, response: UnattachedItem| {
            run_on_opened_link(vec![
                (timed_out.clone(), vec![transmit(10_000)]),
                (own_transmit(0x91, 3_010_000), starts),
                (own_transmit(0x92, 6_010_000), vec![response]),
            ])
        };
        let with_bytes = |bytes: Vec<u8>| UnattachedItem {
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes,
            },
            ..response.clone()
        };
        // The ECU began again, later still: the VCI ended its wait at the limit all the same.
        // And a response of seven bytes, which may fit one frame, completing a late start, is
        // late as well.
        let again = UnattachedItem {
            vci_us: 1_700_000,
            read: read(3_010_600),
            ..start_of_message(0x7E8)
        };
        for (starts, response) in [
            (vec![start.clone(), again], response.clone()),
            (
                vec![start.clone()],
                with_bytes(vec![0x62, 0xF1, 0x90, 0x41, 0x42, 0x43, 0x44]),
            ),
        ] {
            let lines = run(starts, response);
            let [completed] = &of_event(&lines, "late_response")[..] else {
                panic!("{lines:?}");
            };
            assert_eq!(
                completed["exchanges"][0]["classification"], "vehicle",
                "{completed}"
            );
        }
        let lines = run(vec![start], response);
        let exchanges = of_event(&lines, "exchange");
        assert_eq!(
            exchanges[0]["timing"]["window"], "timeout_at_limit",
            "{lines:?}"
        );
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["outcome"]["result"], "response_incomplete", "{late}");
        assert_eq!(
            late["exchanges"][0]["classification"], "undetermined",
            "{late}"
        );
        assert!(
            late["exchanges"][0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("the ECU started late")),
            "{late}"
        );
        let [completed] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            completed["exchanges"][0]["classification"], "vehicle",
            "{completed}"
        );
    }

    /// ADR-371: a forwarded pending bound to no primitive, read during a later exchange before
    /// that exchange's own transmit done, is the late answer of the one owed exchange whose
    /// negative pattern it carries. It is judged against that exchange's limit in force before
    /// it as a late response is: within the limit less the margin `system`, after the limit
    /// plus the margin `vehicle` when the VCI ended its wait at the limit, otherwise
    /// `undetermined`, and on a link whose VCI absorbs pendings it is not judged. The exchange
    /// stays owed, and a pending in time extends its limit for a later late response.
    #[test]
    fn a_late_forwarded_pending_settles_its_ended_exchange() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
        let silent = |request: &[u8], at_us: u64, visibility| {
            let mut evidence = evidence(endpoint(0x7E0), request, exchange::End::NoResponse);
            evidence.pending_visibility = visibility;
            evidence.vci_ended = true;
            evidence.times.agent.before_us = at_us;
            evidence.times.agent.end_us = at_us + 1_001_000;
            evidence
        };
        let forwarded = exchange::PendingVisibility::Forwarded;
        // A later exchange of another service, whose response carries its own transmit done.
        let routine = |tx: Option<u64>| {
            let mut evidence = silent(&[0x31, 0x01, 0x02, 0x03], 3_010_000, forwarded);
            evidence.times.vci = tx.map(|us| exchange::VciTimes {
                tx_done: Some(device(us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let received = |bytes: Vec<u8>, vci_us: u32, read_us| UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us,
            clock: Some(exchange::Clock::Device),
            read: read(read_us),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes,
            },
        };
        // The VCI ended its wait at `at_us`.
        let timed_out = |mut evidence: exchange::Evidence, at_us: u64| {
            evidence.times.vci = Some(exchange::VciTimes {
                rx_timeout: Some(device(at_us)),
                error: Some(device(at_us)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let pending = |vci_us| received(vec![0x7F, 0x22, 0x78], vci_us, 3_010_500);
        let run_with =
            |first: exchange::Evidence,
             vci_us,
             more: Vec<(exchange::Evidence, Vec<UnattachedItem>)>| {
                let mut exchanges = vec![
                    (first, vec![transmit(10_000)]),
                    (routine(Some(3_010_000)), vec![pending(vci_us)]),
                ];
                exchanges.extend(more);
                run_on_opened_link(exchanges)
            };
        let run = |visibility, vci_us, more| {
            run_with(
                silent(&[0x22, 0xF1, 0x90], 10_000, visibility),
                vci_us,
                more,
            )
        };
        // At P2 (1 s) after the transmit.
        let at_limit = timed_out(silent(&[0x22, 0xF1, 0x90], 10_000, forwarded), 1_010_000);
        // A late pending is `vehicle` only when the VCI is shown to have ended its wait at the
        // limit, as for a late response.
        for (first, vci_us, expected) in [
            (
                silent(&[0x22, 0xF1, 0x90], 10_000, forwarded),
                500_000,
                "system",
            ),
            (at_limit.clone(), 1_500_000, "vehicle"),
            (
                silent(&[0x22, 0xF1, 0x90], 10_000, forwarded),
                1_500_000,
                "undetermined",
            ),
            (
                silent(
                    &[0x22, 0xF1, 0x90],
                    10_000,
                    exchange::PendingVisibility::AbsorbedByVci,
                ),
                500_000,
                "undetermined",
            ),
        ] {
            let lines = run_with(first, vci_us, Vec::new());
            assert!(of_event(&lines, "unattached").is_empty(), "{lines:?}");
            let exchanges = of_event(&lines, "exchange");
            let [late] = &of_event(&lines, "late_item")[..] else {
                panic!("{lines:?}");
            };
            assert_eq!(late["refers_to"][0], exchanges[0]["seq"], "{late}");
            assert_eq!(late["exchanges"][0]["classification"], expected, "{late}");
        }
        // A `system` the exchange's own timing gave stands: the VCI ended its wait well before
        // P2, and the late pending does not lower it.
        let lines = run_with(
            timed_out(silent(&[0x22, 0xF1, 0x90], 10_000, forwarded), 510_000),
            1_500_000,
            Vec::new(),
        );
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["exchanges"][0]["classification"], "system", "{late}");
        // A late pending carries no limit on: a response after it, past P2 from the transmit,
        // is late, and the VCI ended its wait at the limit.
        let response = received(vec![0x62, 0xF1, 0x90, 0x41], 3_500_000, 6_010_500);
        let lines = run_with(
            at_limit,
            1_500_000,
            vec![(
                silent(&[0x31, 0x01, 0x02, 0x04], 6_010_000, forwarded),
                vec![response.clone()],
            )],
        );
        let [completed] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            completed["exchanges"][0]["classification"], "vehicle",
            "{completed}"
        );
        // A pending in time extends the limit: a response 3 s after it is within P2*, so the
        // system missed a response that came in time.
        let lines = run(
            forwarded,
            500_000,
            vec![(
                silent(&[0x31, 0x01, 0x02, 0x04], 6_010_000, forwarded),
                vec![response],
            )],
        );
        let [completed] = &of_event(&lines, "late_response")[..] else {
            panic!("{lines:?}");
        };
        assert!(
            completed["exchanges"][0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("within the limit in force less the margin")),
            "{completed}"
        );
        // Two owed reads it could belong to: neither is chosen, each is at least undetermined.
        let lines = run_on_opened_link(vec![
            (
                silent(&[0x22, 0xF1, 0x90], 10_000, forwarded),
                vec![transmit(10_000)],
            ),
            (
                silent(&[0x22, 0xF1, 0x91], 1_510_000, forwarded),
                vec![transmit(1_510_000)],
            ),
            (routine(Some(3_010_000)), vec![pending(2_000_000)]),
        ]);
        assert_eq!(of_event(&lines, "unattached").len(), 1, "{lines:?}");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(
            late["refers_to"].as_array().map(Vec::len),
            Some(2),
            "{late}"
        );
        // An exchange in flight that awaits the same pattern and has no transmit done of its
        // own could own it too.
        let lines = run_on_opened_link(vec![
            (
                silent(&[0x22, 0xF1, 0x90], 10_000, forwarded),
                vec![transmit(10_000)],
            ),
            (
                silent(&[0x22, 0xF1, 0x92], 3_010_000, forwarded),
                vec![pending(500_000)],
            ),
        ]);
        assert_eq!(of_event(&lines, "unattached").len(), 1, "{lines:?}");
        let exchanges = of_event(&lines, "exchange");
        assert_eq!(exchanges[1]["classification"], "undetermined", "{lines:?}");
    }

    /// ADR-371: what a late pending gave its exchange stands when a later settling entry
    /// classifies that exchange again from its history: the pending is not judged by the
    /// late-pending row, and an undetermined judgement stays.
    #[test]
    fn a_late_pending_s_judgement_stands_when_its_exchange_is_reclassified() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |monotonic_us| {
            Some(AgentNow {
                epoch: 0,
                monotonic_us,
                wall_unix_ms: 0,
            })
        };
        let transmit = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            read: read(at_us + 100),
            ..tx_done(0x7E0)
        };
        // An exchange the VCI ended at P2 (1 s) after its transmit at `at_us`.
        let timed_out = |request: &[u8], at_us: u64| {
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
            evidence
        };
        // One whose response carries its own transmit done, read after the items read during
        // it.
        let in_flight = |request: &[u8], at_us: u64| {
            let mut evidence = timed_out(request, at_us);
            if let Some(vci) = evidence.times.vci.as_mut() {
                vci.tx_done = Some(device(at_us));
            }
            evidence
        };
        let received = |bytes: Vec<u8>, vci_us: u32, read_us| UnattachedItem {
            link: 1,
            earlier_primitive: false,
            vci_us,
            clock: Some(exchange::Clock::Device),
            read: read(read_us),
            data: UnattachedData::Result {
                rx_flag: vec![0, 0, 0, 0],
                identifier: Some(0x7E8),
                bytes,
            },
        };
        let routine = [0x31, 0x01, 0x02, 0x03];
        // A read and a routine both time out; a pending for the routine comes 1.5 s after its
        // transmit (past P2 plus the margin) or at P2 (between), and then the read's late
        // response is delivered, which answers an owed request before the routine.
        for (pending_us, expected) in [(3_010_000, "vehicle"), (2_510_000, "undetermined")] {
            let lines = run_on_opened_link(vec![
                (
                    timed_out(&[0x22, 0xF1, 0x90], 10_000),
                    vec![transmit(10_000)],
                ),
                (timed_out(&routine, 1_510_000), vec![transmit(1_510_000)]),
                (
                    in_flight(&[0x22, 0xF1, 0x92], 3_510_000),
                    vec![received(vec![0x7F, 0x31, 0x78], pending_us, 3_510_500)],
                ),
                (
                    in_flight(&[0x22, 0xF1, 0x93], 6_010_000),
                    vec![received(vec![0x62, 0xF1, 0x90, 0x41], 5_000_000, 6_010_500)],
                ),
            ]);
            let exchanges = of_event(&lines, "exchange");
            let routine_seq = &exchanges[1]["seq"];
            let [late] = &of_event(&lines, "late_item")[..] else {
                panic!("{lines:?}");
            };
            assert_eq!(&late["refers_to"][0], routine_seq, "{late}");
            assert_eq!(late["exchanges"][0]["classification"], expected, "{late}");
            let [completed] = &of_event(&lines, "late_response")[..] else {
                panic!("{lines:?}");
            };
            // The routine is classified again, with one more answered exchange before it.
            let again: Vec<_> = completed["exchanges"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|settled| &settled["seq"] == routine_seq)
                .collect();
            assert_eq!(again.len(), 1, "{completed}");
            assert_eq!(again[0]["classification"], expected, "{completed}");
        }
    }

    /// ADR-370: a late transmit indication places the ended exchange's transmit, so its timing is
    /// judged again: a receive timeout well within P2 of that transmit is the VCI's early end,
    /// `system`, as the exchange's own entry would have judged it.
    #[test]
    fn a_late_transmit_indication_judges_its_exchange_s_timing_again() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let mut early = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::NoResponse,
        );
        early.vci_ended = true;
        early.pending_visibility = exchange::PendingVisibility::Forwarded;
        early.times.vci = Some(exchange::VciTimes {
            rx_timeout: Some(device(1_020_000)),
            error: Some(device(1_020_000)),
            ..exchange::VciTimes::default()
        });
        let mut later = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x92],
            exchange::End::NoResponse,
        );
        later.vci_ended = true;
        later.times.vci = Some(exchange::VciTimes {
            tx_done: Some(device(3_000_000)),
            ..exchange::VciTimes::default()
        });
        let indication = UnattachedItem {
            vci_us: 1_000_000,
            ..tx_done(0x7E0)
        };
        let lines = run_on_opened_link(vec![
            (early.clone(), Vec::new()),
            (later.clone(), vec![indication.clone()]),
        ]);
        let exchanges = of_event(&lines, "exchange");
        assert_eq!(exchanges[0]["timing"]["window"], "not_judged", "{lines:?}");
        let [late] = &of_event(&lines, "late_item")[..] else {
            panic!("{lines:?}");
        };
        assert_eq!(late["exchanges"][0]["classification"], "system", "{late}");
        // A start read after it, before the timeout, refutes that judgement: the timeout was the
        // transport's, and the response did not complete. The `system` the transmit indication
        // gave sets no floor.
        let start = UnattachedItem {
            vci_us: 1_010_000,
            ..start_of_message(0x7E8)
        };
        let lines = run_on_opened_link(vec![(early, Vec::new()), (later, vec![indication, start])]);
        let late = of_event(&lines, "late_item");
        assert_eq!(late.len(), 2, "{lines:?}");
        assert_eq!(
            late[1]["outcome"]["result"], "response_incomplete",
            "{lines:?}"
        );
        assert_eq!(
            late[1]["exchanges"][0]["classification"], "undetermined",
            "{lines:?}"
        );
    }

    /// ADR-370 item 4: the floor an ambiguous indication gave an owed exchange stands after that
    /// exchange leaves the window, when a later indication only it could own confirms its send.
    #[test]
    fn an_ambiguity_floor_stands_after_its_exchange_leaves_the_window() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let read = |did: u8, end: exchange::End| {
            let mut evidence = evidence(endpoint(0x7E0), &[0x22, 0xF1, did], end);
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.vci_ended = true;
            evidence
        };
        let answered = || {
            let mut evidence = read(0x91, exchange::End::Positive(vec![0x62, 0xF1, 0x91, 0x41]));
            evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxDoneTime);
            evidence
        };
        let mut later = read(0x92, exchange::End::NoResponse);
        later.times.vci = Some(exchange::VciTimes {
            tx_done: Some(device(3_000_000)),
            ..exchange::VciTimes::default()
        });
        let indication = |vci_us: u32| UnattachedItem {
            vci_us,
            ..tx_done(0x7E0)
        };
        let mut exchanges = vec![
            // A matched answer for F190, then a silent F190 read whose send is unknown.
            (
                read(0x90, exchange::End::Positive(vec![0x62, 0xF1, 0x90, 0x41])),
                Vec::new(),
            ),
            (read(0x90, exchange::End::NoResponse), Vec::new()),
            // An indication either it or the exchange in flight, whose send its response's
            // transmit done shows but which has no time of its own, could own raises it.
            (
                {
                    let mut evidence = read(0x93, exchange::End::NoResponse);
                    evidence.transmit_evidence = Some(exchange::TransmitEvidence::TxDoneTime);
                    evidence
                },
                vec![indication(5)],
            ),
        ];
        exchanges.extend((0..exchange::WINDOW).map(|_| (answered(), Vec::new())));
        exchanges.push((later, vec![indication(1_000_000)]));
        let lines = run_on_opened_link(exchanges);
        let entries = of_event(&lines, "exchange");
        let late = of_event(&lines, "late_item");
        let settled = late
            .iter()
            .find(|late| late.get("sent").is_some())
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(settled["refers_to"][0], entries[1]["seq"], "{settled}");
        assert_eq!(settled["sent"], "confirmed", "{settled}");
        assert_eq!(
            settled["exchanges"][0]["classification"], "undetermined",
            "{settled}"
        );
    }

    /// The maintainer's decisions (2026-10-28): an exchange in flight with no transmit done of
    /// its own, as on J2534, that gets an answer is not lowered by an indication an earlier
    /// exchange whose send is unknown could own, and that earlier exchange is named once, not
    /// after every later exchange. A response's start does not bound the exchange in flight:
    /// its own transmit indication comes before it.
    #[test]
    fn an_answered_exchange_keeps_its_classification_beside_an_ambiguous_indication() {
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let unknown = || {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x90],
                exchange::End::NoResponse,
            );
            evidence.vci_ended = true;
            evidence
        };
        // Answered reads of another identifier, whose responses carry a start and no transmit
        // done; their own transmit indication comes before that start.
        let answered = |at_us: u64| {
            let mut evidence = evidence(
                endpoint(0x7E0),
                &[0x22, 0xF1, 0x91],
                exchange::End::Positive(vec![0x62, 0xF1, 0x91, 0x41]),
            );
            evidence.pending_visibility = exchange::PendingVisibility::Forwarded;
            evidence.times.vci = Some(exchange::VciTimes {
                rx_start: Some(device(at_us + 10_000)),
                ..exchange::VciTimes::default()
            });
            evidence
        };
        let own = |at_us: u64| UnattachedItem {
            vci_us: u32::try_from(at_us).unwrap(),
            ..tx_done(0x7E0)
        };
        let lines = run_on_opened_link(vec![
            (unknown(), Vec::new()),
            (answered(1_000_000), vec![own(1_000_000)]),
            (answered(2_000_000), vec![own(2_000_000)]),
        ]);
        let exchanges = of_event(&lines, "exchange");
        assert_eq!(exchanges[0]["sent"], "unknown", "{lines:?}");
        for answered in &exchanges[1..] {
            assert_eq!(answered["classification"], "none", "{answered}");
        }
        // Not settled by the indication before the start; raised once, not after each.
        assert_eq!(of_event(&lines, "late_item").len(), 1, "{lines:?}");
        assert!(of_event(&lines, "late_item")[0]["unattached"].is_u64());
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

    /// The `seq` and classification of each exchange a `vci_loss_settled` settles.
    fn settled_exchanges(entry: &serde_json::Value) -> Vec<(u64, &str)> {
        entry["exchanges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|exchange| {
                (
                    exchange["seq"].as_u64().unwrap(),
                    exchange["classification"].as_str().unwrap(),
                )
            })
            .collect()
    }

    /// The `vci_loss_settled` a run writes over `existing` when its VCI connects with `reading`.
    fn settled_over(existing: &str, reading: Option<exchange::VciTime>) -> serde_json::Value {
        let sink = Recording::default();
        let log = CommLog::over(Box::new(sink.clone()), existing.as_bytes());
        assert!(log.loss_to_settle(), "{existing}");
        log.begin(&dpdu_facts(), RunKind::Resume);
        let reading = reading.map(|time| ConnectReading {
            time,
            read: log.now(),
        });
        log.vci_connected(Some(&serial(77)), reading).unwrap();
        let taken = String::from_utf8(sink.taken.lock().unwrap().clone()).unwrap();
        taken
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| line["event"] == "vci_loss_settled")
            .unwrap()
    }

    /// A record line of loss `loss` at `seq` that ended the exchanges `ended`.
    fn loss_line(seq: u64, loss: u64, ended: &[(u64, &str)]) -> String {
        let ended: Vec<serde_json::Value> = ended
            .iter()
            .map(|(seq, classification)| serde_json::json!({"seq": seq, "classification": classification}))
            .collect();
        format!(
            "{}\n",
            serde_json::json!({"seq": seq, "event": "vci_loss", "loss": loss, "agent_epoch": 0, "ended": ended, "refused": [], "clocks": []})
        )
    }

    /// The scan settles the last record no `vci_loss_settled` names: a settlement of another loss
    /// leaves it, an unreadable record is passed over, and an earlier loss a later one followed
    /// is not settled. The entry's classification is the most severe of the loss's and its
    /// exchanges', and the exchanges come in `seq` order; a reading on the worker's clock is
    /// recorded but compared with nothing.
    #[test]
    fn the_last_unsettled_record_is_settled_with_its_most_severe_exchange() {
        let header = "{\"seq\":0,\"event\":\"header\"}\n";
        // L1 at 2, settled at 3; L2 at 5, whose later record at 7 does not read.
        let log = format!(
            "{header}{}{}{}{}{}{}{}",
            loss_line(2, 2, &[(1, "undetermined")]),
            "{\"seq\":3,\"event\":\"vci_loss_settled\",\"loss\":2}\n",
            "{\"seq\":4,\"event\":\"vci_loss_settled\",\"loss\":99}\n",
            loss_line(5, 5, &[(4, "system"), (1, "undetermined")]),
            "{\"seq\":6,\"event\":\"exchange\"}\n",
            "{\"seq\":7,\"event\":\"vci_loss\",\"loss\":5,\"ended\":\"damaged\"}\n",
            "{\"seq\":8,\"event\":\"job_end\"}\n",
        );
        let worker = exchange::VciTime::new(5, Some(exchange::Clock::Worker));
        let entry = settled_over(&log, Some(worker));
        assert_eq!(entry["loss"], 5, "{entry}");
        assert_eq!(entry["refers_to"], serde_json::json!([1, 4]), "{entry}");
        assert_eq!(
            settled_exchanges(&entry),
            [(1, "undetermined"), (4, "system")]
        );
        assert_eq!(entry["power_kept"], "unknown", "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        assert_eq!(entry["clock"]["reading"]["clock"], "worker", "{entry}");
        assert!(entry["clock"]["fits_carried_on"].is_null(), "{entry}");
        // Only L1, settled: nothing to settle.
        let settled = format!(
            "{header}{}{}",
            loss_line(2, 2, &[(1, "undetermined")]),
            "{\"seq\":3,\"event\":\"vci_loss_settled\",\"loss\":2}\n"
        );
        let log = CommLog::over(Box::new(Recording::default()), settled.as_bytes());
        assert!(!log.loss_to_settle());
        // L1 then L2 unsettled: only L2, the last.
        let both = format!(
            "{header}{}{}",
            loss_line(2, 2, &[(1, "undetermined")]),
            loss_line(4, 4, &[(3, "undetermined")])
        );
        let entry = settled_over(&both, None);
        assert_eq!(entry["loss"], 4, "{entry}");
        assert_eq!(entry["refers_to"], serde_json::json!([3]), "{entry}");
    }

    /// A primitive the loss ended after the agent stopped waiting for it is an exchange the loss
    /// ended, recorded as its `primitive_ended` classifies it.
    #[test]
    fn a_primitive_the_loss_ended_is_recorded_with_its_settlement() {
        let dir = dir();
        let setup = setup(&dir);
        let log = CommLog::open(&setup).unwrap();
        log.begin(&dpdu_facts(), RunKind::First);
        let seq = log
            .exchange_with(
                evidence(
                    endpoint(0x7E0),
                    &[0x22, 0xF1, 0x90],
                    exchange::End::VciDidNotEnd,
                ),
                Vec::new(),
            )
            .unwrap();
        log.primitive_ended(
            seq,
            PrimitiveEnded {
                tx_indication: None,
                end: Some(exchange::End::VciLost {
                    refused_after_loss: false,
                }),
                detail: exchange::Detail::default(),
                pending: Vec::new(),
                vci: exchange::VciTimes::default(),
                vci_ended: false,
                link_errors: Vec::new(),
                indications: Vec::new(),
            },
        )
        .unwrap();
        log.end("ok");
        drop(log);
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let ended = lines
            .iter()
            .find(|line| line["event"] == "primitive_ended")
            .unwrap();
        let record = lines
            .iter()
            .find(|line| line["event"] == "vci_loss")
            .unwrap();
        assert_eq!(
            record["seq"],
            ended["seq"].as_u64().unwrap() + 1,
            "{record}"
        );
        assert_eq!(
            record["ended"],
            serde_json::json!([{"seq": seq, "classification": ended["exchanges"][0]["classification"]}]),
            "{record}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn dpdu_facts() -> RunFacts {
        RunFacts {
            vci: Some(VciFacts {
                api: crate::launch::Api::Dpdu,
                long_size: 8,
            }),
            ..facts()
        }
    }

    fn serial(number: u32) -> vci_service_interface::VersionData {
        vci_service_interface::VersionData {
            hw_serial_number: number,
            ..vci_service_interface::VersionData::default()
        }
    }

    /// Design 8.11 `vci_loss_settled`, ADR-373: the run that loses the VCI records the loss
    /// again after each exchange it ends or causes, and the next run that connects settles the
    /// last record, after its `vci_connected`, against the first reading of the VCI's clock. A
    /// later run's agent clock cannot measure the gap, so the power is not shown kept.
    #[test]
    fn a_lost_vci_is_recorded_and_settled_when_a_later_run_connects() {
        let dir = dir();
        let setup = setup(&dir);
        let device = |us| exchange::VciTime::new(us, Some(exchange::Clock::Device));
        let log = CommLog::open(&setup).unwrap();
        assert!(!log.loss_to_settle());
        log.begin(&dpdu_facts(), RunKind::First);
        log.vci_connected(Some(&serial(77)), None).unwrap();
        let mut answered = evidence(
            endpoint(0x7E0),
            &[0x22, 0xF1, 0x90],
            exchange::End::Positive(vec![0x62, 0xF1, 0x90, 1]),
        );
        answered.times.vci = Some(exchange::VciTimes {
            rx_end: Some(device(1_835_511_040)),
            ..exchange::VciTimes::default()
        });
        log.exchange_with(answered, Vec::new()).unwrap();
        let lost = |refused_after_loss| {
            evidence(
                endpoint(0x7E0),
                &[0x2E, 0xF1, 0x98, 0],
                exchange::End::VciLost { refused_after_loss },
            )
        };
        let ended = log.exchange_with(lost(false), Vec::new()).unwrap();
        let refused = log.exchange_with(lost(true), Vec::new()).unwrap();
        log.end("Host");
        drop(log);
        let lines = lines(&setup);
        assert_consecutive(&lines);
        let records = of_event(&lines, "vci_loss");
        assert_eq!(records.len(), 2, "{lines:?}");
        assert_eq!(records[0]["seq"], ended + 1);
        assert_eq!(records[1]["seq"], refused + 1);
        // The later record restates the loss whole.
        for record in &records {
            assert_eq!(record["loss"], ended + 1, "{record}");
            assert_eq!(record["agent_epoch"], 0, "{record}");
            assert_eq!(record["serial_number"], 77, "{record}");
            assert_eq!(
                record["ended"],
                serde_json::json!([{"seq": ended, "classification": "undetermined"}]),
                "{record}"
            );
            assert_eq!(
                record["clocks"],
                // The evidence gave no agent time for the response's read.
                serde_json::json!([{"clock": "device", "epoch": 0, "us": 1_835_511_040u64, "read_us": 0}]),
                "{record}"
            );
        }
        assert_eq!(records[0]["refused"], serde_json::json!([]));
        assert_eq!(
            records[1]["refused"],
            serde_json::json!([{"seq": refused, "classification": "undetermined"}])
        );

        // A run that does not connect the VCI leaves the loss to the next one.
        let log = CommLog::open(&setup).unwrap();
        assert!(log.loss_to_settle());
        log.begin(&dpdu_facts(), RunKind::Resume);
        log.end("Link");
        drop(log);

        let log = CommLog::open(&setup).unwrap();
        assert!(log.loss_to_settle());
        log.begin(&dpdu_facts(), RunKind::Resume);
        let reading = ConnectReading {
            time: device(1_866_720_397),
            read: log.now(),
        };
        log.vci_connected(Some(&serial(77)), Some(reading)).unwrap();
        assert!(!log.loss_to_settle());
        log.end("ok");
        drop(log);
        let lines = self::lines(&setup);
        assert_consecutive(&lines);
        let start = lines.len() - 6;
        assert_eq!(
            events(&lines[start..]),
            [
                "agent_clock",
                "job_start",
                "vci_connected",
                "clock_epoch",
                "vci_loss_settled",
                "job_end"
            ]
        );
        let settled = &lines[start + 4];
        assert_eq!(settled["loss"], ended + 1, "{settled}");
        assert_eq!(
            settled["refers_to"],
            serde_json::json!([ended, refused]),
            "{settled}"
        );
        assert_eq!(settled["clock"]["last_before"]["us"], 1_835_511_040u64);
        assert_eq!(settled["clock"]["last_before"]["agent_epoch"], 0);
        assert_eq!(settled["clock"]["reading"]["us"], 1_866_720_397u64);
        assert_eq!(settled["clock"]["reading"]["agent_epoch"], 2);
        assert_eq!(settled["clock"]["reading"]["epoch"], 1, "{settled}");
        assert!(settled["clock"]["agent_gap_us"].is_null(), "{settled}");
        assert!(settled["clock"]["fits_carried_on"].is_null(), "{settled}");
        assert_eq!(settled["serial_match"], true);
        assert_eq!(settled["power_kept"], "unknown");
        assert_eq!(settled["classification"], "undetermined");
        assert!(
            settled["reason"].as_str().unwrap().contains("later run"),
            "{settled}"
        );
        assert_eq!(
            settled_exchanges(settled),
            [(ended, "undetermined"), (refused, "undetermined")]
        );
        assert!(
            settled["exchanges"][0]["reason"]
                .as_str()
                .unwrap()
                .ends_with("The exchange keeps its classification."),
            "{settled}"
        );

        // Settled once: a later run finds nothing to settle.
        let log = CommLog::open(&setup).unwrap();
        assert!(!log.loss_to_settle());
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The comparison of `vci_loss_settled` where one agent epoch measures the gap: a log whose
    /// record names the agent epoch the reconnecting run is in. A clock that carried on on the
    /// same VCI shows the power kept, and the exchanges are `system`; a restart shows it lost;
    /// another serial number, a J2534 VCI or no reading leave it unknown.
    #[test]
    fn a_loss_settles_by_the_comparison_when_one_agent_epoch_measures_the_gap() {
        let record = |serial: u32| {
            format!(
                "{{\"seq\":0,\"event\":\"header\",\"clock\":{{\"epoch\":0}}}}\n\
                 {{\"seq\":1,\"event\":\"vci_loss\",\"loss\":1,\"agent_epoch\":1,\
                 \"ended\":[{{\"seq\":0,\"classification\":\"undetermined\"}}],\"refused\":[],\
                 \"clocks\":[{{\"clock\":\"device\",\"epoch\":4,\"us\":1835511040,\"read_us\":0}},\
                 {{\"clock\":\"worker\",\"epoch\":2,\"us\":9,\"read_us\":0}}],\"serial_number\":{serial}}}\n\
                 {{\"seq\":2,\"event\":\"job_end\"}}\n"
            )
        };
        let settle = |serial_after: u32, facts: RunFacts, reading_us: Option<u64>| {
            let sink = Recording::default();
            let log = CommLog::over(Box::new(sink.clone()), record(77).as_bytes());
            assert!(log.loss_to_settle());
            log.begin(&facts, RunKind::Resume);
            let reading = reading_us.map(|us| {
                let read = log.now();
                // The gap the agent's clock measured: 31.2092 s since the last time was read.
                let read = AgentNow {
                    monotonic_us: 31_209_200,
                    ..read
                };
                ConnectReading {
                    time: exchange::VciTime::new(us, Some(exchange::Clock::Device)),
                    read,
                }
            });
            log.vci_connected(Some(&serial(serial_after)), reading)
                .unwrap();
            let taken = String::from_utf8(sink.taken.lock().unwrap().clone()).unwrap();
            let settled: serde_json::Value = taken
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|line| line["event"] == "vci_loss_settled")
                .unwrap();
            settled
        };
        // Carried on: the last time plus the gap, within 2 ms plus 100 ppm of it.
        let carried = 1_835_511_040 + 31_209_200 + 3_000;
        let entry = settle(77, dpdu_facts(), Some(carried));
        assert_eq!(entry["power_kept"], true, "{entry}");
        assert_eq!(entry["classification"], "system", "{entry}");
        assert_eq!(settled_exchanges(&entry), [(0, "system")]);
        assert_eq!(entry["clock"]["agent_gap_us"], 31_209_200);
        assert_eq!(entry["clock"]["expected_us"], 1_835_511_040u64 + 31_209_200);
        assert_eq!(entry["clock"]["tolerance_us"], 2_000 + 3_120);
        assert_eq!(entry["clock"]["drift_from"], "default");
        assert_eq!(entry["clock"]["fits_carried_on"], true);
        assert_eq!(entry["clock"]["fits_restarted"], false);
        assert_eq!(entry["clock"]["last_before"]["epoch"], 4);
        // The reading opens the clock's first epoch of the run, after the log's largest.
        assert_eq!(entry["clock"]["reading"]["epoch"], 0, "{entry}");
        // Restarted: powered off, the supply's drop not excluded.
        let entry = settle(77, dpdu_facts(), Some(20_000_000));
        assert_eq!(entry["power_kept"], false, "{entry}");
        assert_eq!(entry["classification"], "undetermined");
        assert_eq!(settled_exchanges(&entry), [(0, "undetermined")]);
        // Another VCI's clock that fits: unknown.
        let entry = settle(78, dpdu_facts(), Some(carried));
        assert_eq!(entry["serial_match"], false);
        assert_eq!(entry["power_kept"], "unknown", "{entry}");
        assert!(entry["reason"].as_str().unwrap().contains("serial number"));
        // A worker that gives no serial number: unknown, whatever the reading.
        let entry = settle(0, dpdu_facts(), Some(carried));
        assert_eq!(entry["power_kept"], "unknown", "{entry}");
        assert!(entry["serial_match"].is_null(), "{entry}");
        assert!(entry["reason"].as_str().unwrap().contains("serial number"));
        // The API the job was told changes nothing: the same evidence settles the same
        // (design principle 6).
        let j2534 = RunFacts {
            vci: Some(VciFacts {
                api: crate::launch::Api::J2534,
                long_size: 8,
            }),
            ..facts()
        };
        let entry = settle(77, j2534, Some(carried));
        assert_eq!(entry["power_kept"], true, "{entry}");
        // No reading after the reconnect.
        let entry = settle(77, dpdu_facts(), None);
        assert_eq!(entry["power_kept"], "unknown", "{entry}");
        assert!(entry["clock"]["reading"].is_null());
    }
}

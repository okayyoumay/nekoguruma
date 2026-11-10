//! The exchange entry of the communication log: its types, the evidence it is built from, the
//! per-link history and the classification (design 8.11, ADR-346 items 2, 3 and 6, ADR-347 item
//! 6).
//!
//! Everything here is pure. A producer (a later part, in the host) gathers an [`Evidence`] for
//! one exchange, asks its [`LinkState`] for the exchange's [`LinkHistory`]
//! ([`LinkState::history_for`]), builds the [`ExchangeEntry`] ([`ExchangeEntry::build`], which
//! classifies it) and records it ([`LinkState::record`]). [`Evidence`] has a place for everything
//! the classification table uses, so that later parts add producers and rows, not fields.
//!
//! The rows of design 8.11's table that today's evidence can reach are implemented in the
//! table's order by [`classify`]; each row it cannot reach yet is named by a comment where it
//! would stand. What today's evidence cannot give: no loopback, and one answer per send-receive
//! (`num_receive_cycles` 1). The window is judged by [`super::timing`] once `link_opened` has
//! recorded the link's limits; evidence on a link the log did not open keeps the timing it
//! gives.
//!
//! `text` and `reason` are built from the domain enums only, never from an internal name (a
//! host error, a gRPC status, an error event); those go to [`Detail`].

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use diag_ir::{Idempotency, Interruptible, Section, SourceSpan};
use serde::{Deserialize, Serialize};

use super::clocks::Placed;
use super::redaction::{Direction, LoggedMessage, redact, redact_unanswering};
use crate::host::positive_prefix;

/// The `event` of an exchange entry.
const EVENT: &str = "exchange";

const SID_SESSION_CONTROL: u8 = 0x10;
const SID_ECU_RESET: u8 = 0x11;
pub(crate) const SID_NEGATIVE: u8 = 0x7F;
pub(crate) const NRC_PENDING: u8 = 0x78;

/// Who sent the request (design 8.11, `origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    Procedure,
    /// A request re-sent after a resume (8.2.5), or within a run after a failed attempt (ADR-392).
    Repeat,
    Identification,
    /// The write guard's reads (8.9.1).
    Guard,
    /// The reads of the flash preconditions (8.9).
    Precondition,
    RestartCheck,
    /// A restart's ECUReset (8.2.5).
    Teardown,
    AccessProgram,
    KeepAlive,
}

/// A position in the program's source (the program's source map, 8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct SourcePos {
    pub file_index: u32,
    pub line: u32,
    pub column: u32,
}

impl From<&SourceSpan> for SourcePos {
    fn from(span: &SourceSpan) -> Self {
        Self {
            file_index: span.file,
            line: span.line,
            column: span.column,
        }
    }
}

/// The attributes of a section in force (8.10.1).
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct SectionAttrs {
    pub interruptible: Interruptible,
    pub idempotency: Idempotency,
    /// How long the section is expected to take, 0 when it declares none.
    pub expected_millis: u32,
}

impl From<&Section> for SectionAttrs {
    fn from(section: &Section) -> Self {
        Self {
            interruptible: section.interruptible,
            idempotency: section.idempotency,
            expected_millis: section.expected_millis,
        }
    }
}

/// The VM position of a request a program issued (design 8.11, `at`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct At {
    pub pc: u32,
    pub steps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourcePos>,
    pub sections: Vec<SectionAttrs>,
}

/// Who the request was meant for (design 8.11, `target`): a role serializes as a plain string
/// (`"primary"`), an endpoint as an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub(crate) enum Target {
    /// An exchange sent before roles are bound: the link and the addresses.
    Endpoint { link: u32, tx_id: u32, rx_id: u32 },
    /// The target's role, once bound.
    Role(String),
}

/// Whether the request reached the bus (design 8.11, `sent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[expect(
    clippy::enum_variant_names,
    reason = "`not_sent` is design 8.11's name of the value"
)]
pub(crate) enum Sent {
    Confirmed,
    /// The first frame of a multi-frame request went out and no more.
    Partial,
    NotSent,
    Unknown,
}

/// What set `sent` (design 8.11, `sent_evidence`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SentEvidence {
    TxIndication,
    Loopback,
    TxDoneTime,
    /// An answer of the ECU that no owed request could own.
    Answer,
    FlowControl,
    Refused,
    TxError,
    None,
}

/// A transmit confirmation the link gave (the evidence behind `sent` `confirmed` other than an
/// answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransmitEvidence {
    TxIndication,
    Loopback,
    TxDoneTime,
}

impl From<TransmitEvidence> for SentEvidence {
    fn from(evidence: TransmitEvidence) -> Self {
        match evidence {
            TransmitEvidence::TxIndication => Self::TxIndication,
            TransmitEvidence::Loopback => Self::Loopback,
            TransmitEvidence::TxDoneTime => Self::TxDoneTime,
        }
    }
}

/// The clock a VCI time is on (design 8.11 "On one clock", ADR-120).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Clock {
    /// The VCI's own clock.
    Device,
    /// A worker's synthetic clock.
    Worker,
}

/// Whether the link shows pendings to the agent (design 8.11, `pending_visibility`). A link on
/// which a profile declares 0x21 or 0x23 a pending code is `AbsorbedByVci` as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingVisibility {
    Forwarded,
    AbsorbedByVci,
}

/// A pending (0x78) the agent saw.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Pending {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vci_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    pub agent_us: u64,
    /// The epoch of `vci_us` on its clock (design 8.11 `clock_epoch`, ADR-361); absent when the
    /// time names no clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    /// The interval from the transmit or the previous pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_previous_ms: Option<f64>,
}

/// What `timing`'s response time is measured between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MeasuredBy {
    VciTxDoneToRxStart,
    /// Judged one way only: a response's end includes its own transfer.
    VciTxDoneToRxEnd,
    /// Reported only, never judged.
    Agent,
}

/// How the exchange's times stand against the limit in force (design 8.11, `window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Window {
    WithinP2,
    WithinP2Star,
    PossiblePending,
    Late,
    TimeoutAtLimit,
    TimeoutEarly,
    TimeoutLate,
    NotJudged,
}

/// Why a window is `not_judged` (design 8.11, `timing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotJudgedReason {
    /// The exchange has no transmit time on a clock.
    NoTransmitTime,
    /// A time to judge, or a pending the limit depends on, is not on the transmit's clock and
    /// epoch (design 8.11 "On one clock").
    OtherClock,
    /// The response's end, with no start, came after the limit: it includes the response's
    /// own transfer, so it proves the response neither on time nor late.
    EndAfterLimit,
    /// No response start, end or receive timeout has a VCI time.
    NoTimeToJudge,
    /// A time to judge lies before the transmit done.
    BeforeTransmit,
    /// The receive timeout came after the ECU may have begun a response, so it is the
    /// transport's consecutive-frame timer, not the limit in force.
    ResponseBegun,
    /// No profile gives the link's timing margin, so nothing bounds the error of the VCI's
    /// times (ADR-377).
    NoMargin,
}

/// The entry's timing group (design 8.11, `timing`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Timing {
    pub response_ms: Option<f64>,
    pub measured_by: MeasuredBy,
    pub window: Window,
    /// By how much a `late`, `timeout_early` or `timeout_late` window was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_by_ms: Option<f64>,
    /// Why the window is `not_judged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_judged_reason: Option<NotJudgedReason>,
    /// The forwarded pending a `late` window names, by its place in `pending` from 0, when that
    /// pending came after the limit in force before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_pending: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2_star_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_ms: Option<u32>,
    /// The final response's interval from the latest pending seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_latest_pending_ms: Option<f64>,
}

impl Timing {
    /// What the evidence of today gives: agent times only, nothing judged.
    pub(crate) fn not_judged() -> Self {
        Self {
            response_ms: None,
            measured_by: MeasuredBy::Agent,
            window: Window::NotJudged,
            window_by_ms: None,
            not_judged_reason: Some(NotJudgedReason::NoTransmitTime),
            late_pending: None,
            p2_ms: None,
            p2_star_ms: None,
            completion_ms: None,
            margin_ms: None,
            since_latest_pending_ms: None,
        }
    }
}

/// Agent times of an exchange (monotonic, with the agent epoch, and wall).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AgentTimes {
    pub epoch: u32,
    pub before_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_confirmed_us: Option<u64>,
    pub end_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_wall_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_wall_unix_ms: Option<u64>,
}

/// One VCI time of an exchange (design 8.11 `times`): the raw microseconds, the clock its item
/// named (absent when it named none: the time is then compared with no other, "On one clock",
/// ADR-353) and that clock's epoch, which the log gives it when it places the time on its clock
/// (ADR-361), with the time unwrapped within the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct VciTime {
    pub us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    #[serde(skip)]
    pub unwrapped: Option<i64>,
}

impl VciTime {
    /// A raw time on `clock`, not placed yet.
    pub(crate) fn new(us: u64, clock: Option<Clock>) -> Self {
        Self {
            us,
            clock,
            epoch: None,
            unwrapped: None,
        }
    }

    /// The time placed on its clock at `placed`.
    pub(crate) fn at(self, placed: Option<Placed>) -> Self {
        Self {
            epoch: placed.map(|placed| placed.epoch),
            unwrapped: placed.map(|placed| placed.unwrapped),
            ..self
        }
    }

    /// Where the time stands on its clock, once placed.
    pub(crate) fn placed(&self) -> Option<(Clock, Placed)> {
        Some((
            self.clock?,
            Placed {
                epoch: self.epoch?,
                unwrapped: self.unwrapped?,
            },
        ))
    }

    /// Of an indication's time and the one the response's item carries for the same point,
    /// the response's (`given`), unless only the indication's names a clock: a time with none
    /// is compared with no other.
    pub(crate) fn prefer(indication: Self, given: Self) -> Self {
        if given.clock.is_none() && indication.clock.is_some() {
            indication
        } else {
            given
        }
    }
}

/// VCI times of an exchange, each as its item gave it, with its own clock: on a J2534 link a
/// frame's time is on `device` and an error's on `worker` (design 8.11 "On one clock").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct VciTimes {
    /// The transmit done: a transmit indication's time, or the one the response's item carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_done: Option<VciTime>,
    /// The response's start: a start-of-message indication's time, or the one the response's
    /// item carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_start: Option<VciTime>,
    /// The response's end: the final response's own time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_end: Option<VciTime>,
    /// The error event the entry names (`detail`): a lost VCI, a failed primitive's last error,
    /// else the last link error the primitive reported or else its receive timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<VciTime>,
    /// The reading of the VCI's clock the host took when the agent's deadline passed
    /// (`vci_did_not_end`, design 8.11 "Settlement"), with the time from the deadline to the
    /// read's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<ClockReading>,
    /// The VCI's receive timeout that ended the exchange, which `timing` judges; `error` gives
    /// it only when no other error is named. Not written.
    #[serde(skip)]
    pub rx_timeout: Option<VciTime>,
    /// The item that finished a collecting primitive, which ends its window when answers came:
    /// then no receive timeout does (ISO 22900-2:2022 8.2.6.2.2). `timing` judges the window's
    /// end by it when it is on the clock the answers are. Not written.
    #[serde(skip)]
    pub finished: Option<VciTime>,
    /// When the host read the final response's item, on the entry's agent epoch: the log places
    /// its times in the order the host read them (ADR-361). Not written.
    #[serde(skip)]
    pub response_read_us: Option<u64>,
}

/// A reading of the VCI's clock (`GetTimestamp`): the time, with the clock the worker says it is
/// on (ADR-353), and the agent's monotonic time from the agent's deadline to the answer, which
/// bounds when after the deadline the clock was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ClockReading {
    #[serde(flatten)]
    pub time: VciTime,
    pub round_trip_us: u64,
}

impl VciTimes {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Gives each pending its interval from the transmit (`transmit`, the transmit done placed on
/// its clock) or the previous pending (design 8.11 `pending`), each pending placed at `placed`
/// (ADR-361): a difference only between two VCI times on the same clock and epoch ("On one
/// clock"), unwrapped, so an interval across a wrap is given, and only forward. A pending with
/// no time, clock or place starts no interval.
pub(crate) fn pending_intervals(
    pending: &mut [Pending],
    placed: &[Option<Placed>],
    transmit: Option<(Clock, Placed)>,
) {
    let mut previous = transmit;
    for (index, pending) in pending.iter_mut().enumerate() {
        let this = pending
            .clock
            .zip(placed.get(index).copied().flatten())
            .filter(|_| pending.vci_us.is_some());
        pending.since_previous_ms = match (previous, this) {
            (Some((before_clock, before)), Some((clock, at)))
                if before_clock == clock
                    && before.epoch == at.epoch
                    && at.unwrapped >= before.unwrapped =>
            {
                Some((at.unwrapped - before.unwrapped) as f64 / 1000.0)
            }
            _ => None,
        };
        previous = this;
    }
}

/// The entry's times group (design 8.11, `times`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Times {
    pub agent: AgentTimes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vci: Option<VciTimes>,
}

/// The last supply-voltage reading of the VCI (8.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct SupplyVoltage {
    pub millivolts: u32,
    pub age_ms: u64,
}

/// The internal names, for developers (design 8.11, `detail`). Nothing else in the entry
/// carries one.
///
/// A producer puts names and codes here, never message bytes: the host error is the error's
/// variant name, not its text (which prints response bytes, and would carry a seed or a key past
/// [`redact`]), and the strings that are not `&'static str` are for a name or a code the producer
/// reads off the error, not for its message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Detail {
    /// The host error's variant name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_error: Option<&'static str>,
    /// The D-PDU error event's name, or its code when the interface lists no name for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_event: Option<String>,
    /// The D-PDU error event's extra error code (ISO 22900-2:2022 D.4.2), as the worker gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_error_info: Option<u32>,
    /// The name the worker gives the extra error information (ADR-351): for a D-PDU worker the
    /// `SHORT_NAME` the library's MDF gives the code (Annex F, F.4), for a J2534 worker its own
    /// name for a case it tells apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_error_info_name: Option<String>,
    /// The gRPC status code's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc_status: Option<&'static str>,
    /// The worker's error detail: a name or a code, never a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_detail: Option<String>,
    /// The native library's return code, as a name or a number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_return_code: Option<String>,
    /// The primitive the request went as, when it was not a send-receive: `startcomm` for the
    /// session control that starts the link's communication, and with it the worker's
    /// tester-present (ADR-420 item 3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cop_type: Option<&'static str>,
}

/// What kind of error a link reported (design 8.11 "Link error").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LinkErrorKind {
    VciHardwareFault,
    ResourceLocked,
    Initialisation,
    Receive,
    Protocol,
    Frame,
    TesterPresent,
    /// A transmit error. Its extra error information is not interpreted (ADR-351), so it is
    /// logged as a link error with `sent` `unknown`.
    Transmit,
    /// An unlisted code.
    Other,
}

impl LinkErrorKind {
    fn phrase(self) -> &'static str {
        match self {
            Self::VciHardwareFault => "a fault of the VCI itself",
            Self::ResourceLocked => "a resource of the link held elsewhere",
            Self::Initialisation => "an initialisation error",
            Self::Receive => "a receive error",
            Self::Protocol => "a protocol error",
            Self::Frame => "a frame error",
            Self::TesterPresent => "a tester-present error",
            Self::Transmit => "a transmit error",
            Self::Other => "an error of another kind",
        }
    }

    /// The `link_error` rows of the table.
    fn judge(self) -> (Classification, String) {
        match self {
            Self::VciHardwareFault | Self::ResourceLocked => (
                Classification::System,
                format!(
                    "The VCI reported a fault of its own, or the link's resource was held elsewhere: {}.",
                    self.phrase()
                ),
            ),
            Self::Transmit => (
                Classification::Undetermined,
                "The link reported a transmit error; the detail is not available, so it is not known whether the request reached the bus, and the vehicle network and the VCI's receive path can both cause it."
                    .to_owned(),
            ),
            _ => (
                Classification::Undetermined,
                format!(
                    "The link reported {}; the vehicle network and the VCI's receive path can both cause it, and the link's history shows whether the network answered earlier.",
                    self.phrase()
                ),
            ),
        }
    }
}

/// Why the system did not send a request (design 8.11, `not_sent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefusalCause {
    /// The ECU's earlier response, in the exchange the entry's `caused_by` names, does not give
    /// what this request needs (no block length in a RequestDownload answer).
    EarlierResponse,
    Policy,
    BlockTooLong,
    PreparationError,
    NoTransferActive,
    EmptyRequest,
    /// The job's definition does not declare the service, or its number is not one a request
    /// can carry (it does not fit a byte).
    UndeclaredService,
    /// The worker refused it for its own reasons: invalid parameters, a full queue, a request
    /// the protocol cannot carry.
    SystemOwnReasons,
    /// An earlier primitive the agent stopped waiting for still holds the link, and the worker
    /// did not cancel it (design 8.11 "Settlement").
    LinkHeld,
    /// Switching the worker's tester present on failed, or could not be done, so the job's
    /// session is no longer kept alive (design 8.2.5, ADR-420 item 6).
    KeepAliveFailed,
    /// The worker's tester-present reported an error while the ECU was in a non-default
    /// session, so the session may have ended and the job stops (design 8.2.5, ADR-420 item 8).
    /// `caused_by` is the `keep_alive` entry of that error.
    SessionLost,
    /// The link's event stream failed before the request started, so no answer could have been
    /// read (design 8.11).
    StreamFailed,
}

impl RefusalCause {
    /// `caused_by` is the entry of the response that caused an `EarlierResponse` refusal.
    fn phrase(self, caused_by: Option<u64>) -> String {
        match self {
            Self::EarlierResponse => match caused_by {
                Some(entry) => format!(
                    "the ECU's earlier response (entry {entry}) does not give what this request needs"
                ),
                None => "the ECU's earlier response does not give what this request needs"
                    .to_owned(),
            },
            Self::Policy => "the agent's request policy does not allow this service".to_owned(),
            Self::BlockTooLong => {
                "the request would be longer than the block length the ECU allows".to_owned()
            }
            Self::PreparationError => "the agent could not prepare the request".to_owned(),
            Self::NoTransferActive => {
                "no download is in progress, so there is no transfer to continue".to_owned()
            }
            Self::EmptyRequest => "the request has no bytes".to_owned(),
            Self::UndeclaredService => {
                "the job's definition does not declare this service, or its number is not one a request can carry".to_owned()
            }
            Self::SystemOwnReasons => "the system refused it for its own reasons (invalid parameters, a full queue or a request the protocol cannot carry)".to_owned(),
            Self::LinkHeld => "an earlier request the agent stopped waiting for still holds the link, and the system could not cancel it".to_owned(),
            Self::KeepAliveFailed => "the system could not keep the session alive, so the job's next request was not sent".to_owned(),
            Self::StreamFailed => "the system lost the link's stream of events, so no answer could be read and the request was not sent".to_owned(),
            Self::SessionLost => match caused_by {
                Some(entry) => format!(
                    "the tester-present kept running on the link reported an error (entry {entry}) while the ECU was in a non-default session, so the session may have ended"
                ),
                None => "the tester-present kept running on the link reported an error while the ECU was in a non-default session, so the session may have ended".to_owned(),
            },
        }
    }
}

/// How a part of the system failed (design 8.11, outcome `unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnknownCause {
    RpcFailure,
    WorkerNotAnswering,
    EventsEnded,
    EventsLost,
    PrimitiveFailedToStart,
}

impl UnknownCause {
    fn phrase(self) -> &'static str {
        match self {
            Self::RpcFailure => "a request to the diagnostic interface's software failed",
            Self::WorkerNotAnswering => {
                "the diagnostic interface's software did not answer in time"
            }
            Self::EventsEnded => "the diagnostic interface's software stopped reporting",
            Self::EventsLost => {
                "the diagnostic interface's software lost some of the link's reports, so the request may have been answered"
            }
            Self::PrimitiveFailedToStart => "the request could not be started on the link",
        }
    }
}

/// How the exchange ended, as the producer saw it: the input of `outcome`. The response bytes
/// are raw here; [`Outcome`] carries them through [`redact`].
///
/// No outcome rests on a worker's extra error information (ADR-351).
#[derive(Debug, Clone)]
#[expect(
    clippy::enum_variant_names,
    reason = "`vci_did_not_end` is design 8.11's name of the outcome"
)]
pub(crate) enum End {
    Positive(Vec<u8>),
    /// A response the worker took but the agent cannot use.
    PositiveUnusable(Vec<u8>),
    /// A negative response (`7F`, the SID, the code).
    Negative(Vec<u8>),
    NoResponse,
    /// The ECU began a response and did not finish it: a start-of-message indication was
    /// attached to an exchange the VCI ended without a response (design 8.11 "Link settings for
    /// the log"). [`CommLog::exchange_with`](super::CommLog::exchange_with) turns a `NoResponse`
    /// whose [`Evidence::vci_ended`] is set into it; a producer does not give it.
    ResponseIncomplete,
    /// The agent's deadline passed before the VCI ended the exchange.
    VciDidNotEnd,
    LinkError(LinkErrorKind),
    NotSent(RefusalCause),
    /// `refused_after_loss`: the worker refused the request on a link it had taken offline
    /// (ADR-294), so it was not sent.
    VciLost {
        refused_after_loss: bool,
    },
    Unknown(UnknownCause),
}

impl End {
    /// The response the exchange ended with, if any.
    pub(crate) fn response(&self) -> Option<&[u8]> {
        match self {
            Self::Positive(response)
            | Self::PositiveUnusable(response)
            | Self::Negative(response) => Some(response),
            _ => None,
        }
    }
}

/// The entry's `outcome`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Positive {
        response: LoggedMessage,
    },
    PositiveUnusable {
        response: LoggedMessage,
    },
    Negative {
        response: LoggedMessage,
        #[serde(skip_serializing_if = "Option::is_none")]
        nrc: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        nrc_name: Option<&'static str>,
        nrc_kind: NrcKind,
    },
    NoResponse,
    ResponseIncomplete,
    VciDidNotEnd,
    LinkError {
        error: LinkErrorKind,
    },
    NotSent {
        cause: RefusalCause,
    },
    VciLost,
    Unknown {
        cause: UnknownCause,
    },
}

impl Outcome {
    fn of(end: &End) -> Self {
        let message = |bytes: &[u8]| redact(Direction::Response, bytes);
        match end {
            End::Positive(bytes) => Self::Positive {
                response: message(bytes),
            },
            End::PositiveUnusable(bytes) => Self::PositiveUnusable {
                response: message(bytes),
            },
            End::Negative(bytes) => {
                let code = negative_code(bytes);
                Self::Negative {
                    response: message(bytes),
                    nrc: code.map(|code| format!("{code:#04x}")),
                    nrc_name: code.and_then(nrc_name),
                    nrc_kind: code.map_or(NrcKind::UnknownCode, nrc_kind),
                }
            }
            End::NoResponse => Self::NoResponse,
            End::ResponseIncomplete => Self::ResponseIncomplete,
            End::VciDidNotEnd => Self::VciDidNotEnd,
            End::LinkError(error) => Self::LinkError { error: *error },
            End::NotSent(cause) => Self::NotSent { cause: *cause },
            End::VciLost { .. } => Self::VciLost,
            End::Unknown(cause) => Self::Unknown { cause: *cause },
        }
    }
}

/// The code of a negative response, when it has one. It assumes the UDS negative shape (`7F`,
/// the SID, the code), as every profile the agent sends today uses; a profile with another
/// negative shape (ADR-321) would need its own reading of the code.
fn negative_code(response: &[u8]) -> Option<u8> {
    match response {
        [SID_NEGATIVE, _, code, ..] => Some(*code),
        _ => None,
    }
}

/// A result bound to the exchange's primitive that the host did not take as its answer (design
/// 8.11 `rejected`, ADR-359), raw; the entry carries its bytes through `redact_unanswering`. It counts for
/// neither the classification nor `link_history`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejected {
    /// The response identifier the entry lists: the header's, or the link's when the worker
    /// split none off.
    pub rx_id: u32,
    /// The identifier the worker's header named, none when it named none: what the
    /// `late_response` line of an offered result may say (ADR-447).
    pub identifier: Option<u32>,
    /// The RxFlag the worker gave it.
    pub rx_flag: Vec<u8>,
    pub bytes: Vec<u8>,
    pub vci_us: Option<u64>,
    pub clock: Option<Clock>,
    /// The epoch of `vci_us` on its clock, which the log gives it (ADR-361).
    pub epoch: Option<u32>,
    pub agent_us: u64,
    pub reason: RejectedReason,
    /// The `seq` of the `late_response` line the log wrote for it, when it offered the result
    /// to the exchanges kept owed and one or several could own it (design 8.11 `rejected`,
    /// ADR-447). Set by the log.
    pub late_response: Option<u64>,
}

/// Why a result bound to the primitive was not the exchange's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RejectedReason {
    /// A response that is not negative and does not begin as the request's positive response
    /// does (its SID or its echo).
    WrongEcho,
    /// A negative response that is not a complete negative response to the request's SID.
    PatternMismatch,
}

/// A [`Rejected`] result as the entry gives it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RejectedItem {
    pub rx_id: u32,
    pub response: LoggedMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vci_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    pub agent_us: u64,
    pub reason: RejectedReason,
    /// The `late_response` line that placed the result with the earlier exchanges that could
    /// own it (ADR-447).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_response: Option<u64>,
}

/// The worker's report that the link's events were lost during the exchange (design 8.11
/// `events_lost`, ADR-359): when the host read it and how many such notifications it read. The
/// host stops waiting at the first, so it gives one; the exchange's outcome is `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct EventsLost {
    pub agent_us: u64,
    pub notifications: u32,
}

/// One answer of a request that collects every answer, or the further answers of one meant for
/// one ECU, as the host read it (design 8.11, `answers`, ADR-400). The functional presence
/// request gives them (ADR-396). The raw form: [`ExchangeEntry::build`] classifies each into an
/// [`AnswerItem`].
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub rx_id: u32,
    /// What the answer's bytes are: positive, negative (a pending the window did not follow
    /// with a final answer included) or unusable.
    pub end: End,
    /// The item's time, with the clock it names, and the epoch of that time on it, which the
    /// log gives it (ADR-361).
    pub vci_us: Option<u64>,
    pub clock: Option<Clock>,
    pub epoch: Option<u32>,
    /// The start and the transmit done the item carries, on its clock.
    pub start_us: Option<u64>,
    pub tx_done_us: Option<u64>,
    pub agent_us: u64,
    /// The pendings that came in this answer's window, before it.
    pub pending: Vec<Pending>,
    /// Judged by the log from the end of the answer before it (ADR-400); the response time is
    /// from the transmit.
    pub timing: Timing,
    /// The link's history for this answer as an exchange of its own, as the log took it in;
    /// `None` is the entry's.
    pub history: Option<LinkHistory>,
}

impl Answer {
    /// Whether the answer is a response pending that no final answer on its identifier
    /// followed.
    pub(crate) fn is_pending(&self, expected: &Expected) -> bool {
        matches!(&self.end, End::Negative(bytes) if expected.pending(bytes))
    }

    /// Whether the answer is a result the worker took and the agent cannot use as the request's
    /// answer (`positive_unusable`, a wrong echo or a negative of another service): it is no
    /// answer on its identifier, so it folds no pending and covers no identifier (design 8.11
    /// "Owed request", ADR-437).
    pub(crate) fn is_rejected(&self) -> bool {
        matches!(self.end, End::PositiveUnusable(_))
    }

    /// Whether the answer is one the host takes as the primitive's response: neither a pending
    /// nor a rejected result (`Progress::answers`). The link errors the host reads are ordered
    /// against the first of these (`Progress::errors_before_response`), so the log orders the
    /// ones it reads for an end by the same answer.
    pub(crate) fn is_response(&self, expected: &Expected) -> bool {
        !self.is_pending(expected) && !self.is_rejected()
    }

    /// The end and the pendings the answer stands for as an exchange of its own: a lone pending
    /// is a request that got a pending and no response, its own pending the last of its list.
    /// The one conversion both the classification ([`Evidence::as_answer`]) and the log's
    /// judgement of the answer's time read.
    pub(crate) fn as_exchange(&self, expected: &Expected) -> (End, Vec<Pending>) {
        let mut pending = self.pending.clone();
        if !self.is_pending(expected) {
            return (self.end.clone(), pending);
        }
        pending.push(Pending {
            vci_us: self.vci_us,
            clock: self.clock,
            agent_us: self.agent_us,
            epoch: self.epoch,
            since_previous_ms: None,
        });
        (End::NoResponse, pending)
    }
}

/// Which of `items`, the results of a collecting primitive in the order they were read, stand as
/// answers: every result that is not a pending, and a pending only when no result on its
/// identifier (`key`) came after it, so a pending followed by its final answer is one answer
/// (ADR-396). A later result that is `rejected` (one the agent cannot use) is no answer on its
/// identifier, so it does not fold the pending before it (ADR-437). The one rule both the host's
/// list of a window's answers and the log's restatement of them at a settlement follow
/// (ADR-425).
pub(crate) fn standing_answers<T, K: PartialEq>(
    items: &[T],
    key: impl Fn(&T) -> K,
    pending: impl Fn(&T) -> bool,
    rejected: impl Fn(&T) -> bool,
) -> Vec<bool> {
    items
        .iter()
        .enumerate()
        .map(|(at, item)| {
            !pending(item)
                || !items[at + 1..]
                    .iter()
                    .any(|later| key(later) == key(item) && !rejected(later))
        })
        .collect()
}

/// Whether the final answers cover the receive table: every identifier of a non-empty `table`
/// has one (`final_on` tells it). An unnamed pending (identifier 0) is covered by them, as it
/// stands for the identifiers with no final answer (design 8.11 "Owed request", ADR-433). The
/// one definition [`Evidence::open_pending_ids`] and [`standing_answers_covering`] read.
pub(crate) fn table_covered(table: &[u32], final_on: impl Fn(u32) -> bool) -> bool {
    !table.is_empty() && table.iter().all(|&id| final_on(id))
}

/// [`standing_answers`] with the covering fold of an unnamed pending (ADR-433): once the final
/// answers cover every identifier of `table`, a standing pending that names none (`id` is 0)
/// leaves the list as well, since the last uncovered identifier's final answer is the one it
/// stood for. `id` is an item's response identifier, 0 when it names none. The one rule the
/// three restatements of a collecting exchange's answers follow: [`Kept::answered`], the log's
/// `primitive_ended`, and the host's collected answers.
pub(crate) fn standing_answers_covering<T>(
    items: &[T],
    id: impl Fn(&T) -> u32,
    pending: impl Fn(&T) -> bool,
    rejected: impl Fn(&T) -> bool,
    table: &[u32],
) -> Vec<bool> {
    let mut standing = standing_answers(items, &id, &pending, &rejected);
    // A rejected result is no final answer, so it covers no identifier (ADR-437).
    let final_on = |wanted: u32| {
        items
            .iter()
            .any(|item| !pending(item) && !rejected(item) && id(item) == wanted)
    };
    if table_covered(table, final_on) {
        for (item, standing) in items.iter().zip(&mut standing) {
            if *standing && pending(item) && id(item) == 0 {
                *standing = false;
            }
        }
    }
    standing
}

/// The end of a collecting request that was not ended by an error or the agent's deadline, and
/// whether that end is a rejected result (`end_rejected`): the end of its first standing answer
/// that is neither a pending nor a rejected result; else, when only rejected results (and
/// pendings) stood, the first rejected result's, as `positive_unusable` (ADR-437); else no
/// response, when nothing answered or only pendings did (ADR-396 item 4, ADR-429). A pending is
/// no final response, so a lone one leaves the request owed. The one rule the host ends an
/// exchange by and the log restates a settled end by (ADR-425); `end_rejected` is derived here
/// wherever an end is stated.
pub(crate) fn collecting_end(answers: &[Answer], expected: &Expected) -> (End, bool) {
    if let Some(answer) = answers
        .iter()
        .find(|answer| !answer.is_pending(expected) && !answer.is_rejected())
    {
        return (answer.end.clone(), false);
    }
    answers
        .iter()
        .find(|answer| answer.is_rejected())
        .map_or((End::NoResponse, false), |answer| {
            (answer.end.clone(), true)
        })
}

/// An [`Answer`] as the entry gives it, with its own outcome and classification.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnswerItem {
    pub rx_id: u32,
    pub response: LoggedMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vci_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u32>,
    pub agent_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_ms: Option<f64>,
    pub outcome: Outcome,
    pub classification: Classification,
    pub reason: String,
}

impl AnswerItem {
    fn of(answer: &Answer, classification: Classification, reason: String) -> Self {
        Self {
            rx_id: answer.rx_id,
            // A rejected result is no answer: its bytes are redacted as a rejected result's are
            // (ADR-359 item 1, ADR-437).
            response: if answer.is_rejected() {
                redact_unanswering(answer.end.response().unwrap_or(&[]))
            } else {
                redact(Direction::Response, answer.end.response().unwrap_or(&[]))
            },
            vci_us: answer.vci_us,
            clock: answer.clock,
            epoch: answer.epoch,
            agent_us: answer.agent_us,
            response_ms: answer.timing.response_ms,
            outcome: match &answer.end {
                End::PositiveUnusable(bytes) => Outcome::PositiveUnusable {
                    response: redact_unanswering(bytes),
                },
                end => Outcome::of(end),
            },
            classification,
            reason,
        }
    }
}

/// Which side an exchange's fault lies on (design 8.11 "Classification"), ordered by severity:
/// `system` over `undetermined` over `vehicle` over `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Classification {
    None,
    Vehicle,
    Undetermined,
    System,
}

/// What a negative response's code says (ISO 14229-1:2026 Annex A.1), as design 8.11's table
/// groups the codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NrcKind {
    SecurityKey,
    DefinitionMismatch,
    Ambiguous,
    Session,
    Access,
    Sequence,
    Attempts,
    SecuredData,
    Authentication,
    /// The ECU's or the vehicle's own state: busy, conditions not correct and its ranges, a
    /// subnet component that did not answer, a failure that prevents the action, programming
    /// state.
    EcuState,
    /// Any code the table does not list, reserved values included.
    UnknownCode,
}

/// The kind of a negative response code (design 8.11's table, ISO 14229-1:2026 Annex A.1).
pub(crate) fn nrc_kind(code: u8) -> NrcKind {
    match code {
        0x35 => NrcKind::SecurityKey,
        0x11..=0x13 => NrcKind::DefinitionMismatch,
        0x14 | 0x31 => NrcKind::Ambiguous,
        0x7E | 0x7F => NrcKind::Session,
        0x33 | 0x34 => NrcKind::Access,
        0x24 | 0x73 => NrcKind::Sequence,
        0x36 | 0x37 => NrcKind::Attempts,
        0x38..=0x3A => NrcKind::SecuredData,
        0x50..=0x5D => NrcKind::Authentication,
        // Busy, conditions not correct, a subnet component, a failure preventing the action,
        // programming state, and the conditions-not-correct ranges up to 0xFE.
        0x21 | 0x22 | 0x25 | 0x26 | 0x70..=0x72 | 0x81..=0x8D | 0x8F..=0xFE => NrcKind::EcuState,
        _ => NrcKind::UnknownCode,
    }
}

/// The name a code goes by (ISO 14229-1:2026 Annex A.1 defines the codes), written as this
/// repository writes it elsewhere (`sim-ecu`'s `Nrc`): one camelCase word, no punctuation. The
/// authentication codes 0x50 to 0x5D have no name here.
pub(crate) fn nrc_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x10 => "generalReject",
        0x11 => "serviceNotSupported",
        0x12 => "SubFunctionNotSupported",
        0x13 => "incorrectMessageLengthOrInvalidFormat",
        0x14 => "responseTooLong",
        0x21 => "busyRepeatRequest",
        0x22 => "conditionsNotCorrect",
        0x24 => "requestSequenceError",
        0x25 => "noResponseFromSubnetComponent",
        0x26 => "FailurePreventsExecutionOfRequestedAction",
        0x31 => "requestOutOfRange",
        0x33 => "securityAccessDenied",
        0x34 => "authenticationRequired",
        0x35 => "invalidKey",
        0x36 => "exceedNumberOfAttempts",
        0x37 => "requiredTimeDelayNotExpired",
        0x38 => "secureDataTransmissionRequired",
        0x39 => "secureDataTransmissionNotAllowed",
        0x3A => "secureDataVerificationFailed",
        0x70 => "uploadDownloadNotAccepted",
        0x71 => "transferDataSuspended",
        0x72 => "generalProgrammingFailure",
        0x73 => "wrongBlockSequenceCounter",
        0x78 => "requestCorrectlyReceivedResponsePending",
        0x7E => "SubFunctionNotSupportedInActiveSession",
        0x7F => "serviceNotSupportedInActiveSession",
        0x81 => "rpmTooHigh",
        0x82 => "rpmTooLow",
        0x83 => "engineIsRunning",
        0x84 => "engineIsNotRunning",
        0x85 => "engineRunTimeTooLow",
        0x86 => "temperatureTooHigh",
        0x87 => "temperatureTooLow",
        0x88 => "vehicleSpeedTooHigh",
        0x89 => "vehicleSpeedTooLow",
        0x8A => "throttlePedalTooHigh",
        0x8B => "throttlePedalTooLow",
        0x8C => "transmissionRangeNotInNeutral",
        0x8D => "transmissionRangeNotInGear",
        0x8F => "brakeSwitchesNotClosed",
        0x90 => "shifterLeverNotInPark",
        0x91 => "torqueConverterClutchLocked",
        0x92 => "voltageTooHigh",
        0x93 => "voltageTooLow",
        0x94 => "ResourceTemporarilyNotAvailable",
        0x95..=0xEF => "reservedForSpecificConditionsNotCorrect",
        0xF0..=0xFE => "vehicleManufacturerSpecificConditionsNotCorrect",
        _ => return None,
    })
}

/// The name a diagnostic service goes by (ISO 14229-1:2026 7.4 defines the identifiers), written
/// as this repository writes it elsewhere (design 8.11's example logs, `sim-ecu`). A reserved
/// identifier, such as 0x83, has no name.
pub(crate) fn service_name(sid: u8) -> Option<&'static str> {
    Some(match sid {
        0x10 => "DiagnosticSessionControl",
        0x11 => "ECUReset",
        0x14 => "ClearDiagnosticInformation",
        0x19 => "ReadDTCInformation",
        0x22 => "ReadDataByIdentifier",
        0x23 => "ReadMemoryByAddress",
        0x24 => "ReadScalingDataByIdentifier",
        0x27 => "SecurityAccess",
        0x28 => "CommunicationControl",
        0x29 => "Authentication",
        0x2A => "ReadDataByPeriodicIdentifier",
        0x2C => "DynamicallyDefineDataIdentifier",
        0x2E => "WriteDataByIdentifier",
        0x2F => "InputOutputControlByIdentifier",
        0x31 => "RoutineControl",
        0x34 => "RequestDownload",
        0x35 => "RequestUpload",
        0x36 => "TransferData",
        0x37 => "RequestTransferExit",
        0x38 => "RequestFileTransfer",
        0x3D => "WriteMemoryByAddress",
        0x3E => "TesterPresent",
        0x84 => "SecuredDataTransmission",
        0x85 => "ControlDTCSetting",
        0x86 => "ResponseOnEvent",
        0x87 => "LinkControl",
        _ => return None,
    })
}

/// How the worker matches an answer to the request (ADR-100): the positive response's prefix,
/// with what it echoes of the request, and the negative response's pattern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Expected {
    pub positive_prefix: Vec<u8>,
    pub negative_pattern: Vec<u8>,
}

impl Expected {
    /// Whether `bytes` is a pending (NRC 0x78) carrying this negative pattern.
    pub(crate) fn pending(&self, bytes: &[u8]) -> bool {
        !self.negative_pattern.is_empty()
            && bytes.starts_with(&self.negative_pattern)
            && bytes.get(self.negative_pattern.len()) == Some(&NRC_PENDING)
    }

    /// UDS's rule for `request`: the positive response's prefix as the worker is given it (the
    /// host's own rule: a data identifier or a sub-function echoed, without its suppress bit),
    /// and `7F` with the SID.
    pub(crate) fn uds(request: &[u8]) -> Self {
        let Some(&sid) = request.first() else {
            return Self {
                positive_prefix: Vec::new(),
                negative_pattern: Vec::new(),
            };
        };
        Self {
            positive_prefix: positive_prefix(request),
            negative_pattern: vec![SID_NEGATIVE, sid],
        }
    }

    /// Whether `answer` could be a response the worker would accept for this request.
    pub(crate) fn matches(&self, answer: &[u8]) -> bool {
        let starts = |pattern: &[u8]| !pattern.is_empty() && answer.starts_with(pattern);
        starts(&self.positive_prefix) || starts(&self.negative_pattern)
    }
}

/// Everything the classification and the entry are built from, for one exchange. A producer
/// fills what it has; the rest keeps its default ([`Evidence::new`]).
#[derive(Debug, Clone)]
pub(crate) struct Evidence {
    pub origin: Origin,
    /// The VM position, for a request a program issued.
    pub at: Option<At>,
    pub target: Target,
    pub link: u32,
    /// The response identifier of the link, which a matched answer shares.
    pub rx_id: u32,
    /// The request bytes, raw; the entry carries them through [`redact`].
    pub request: Vec<u8>,
    /// Whether the request asked for its positive response to be suppressed.
    pub suppressed: bool,
    pub expected: Expected,
    pub pending_visibility: PendingVisibility,
    pub end: End,
    /// The exchange whose response caused a `not_sent` refusal (`EarlierResponse`).
    pub caused_by: Option<u64>,
    /// The pendings the agent saw, bound to the exchange's primitive. On a link whose VCI may
    /// absorb them (`pending_visibility` `absorbed_by_vci`) an empty list proves nothing.
    pub pending: Vec<Pending>,
    /// The response identifier each of `pending` came on, by place, where the host knows it: a
    /// pending is kept per identifier (ADR-400). Shorter than `pending`, or `None`, for one
    /// whose identifier is not known; it then counts for every identifier. Not written.
    pub pending_ids: Vec<Option<u32>>,
    /// The response identifiers the link receives on, from the host's receive table (a
    /// functional link lists the identifiers of the ECUs it addresses, its own `rx_id` is 0).
    /// Empty counts as `[rx_id]`. Not written.
    pub receive_ids: Vec<u32>,
    pub timing: Timing,
    pub times: Times,
    /// A transmit confirmation the link gave. No link gives one yet.
    pub transmit_evidence: Option<TransmitEvidence>,
    /// A start-of-message indication for this exchange's response was attached: the first
    /// frame of a response is an answer of the ECU (design 8.11 "Answer of the ECU").
    pub response_started: bool,
    /// The VCI ended the exchange itself: the worker reported a receive timeout for the
    /// primitive, or the primitive's finished status ended a collecting window that took a
    /// result, usable or rejected (ADR-440), or a primitive that is not collecting on a wrong
    /// echo with no usable answer (ADR-443). An exchange the agent's own deadline ended did not
    /// (design 8.11 `vci_did_not_end`).
    pub vci_ended: bool,
    /// A start-of-message indication bound to the exchange's primitive was read.
    pub bound_start: bool,
    /// For an exchange the agent's deadline ended, how far the VCI's device clock, read at that
    /// deadline, was past the VCI's own limits with no response under way
    /// (`timing::past_vci_limits`); `None` when that is not shown.
    pub vci_limits_past_by_us: Option<i64>,
    /// The exchange's start got no handle, so the host keeps a stray that it finds again by the
    /// tag the start carried (ADR-416): the primitive's end settles the exchange later, and the
    /// `system` the start's failure gave stands under it.
    pub start_unresolved: bool,
    /// A flow-control frame from the ECU was seen (`partial`).
    pub flow_control: bool,
    /// Link errors the exchange's own link reported during it, other than its outcome.
    pub link_errors: Vec<LinkErrorKind>,
    /// Every error in `link_errors` was read before the exchange's final response, in the
    /// order the link's items were read (not by VCI time). An error read after the response, or
    /// no order known, leaves it `false`, and the error ends the answer
    /// ([`LinkState::record`]).
    pub link_errors_before_answer: bool,
    /// Link errors on other links of the same physical channel: evidence only (H3).
    pub channel_link_errors: Vec<LinkErrorKind>,
    /// The request collects every answer within a window the VCI ends (ADR-396). Its `end` is
    /// the first answer's, its `timing` judges the window's end, and each answer carries its own
    /// timing (ADR-400). Readers tell such an entry by its target and origin; the log has no key
    /// for it.
    pub collecting: bool,
    /// The answers of a collecting request, or all of a request meant for one ECU that got more
    /// than one (the first included), in the order they were read.
    pub answers: Vec<Answer>,
    /// The results bound to the primitive that it did not take as its answer.
    pub rejected: Vec<Rejected>,
    /// The worker's report of lost events, when it ended the exchange.
    pub events_lost: Option<EventsLost>,
    /// The end is a rejected result the worker took as the primitive's end (`positive_unusable`,
    /// ADR-359 item 3): it is no answer of the request, so the link's history does not count it,
    /// and its bytes are redacted as a rejected result's are.
    pub end_rejected: bool,
    /// The rejected end is a result the wait for the primitive's end read after the entry (not
    /// the entry's own), which the log never offered to the earlier exchanges (ADR-447).
    pub end_read_after_entry: bool,
    pub supply_voltage: Option<SupplyVoltage>,
    pub detail: Detail,
}

impl Evidence {
    /// Evidence of `request` that ended as `end`, on a link whose VCI absorbs pendings, with
    /// nothing else known: the answer is UDS's own expectation with no echo.
    pub(crate) fn new(
        origin: Origin,
        target: Target,
        link: u32,
        rx_id: u32,
        request: Vec<u8>,
        end: End,
    ) -> Self {
        let expected = Expected::uds(&request);
        Self {
            origin,
            at: None,
            target,
            link,
            rx_id,
            request,
            suppressed: false,
            expected,
            pending_visibility: PendingVisibility::AbsorbedByVci,
            end,
            caused_by: None,
            pending: Vec::new(),
            pending_ids: Vec::new(),
            receive_ids: Vec::new(),
            timing: Timing::not_judged(),
            times: Times {
                agent: AgentTimes {
                    epoch: 0,
                    before_us: 0,
                    tx_confirmed_us: None,
                    end_us: 0,
                    before_wall_unix_ms: None,
                    end_wall_unix_ms: None,
                },
                vci: None,
            },
            transmit_evidence: None,
            response_started: false,
            vci_ended: false,
            bound_start: false,
            vci_limits_past_by_us: None,
            start_unresolved: false,
            flow_control: false,
            link_errors: Vec::new(),
            link_errors_before_answer: false,
            channel_link_errors: Vec::new(),
            collecting: false,
            answers: Vec::new(),
            rejected: Vec::new(),
            events_lost: None,
            end_rejected: false,
            end_read_after_entry: false,
            supply_voltage: None,
            detail: Detail::default(),
        }
    }

    fn sid(&self) -> Option<u8> {
        self.request.first().copied()
    }

    /// Whether a late final response can settle this exchange once it is kept: the evidence shows
    /// the VCI ended its wait (`vci_ended`), or an identifier is still open on a pending (design
    /// 8.11 `late_response`, ADR-436). The pending alone is enough, whatever the primitive's
    /// end: a pending shows the ECU still owes its final answer, which the missing receive
    /// timeout does not contradict. The test is on the exchange, not on the identifier the final
    /// names, so a silent identifier of the same request settles too.
    pub(crate) fn late_final_settles(&self) -> bool {
        self.vci_ended || !self.open_pending_ids().is_empty()
    }

    /// The response identifiers whose last standing answer is a pending, in the order they first
    /// answered: a collecting request is owed once on each of them (design 8.11 "Owed request",
    /// ADR-429). An unnamed pending (identifier 0) is open while some identifier of the receive
    /// table has no final answer, and not once every one has it (ADR-433). Empty for any other
    /// request.
    pub(crate) fn open_pending_ids(&self) -> Vec<u32> {
        let mut ids = Vec::new();
        if !self.collecting {
            return ids;
        }
        // An unnamed pending is covered once the final answers cover the receive table.
        let covered = table_covered(&self.receive_ids, |id| self.final_standing(id));
        for answer in &self.answers {
            if answer.is_pending(&self.expected)
                && !ids.contains(&answer.rx_id)
                && !(answer.rx_id == 0 && covered)
            {
                ids.push(answer.rx_id);
            }
        }
        ids
    }

    /// The latest answer standing on the named response identifier `id`, a pending or a final
    /// one: what a late start on that identifier is judged against (design 8.11 `late_item`).
    pub(crate) fn latest_answer_on(&self, id: u32) -> Option<&Answer> {
        self.answers.iter().rev().find(|answer| answer.rx_id == id)
    }

    /// Whether a final answer on the named identifier `id` stands in the exchange; a rejected
    /// result is none (ADR-437).
    fn final_standing(&self, id: u32) -> bool {
        self.answers.iter().any(|answer| {
            answer.rx_id == id && !answer.is_pending(&self.expected) && !answer.is_rejected()
        })
    }

    /// The response identifiers the exchange is owed on (design 8.11 "Owed request", ADR-432),
    /// each once. With no receive table (a physical link) it is owed as before: on the
    /// identifiers a pending is open on, else on its link's `rx_id`. With a table, a request
    /// that is not collecting is owed on its `rx_id` once a final response ended it (and, when
    /// that end is a rejected one or the `rx_id` is 0, on every receive identifier too,
    /// ADR-443, ADR-446), else on every receive identifier; a collecting request with no answer
    /// on every receive identifier.
    /// With answers, it is owed on each of these once, in this order: the identifiers a pending
    /// is open on (an unnamed pending, identifier 0, standing for every
    /// receive identifier with no final answer, and for none when every one has it, ADR-400
    /// decision 1); every receive identifier with no named final answer standing
    /// ([`Self::silent_ids`], ADR-435); and, when the request ended with a final response, the
    /// identifiers whose final answers' own histories show an owner, so the owner is not the
    /// only request to find a late answer there. An identifier whose final answer no owed
    /// request could own is not owed.
    pub(crate) fn owed_ids(&self) -> Vec<u32> {
        // A link with no receive table (a physical one) is owed as before ADR-432: on the
        // identifiers a pending is open on, else on its own.
        if self.receive_ids.is_empty() {
            let open = self.open_pending_ids();
            return if self.collecting && !open.is_empty() {
                open
            } else {
                vec![self.rx_id]
            };
        }
        let table = self.receive_ids.clone();
        let mut ids: Vec<u32> = Vec::new();
        fn add(ids: &mut Vec<u32>, id: u32) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if !self.collecting {
            if self.final_response().is_some() {
                // An unnamed final (0) may have come on any identifier of the table, as for a
                // collecting exchange below (ADR-446).
                add(&mut ids, self.rx_id);
                if self.rx_id == 0 {
                    table.iter().for_each(|&id| add(&mut ids, id));
                }
            } else {
                table.iter().for_each(|&id| add(&mut ids, id));
            }
        } else if self.answers.is_empty() {
            table.iter().for_each(|&id| add(&mut ids, id));
        } else {
            // (1) The identifiers a pending is open on, and (2) the silent ones, whatever
            // another identifier answered (ADR-435).
            for id in self.unanswered_ids() {
                add(&mut ids, id);
            }
            // (3) The identifiers of the final answers an owed request could own, so that
            // request is not the only one to find a late answer there.
            if self.final_response().is_some() {
                for answer in self.answers.iter().filter(|answer| {
                    !answer.is_pending(&self.expected)
                        && !answer.is_rejected()
                        && answer
                            .history
                            .as_ref()
                            .is_some_and(|history| !history.owners.is_empty())
                }) {
                    // An unnamed answer (0) may have come on any identifier of the table.
                    if answer.rx_id == 0 {
                        table.iter().for_each(|&id| add(&mut ids, id));
                    } else {
                        add(&mut ids, answer.rx_id);
                    }
                }
            }
        }
        // A result the worker rejected is no answer (ADR-359): the exchange is owed as one with
        // no answer is, on every receive identifier, whatever else it is owed on, since it is not
        // settled either ([`Self::settles_owed`]). The set does not depend on its owners, so it
        // never grows after the fact. A request that is not collecting and ended with a rejected
        // end (`positive_unusable` on a wrong echo, ADR-443) is owed on its own identifier
        // and the table; for a collecting request the rejected result covers no identifier, so
        // the silent ones already make the table and the rule stays whole as an invariant
        // (ADR-435 decision 7, ADR-437).
        if self.end_rejected {
            table.iter().for_each(|&id| add(&mut ids, id));
        }
        ids
    }

    /// The identifiers a response that names none may have come on: for a collecting request
    /// with a receive table those still unanswered ([`Self::unanswered_ids`], so an identifier
    /// owed only because an owner could own its final answer does not count, ADR-435), else
    /// those the exchange is owed on.
    pub(crate) fn answerable_ids(&self) -> Vec<u32> {
        let unanswered = self.unanswered_ids();
        if unanswered.is_empty() {
            self.owed_ids()
        } else {
            unanswered
        }
    }

    /// The identifiers a collecting request's pendings are open on, with an unnamed pending
    /// standing for every receive identifier with no final answer, then its silent identifiers
    /// ([`Self::silent_ids`]), each once: those still unanswered by a final answer. A request
    /// that is not collecting, or has no receive table, has none.
    pub(crate) fn unanswered_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = Vec::new();
        if !self.collecting || self.receive_ids.is_empty() {
            return ids;
        }
        for id in self.open_pending_ids() {
            let named = if id == 0 {
                self.receive_ids.clone()
            } else {
                vec![id]
            };
            for id in named {
                if !self.final_standing(id) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        for id in self.silent_ids() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// The identifiers of a collecting request's receive table with no named final answer
    /// standing, in table order: what the ECUs behind them have not answered finally yet, whatever
    /// another identifier answered (ADR-435). An unnamed final answer (identifier 0) covers none.
    /// Empty with no receive table, and for a request that is not collecting.
    pub(crate) fn silent_ids(&self) -> Vec<u32> {
        if !self.collecting {
            return Vec::new();
        }
        self.receive_ids
            .iter()
            .copied()
            .filter(|&id| !self.final_standing(id))
            .collect()
    }

    /// The request may have reached the ECU, so the exchange awaits an answer: it was neither
    /// not sent nor refused after a VCI loss.
    pub(crate) fn awaits_response(&self) -> bool {
        !matches!(
            self.end,
            End::NotSent(_)
                | End::VciLost {
                    refused_after_loss: true
                }
        )
    }

    /// Whether `bytes` could be this request's own response cut short: a non-empty proper
    /// beginning of its positive prefix. The one rule the log offers a rejected result by and
    /// the classification judges the end placed with earlier exchanges by (ADR-447).
    pub(crate) fn own_cut_short(&self, bytes: &[u8]) -> bool {
        let prefix = &self.expected.positive_prefix;
        !bytes.is_empty() && bytes.len() < prefix.len() && prefix.starts_with(bytes)
    }

    /// The `late_response` line that placed the result this exchange ended on, when it ended
    /// on a rejected result (`end_rejected`) the log offered to the exchanges kept owed and
    /// some could own (ADR-447). The rejected end is the last rejected result, and its bytes
    /// are the end's, so a restated end that is another result is not it; nor is a rejected end the
    /// wait for the primitive's end read after the entry (`end_read_after_entry`), even with
    /// the same bytes.
    pub(crate) fn end_placed(&self) -> Option<u64> {
        let End::PositiveUnusable(bytes) = &self.end else {
            return None;
        };
        let last = self.rejected.last()?;
        if self.end_rejected && !self.end_read_after_entry && last.bytes == *bytes {
            last.late_response
        } else {
            None
        }
    }

    /// The exchange has a final response, no identifier is left open on a pending and none is
    /// silent ([`LinkEffect::settles`]), so a final response settles its owed request.
    pub(crate) fn settles_owed(&self) -> bool {
        self.final_response().is_some()
            && !self.end_rejected
            && self.open_pending_ids().is_empty()
            && self.silent_ids().is_empty()
    }

    /// The first link error the exchange saw: the one it ended with, else one read while it
    /// collected.
    fn first_link_error(&self) -> Option<LinkErrorKind> {
        match self.end {
            End::LinkError(kind) => Some(kind),
            _ => self
                .link_errors
                .first()
                .or_else(|| self.channel_link_errors.first())
                .copied(),
        }
    }

    /// Whether its final response is known to come after every link error it saw.
    fn answer_after_error(&self) -> bool {
        self.link_errors_before_answer
            && !self.link_errors.is_empty()
            && self.channel_link_errors.is_empty()
            && self.final_response().is_some()
    }

    /// Whether a link error it saw came after its answer, or in no known order with it, so it
    /// ended that answer's matched answer ([`LinkState::record`]).
    pub(crate) fn error_after_answer(&self) -> bool {
        self.first_link_error().is_some() && !self.answer_after_error()
    }

    /// The final response's bytes, when the exchange ended with one.
    pub(crate) fn final_response(&self) -> Option<&[u8]> {
        match &self.end {
            End::Positive(bytes) | End::PositiveUnusable(bytes) | End::Negative(bytes) => {
                Some(bytes)
            }
            _ => None,
        }
    }

    /// The answer of the ECU in this exchange, as far as ownership needs it: the final
    /// response, or else the pending a pending seen stands for (the negative response with code
    /// 0x78 for this request's SID).
    fn answer_head(&self) -> Option<Vec<u8>> {
        // A rejected result the worker ended the primitive with answers nothing (ADR-359); a
        // pending before it is still an answer of the ECU.
        if let (false, Some(response)) = (self.end_rejected, self.final_response()) {
            return Some(response.to_vec());
        }
        if self.pending.is_empty() {
            return None;
        }
        self.sid().map(|sid| vec![SID_NEGATIVE, sid, NRC_PENDING])
    }

    fn pending_seen(&self) -> bool {
        !self.pending.is_empty()
    }

    /// This evidence with `end` as its end, judged by `timing`: what a settling entry and an
    /// answer both classify (the shared part of [`Evidence::as_answer`] and
    /// [`Kept::answered`]).
    fn ended_as(&self, end: End, timing: Timing) -> Evidence {
        let mut ended = self.clone();
        ended.end = end;
        ended.timing = timing;
        ended.vci_limits_past_by_us = None;
        ended
    }

    /// The exchange `answer` alone would be (ADR-400): its end and timing are the answer's, and
    /// what belongs to the whole exchange (the answer list, the rejected results, the link
    /// errors, the lost events) is left to the entry. A pending the window did not follow with a
    /// final answer is a request that got a pending and no response, so the pending rows apply
    /// to it.
    pub(crate) fn as_answer(&self, answer: &Answer) -> Evidence {
        let (end, pending) = answer.as_exchange(&self.expected);
        let mut one = self.ended_as(end, answer.timing.clone());
        one.rx_id = answer.rx_id;
        one.pending = pending;
        one.pending_ids.clear();
        one.collecting = false;
        one.answers.clear();
        one.rejected.clear();
        one.link_errors.clear();
        one.channel_link_errors.clear();
        one.events_lost = None;
        one.end_rejected = answer.is_rejected();
        one
    }

    /// Gives each answer the history [`LinkState::answer_histories`] derived for it.
    pub(crate) fn set_answer_histories(&mut self, histories: Vec<LinkHistory>) {
        for (answer, history) in self.answers.iter_mut().zip(histories) {
            answer.history = Some(history);
        }
    }

    /// The exchange holds an answer the worker bound to it: a final response or a pending. The
    /// first frame of a response alone is not one (ADR-354): it confirms the send and counts as
    /// an answer, but the worker may still have discarded the full response.
    fn bound_answer(&self) -> bool {
        self.answer_head().is_some()
    }

    pub(crate) fn absorbing(&self) -> bool {
        self.pending_visibility == PendingVisibility::AbsorbedByVci
    }

    /// Whether a link error was reported on this exchange's link (during it, besides its
    /// outcome) or on any other link of the same physical channel (design 8.11 "Link error").
    fn any_link_error(&self) -> bool {
        !self.link_errors.is_empty() || !self.channel_link_errors.is_empty()
    }
}

/// The match key of an answer: the response identifier and the expected responses (design
/// 8.11 "Matched answer").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MatchKey {
    rx_id: u32,
    positive_prefix: Vec<u8>,
    negative_pattern: Vec<u8>,
}

impl MatchKey {
    fn of(evidence: &Evidence) -> Self {
        Self {
            rx_id: evidence.rx_id,
            positive_prefix: evidence.expected.positive_prefix.clone(),
            negative_pattern: evidence.expected.negative_pattern.clone(),
        }
    }
}

/// An owed request (design 8.11 "Owed request"). The list of them grows with every unanswered
/// request on the link until an ECUReset or session change ends it; a loop of unanswered
/// requests is bounded only by the job.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owed {
    seq: u64,
    rx_id: u32,
    sid: u8,
    expected: Expected,
}

/// The entry's `link_history` (design 8.11): how the link's ECU answered before this exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LinkHistory {
    /// Exchanges on this link that got an answer of the ECU earlier in this log.
    pub answered: u32,
    /// The same, since the last link error on the link's physical channel.
    pub answered_since_error: u32,
    /// Matched answers that count for this exchange.
    pub answered_same_match: u32,
    /// How many owed requests this exchange's answer may belong to.
    pub owed_matching: u32,
    /// Whether this exchange itself got an answer of the ECU.
    pub this_exchange: bool,
    /// The owed requests behind `owed_matching`, by `seq`.
    #[serde(skip)]
    pub owners: Vec<u64>,
}

impl LinkHistory {
    /// The history of a link that has not answered anything.
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            answered: 0,
            answered_since_error: 0,
            answered_same_match: 0,
            owed_matching: 0,
            this_exchange: false,
            owners: Vec::new(),
        }
    }
}

/// What an exchange changes in its link's state; [`LinkState::record`] applies it.
#[derive(Debug, Clone)]
struct LinkEffect {
    key: MatchKey,
    sid: Option<u8>,
    expected: Expected,
    sent: Sent,
    this_exchange: bool,
    /// The answer is a final response or a pending, not a response start alone
    /// ([`Evidence::bound_answer`]): only such an answer is a matched answer.
    bound_answer: bool,
    /// The owed requests the answer could belong to.
    owners: Vec<u64>,
    final_response: bool,
    /// [`Evidence::settles_owed`]: a final response, no identifier open on a pending, none silent.
    settles_owed: bool,
    /// The identifiers the exchange is owed on ([`Evidence::owed_ids`]).
    owed_ids: Vec<u32>,
    positive: bool,
    /// The final response is one the agent cannot use.
    unusable: bool,
    suppressed: bool,
    /// The request is a session change or an ECUReset.
    restarts_ecu: bool,
    /// A link error of this exchange's link or of another link of its channel.
    link_error: Option<LinkErrorKind>,
    /// The exchange has a final response and every link error it saw, on its own link, came
    /// before that response, with none from another link of the channel (which has no order
    /// against it): the response counts as an answer since the error.
    answer_after_error: bool,
}

impl LinkEffect {
    /// The exchange has a final response and no identifier is left open on a pending or silent
    /// (ADR-429, ADR-435).
    /// `placed_alone` adds to it that no owed request could own the answer, and `record` reads
    /// that; a settling entry adds the answer's own end ([`ExchangeEntry::settled_answer`]).
    fn settles(&self) -> bool {
        self.settles_owed
    }

    /// A session change or ECUReset that may have been carried out, though nothing shows it:
    /// one whose answer the agent cannot use, or one sent, with its positive response
    /// suppressed and no final response, whose silence is not classified `none` (no judged
    /// timeout shows that the ECU accepted it, ADR-376). It ends the matched answers only.
    fn unsure_restart(&self, classification: Classification) -> bool {
        self.restarts_ecu
            && (self.unusable
                || (self.suppressed
                    && !self.final_response
                    && self.sent != Sent::NotSent
                    && classification != Classification::None))
    }
}

/// Whether `request` is one whose acceptance ends the earlier requests' evidence: a
/// DiagnosticSessionControl or an ECUReset of any type (design 8.11 "Owed request", ADR-346
/// item 6).
fn restarts_ecu(request: &[u8]) -> bool {
    matches!(request, [SID_SESSION_CONTROL, ..] | [SID_ECU_RESET, ..])
}

/// What the log keeps of an owed exchange so a settling entry can classify it again (design
/// 8.11 "Settlement", ADR-365): its evidence, its entry's origin, request and classification,
/// and the VCI times and limits its timing was judged from.
#[derive(Debug, Clone)]
pub(crate) struct Kept {
    pub evidence: Evidence,
    pub origin: Origin,
    pub request: Request,
    pub classification: Classification,
    /// The entry's `sent`.
    pub sent: Sent,
    pub transmit: super::timing::At,
    pub pendings: Vec<super::timing::At>,
    /// Where the entry's pendings stand in the order the host read the entry's items
    /// (`usize::MAX` for one not read at a known place), by place in the evidence's list: a
    /// settling entry restates the windows of the answers with what it reads after them
    /// (ADR-425).
    pub pending_order: Vec<usize>,
    /// The same for the entry's answers, and where each answer ended.
    pub answer_order: Vec<usize>,
    pub answer_ends: Vec<super::timing::At>,
    /// The link's limits, when `link_opened` recorded them.
    pub limits: Option<super::timing::Limits>,
    /// A start-of-message with the response's identifier was read during the exchange and not
    /// attached (another exchange could own it): the response may have begun.
    pub start_unattached: bool,
    /// When the latest start-of-message indication read after its entry showed its response
    /// beginning, judged against the limit in force (a `late_item`, ADR-370, design 8.11
    /// "Settlement"): it decides a late response that completes that response.
    pub late_start: Option<LateStart>,
    /// The response identifier [`Kept::late_start`] was judged on, for a collecting exchange: a
    /// start on one identifier says nothing of another's response (ADR-429).
    pub late_start_id: Option<u32>,
    /// The forwarded pendings read after its entry, each with how it stood against the limit in
    /// force before it (a `late_item`, ADR-371). They come after [`Kept::pendings`], which are
    /// the ones its entry saw and judge its timing; only one in time carries the limit on.
    pub late_pendings: Vec<(super::timing::At, LatePending)>,
    /// For each of [`Kept::late_pendings`], how many of [`Kept::pendings`] came before it in the
    /// evidence's list (`Evidence::pending`), which lists them in the order they were read: the
    /// end's own pendings of a renewed exchange come after the late ones read before it.
    pub late_places: Vec<usize>,
    /// The least classification an ambiguous item read during or after it gave it, with the
    /// reason its reason then ends with: kept with the owed exchange too, so it stands after the
    /// exchange leaves the window (ADR-370).
    pub floor: Option<(Classification, String)>,
    /// The most severe verdict the late final answers that settled the exchange on some of its
    /// identifiers gave it, while it stays owed on the others (ADR-433): a later final answer
    /// is not judged below it. Kept apart from [`Kept::floor`], which ambiguous items set too.
    pub partial: Option<Classification>,
    /// A pending was open on the exchange when it was kept, at its entry or in the evidence its
    /// `primitive_ended` renewed ([`LinkState::note_pending_at_end`]; it only rises): its late
    /// final answers settle it on every identifier it stays owed on, though the first of them
    /// folds the pending out of the evidence (ADR-436). It gives none once the record's evidence
    /// settles the exchange and the record is kept only for its rows (ADR-445 decision 4).
    pub pending_at_end: bool,
    /// The link's clears just after its entry was taken in, which tell whether an answer it gets
    /// later counts for a later exchange ([`LinkState::reclassify_after`]).
    pub clears: Clears,
    /// Its entry's answer counted as a matched answer ([`ExchangeEntry::matched_answer`]).
    pub matched: bool,
    /// The time of the item that answers it: set from its entry's own items, then by each
    /// settling entry that judges an answer ([`LinkState::judge_answer`]). A later offer of the
    /// answer, with no new answering item, is judged by this time (ADR-385).
    pub answer_at: AnswerTime,
}

/// The time of the item that answers an owed exchange, as a settling entry sees it (ADR-384).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerTime {
    /// No item answers it.
    None,
    /// An item answers it, with no placed time.
    Unknown,
    /// The answering item's time.
    At(Clock, super::clocks::Placed),
}

#[cfg(test)]
impl From<Option<(Clock, super::clocks::Placed)>> for AnswerTime {
    fn from(time: Option<(Clock, super::clocks::Placed)>) -> Self {
        time.map_or(Self::Unknown, |(clock, placed)| Self::At(clock, placed))
    }
}

impl Kept {
    /// Whether a late final response can settle this kept exchange: its evidence allows it
    /// ([`Evidence::late_final_settles`]), or a pending was open when it was kept
    /// ([`Kept::pending_at_end`]) (ADR-436). A record whose evidence settles the exchange is kept
    /// only for its rows (ADR-442, ADR-445): the pending it had open earlier gives it no late
    /// final answer, as its in-time twin has none (ADR-445 decision 4). A non-collecting record
    /// whose evidence settles it is the same: it owns no late final (ADR-446), whatever ended
    /// the VCI's wait.
    pub(crate) fn late_final_settles(&self) -> bool {
        // A non-collecting exchange whose evidence settles it owns no late final, whatever ended
        // the VCI's wait (ADR-446); `vci_ended` stands.
        if !self.evidence.collecting && self.evidence.settles_owed() {
            return false;
        }
        self.evidence.late_final_settles() || (self.pending_at_end && !self.evidence.settles_owed())
    }

    /// The late start that bears on a response with identifier `response_id` (`None` when it
    /// names none): a collecting exchange's start counts for a response on the identifier it was
    /// judged on, or an unnamed one when that is the only identifier open; any other exchange's
    /// start counts for its response.
    pub(crate) fn late_start_for(&self, response_id: Option<u32>) -> Option<LateStart> {
        let start = self.late_start?;
        let (true, Some(own)) = (self.evidence.collecting, self.late_start_id) else {
            return Some(start);
        };
        let applies = match response_id {
            Some(id) => id == own,
            None => self.evidence.answerable_ids() == [own],
        };
        applies.then_some(start)
    }

    /// The time of the item that answers this exchange (design 8.11 `late_item`, ADR-384,
    /// ADR-385): the final response (`final_response`), else the latest pending, the settling
    /// entry's own (`new_pendings`); a start (`start`) only when the exchange has no answer yet,
    /// since it answers nothing (ADR-354). With no new answering item, the time stored with the
    /// exchange ([`Kept::answer_at`]). A time that is not placed is [`AnswerTime::Unknown`],
    /// whatever else the exchange has.
    pub(crate) fn answer_time(
        &self,
        final_response: Option<super::timing::At>,
        new_pendings: &[super::timing::At],
        start: Option<super::timing::At>,
    ) -> AnswerTime {
        let new = final_response
            .or_else(|| new_pendings.last().copied())
            .or(start.filter(|_| self.answer_at == AnswerTime::None));
        match new {
            None => self.answer_at,
            Some(None) => AnswerTime::Unknown,
            Some(Some((clock, placed))) => AnswerTime::At(clock, placed),
        }
    }
}

/// When a start-of-message indication read after an exchange's entry showed its response
/// beginning, against the exchange's limit in force (design 8.11 "Settlement" `late_item`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LateStart {
    /// This long after the transmit, before the limit less the margin: once the response
    /// completes, the system missed a response that started in time.
    InTime { at_us: i64 },
    /// At least this long after the limit plus the margin: the ECU started late, and the
    /// response that completes it is judged as a late one. `at_limit`: the exchange's timing,
    /// judged before the start was read, showed the VCI ending its wait at the limit, which a
    /// start after it does not refute.
    Late { by_us: i64, at_limit: bool },
}

/// How often the matched answers, and the answers since a link error, of a link have ended
/// (design 8.11 "Matched answer"): an earlier exchange's answer counts for a later exchange only
/// while no such end came between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Clears {
    matched: u64,
    errors: u64,
}

/// How many logged exchanges per link the log keeps the evidence of, so a settling entry can
/// reclassify the later exchanges whose evidence it changes (design 8.11 "Settlement"; the
/// maintainer's retention decision, ADR-368).
pub(crate) const WINDOW: usize = 16;

/// A logged exchange in its link's window ([`WINDOW`]): its evidence and the history it was
/// classified with, and its standing classification.
#[derive(Debug, Clone)]
struct Logged {
    seq: u64,
    evidence: Evidence,
    history: LinkHistory,
    origin: Origin,
    request: Request,
    classification: Classification,
    reason: String,
    /// The link's clears when its history was taken.
    clears: Clears,
    /// The link's clears just after its entry was taken in. A late link error shifts it as it
    /// shifts the record's `Kept::clears`, the exchange it falls after included (ADR-444).
    after: Clears,
    /// The least classification it keeps whatever its history gives, with the reason: an
    /// ambiguous response read during it may have been its own (design 8.11 `late_response`).
    floor: Option<(Classification, String)>,
    /// A settling entry of its own changed its evidence or set its classification: the history
    /// alone no longer gives it, so it is not reclassified from the history.
    settled: bool,
    /// The link's `Clears::matched` its answer counted as a matched answer in, if it did: it
    /// counts for a later exchange whose history was taken with the same value. An accepted
    /// ECUReset or session change read late counts the matched answers again from these
    /// ([`LinkState::reset_after`]).
    matched_at: Option<u64>,
    /// The time of the item that answers it (as [`Kept::answer_at`]): a recount of the answers
    /// since a late link error tests it ([`LinkState::since_late_error`], ADR-385).
    answer_at: AnswerTime,
}

/// Where a link error read late falls among a link's exchanges ([`LinkState::place_error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorPoint {
    /// Within the exchange logged as this `seq`.
    Within(u64),
    /// The exchange logged as this `seq` has no placed start on the error's clock and epoch, only
    /// a later placed time, and the error comes after every earlier exchange's latest placed
    /// time and not after its own: it may have come during the exchange, or between it and the
    /// one before. Only the exchange's own H3 reading holds (ADR-384).
    MaybeWithin(u64),
    /// After the exchange logged as this `seq`, before the next.
    After(u64),
    /// Before every exchange the window holds, within the owed exchange kept beyond it logged
    /// as `within`, if any.
    BeforeWindow { within: Option<u64> },
    /// No exchange the link keeps has a time on its clock and epoch: it cannot be placed among
    /// them.
    NotPlaced,
}

/// Where an exchange whose owners changed stands, for [`LinkState::free_owner_answer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// In the link's window, which is reclassified.
    Window,
    /// Kept beyond the window, its matched answer counted when freed (ADR-439).
    Beyond,
    /// Refreshed after an accepted restart, kept beyond the window or a survivor in the window
    /// (which may have no kept record): rows and record only, since the restart recounts the
    /// matched answers from the window alone.
    BeyondRows,
    /// Kept beyond the window and later than an accepted restart's exchange (ADR-441): rows and
    /// record, and its matched answer counted when the visit releases the exchange. Its epoch
    /// is the restart's, unlike a survivor's.
    BeyondReleased,
}

/// An answer a settling entry adds to an earlier exchange, as the later exchanges on its link see
/// it ([`LinkState::reclassify_after`]).
#[derive(Debug, Clone)]
pub(crate) struct SettledAnswer {
    /// The earlier exchange.
    pub seq: u64,
    key: MatchKey,
    /// It got its first answer of the ECU: one more exchange on the link has answered.
    newly_answered: bool,
    /// The answer is a matched answer it did not hold before.
    newly_matched: bool,
    /// It is a final response, which ends the earlier exchange as an owed request.
    final_response: bool,
    clears: Clears,
    /// The answer counts as one since the last link error: no late link error read after the
    /// exchange's entry may have come after it ([`LinkState::since_late_error`], ADR-384). When
    /// it does not, the answer counts as answered only, and as no matched answer.
    since_error: bool,
    /// The response identifiers the earlier exchange is owed on no more after this answer, while
    /// it stays owed on others (a settling entry that ends it on some identifiers only, design
    /// 8.11 "Owed request"): the later exchanges' answers on them are freed from it.
    settled_ids: Vec<u32>,
}

impl SettledAnswer {
    /// The answer with the identifiers the earlier exchange is owed on no more.
    pub(crate) fn with_settled_ids(mut self, settled_ids: Vec<u32>) -> Self {
        self.settled_ids = settled_ids;
        self
    }

    /// A link error in no known order with the answer ended its matched answer.
    pub(crate) fn no_matched_answer(&mut self) {
        self.newly_matched = false;
    }

    /// Whether the answer is shown to come after every late link error that may have come after
    /// it ([`LinkState::since_late_error`]); when it is not, it counts in `answered` only and is
    /// no matched answer for the exchanges after it (ADR-384).
    pub(crate) fn set_since_error(&mut self, since_error: bool) {
        self.since_error = since_error;
        if !since_error {
            self.newly_matched = false;
        }
    }
}

/// The time of a late link error read after the exchanges it falls within or after, on its clock
/// and epoch, and the newest exchange whose answer it may precede (ADR-384).
#[derive(Debug, Clone, Copy)]
struct LateError {
    clock: Clock,
    epoch: u32,
    unwrapped: i64,
    /// An answer of an exchange logged at or before this may have come after the error.
    through: u64,
}

/// A later exchange a settling entry reclassified, with its new classification and reason.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reclassified {
    pub seq: u64,
    pub link: u32,
    pub origin: Origin,
    pub request: Request,
    pub classification: Classification,
    pub reason: String,
}

/// The evidence of one link that later exchanges on it use (design 8.11 "Matched answer",
/// "Owed request"). A link that is opened again, a worker restart or a VCI reconnect starts the
/// matched answers over ([`LinkState::reopened`]) and keeps the owed requests, since a response
/// already on its way can still arrive.
#[derive(Debug, Default)]
pub(crate) struct LinkState {
    answered: u32,
    answered_since_error: u32,
    matched: HashMap<MatchKey, u32>,
    owed: Vec<Owed>,
    last_link_error: Option<(u64, LinkErrorKind)>,
    /// The `seq` of the last ECUReset or session change accepted.
    last_reset: Option<u64>,
    /// How many logged exchanges on the link ended with `sent` `unknown`: each could still own a
    /// transmit indication that comes late (design 8.11 `sent_evidence`).
    unknown_sends: u32,
    /// What is kept of each owed exchange, by `seq` ([`LinkState::keep`]).
    kept: HashMap<u64, Kept>,
    clears: Clears,
    /// The last [`WINDOW`] logged exchanges, oldest first, emptied by an accepted ECUReset or
    /// session change.
    window: VecDeque<Logged>,
    /// The largest `seq` that left the window.
    left_window: Option<u64>,
    /// The latest late link error placed within or after the exchanges the link keeps, per clock
    /// and epoch (ADR-384).
    late_errors: Vec<LateError>,
    /// The largest line a late link error was written as that could not be placed against the
    /// link's exchanges: it may have come after the answer of any exchange logged before it.
    late_error_unplaced: Option<u64>,
}

impl LinkState {
    /// The history the classification of `evidence` uses. Ownership never decides: every owed
    /// request the exchange's answer could belong to is counted.
    pub(crate) fn history_for(&self, evidence: &Evidence) -> LinkHistory {
        let head = evidence.answer_head();
        // A collecting exchange's answers are each looked up on their own (ADR-432): the
        // identifier and the bytes of one answer, never one answer's identifier with another's
        // bytes.
        let owners: Vec<u64> = if evidence.collecting && !evidence.answers.is_empty() {
            let mut owners: Vec<u64> = Vec::new();
            for answer in &evidence.answers {
                for owner in self.owners_of(&evidence.as_answer(answer)) {
                    if !owners.contains(&owner) {
                        owners.push(owner);
                    }
                }
            }
            owners
        } else {
            self.owners_of(evidence)
        };
        LinkHistory {
            answered: self.answered,
            answered_since_error: self.answered_since_error,
            answered_same_match: self
                .matched
                .get(&MatchKey::of(evidence))
                .copied()
                .unwrap_or(0),
            owed_matching: owners.len() as u32,
            // A start-of-message indication is attached only when no owed request could own
            // it, so it adds no owner.
            this_exchange: head.is_some() || evidence.response_started,
            owners,
        }
    }

    /// The owed requests the answer head of `evidence` could belong to, by `seq`: those owed on
    /// the identifier it came on and expecting its bytes. An exchange with no identifier of its
    /// own (0) on a link with a receive table, such as an unnamed answer, may have come on any
    /// identifier of the table (ADR-432).
    fn owners_of(&self, evidence: &Evidence) -> Vec<u64> {
        let Some(head) = evidence.answer_head() else {
            return Vec::new();
        };
        let unnamed = evidence.rx_id == 0 && !evidence.receive_ids.is_empty();
        let mut owners: Vec<u64> = Vec::new();
        for owed in &self.owed {
            let on_id = if unnamed {
                evidence.receive_ids.contains(&owed.rx_id)
            } else {
                owed.rx_id == evidence.rx_id
            };
            if on_id && owed.expected.matches(&head) && !owners.contains(&owed.seq) {
                owners.push(owed.seq);
            }
        }
        owners
    }

    /// The history of each answer of `evidence` as an exchange of its own (ADR-400): the one
    /// place that derives it, read at every moment the entry's history is taken again (the
    /// exchange's record, a settlement, a reset, a late link error), so that an answer's owners
    /// follow the owed requests as they are then. `entry` is the entry's history at that moment,
    /// whose counts the answers share; the owed requests are those logged before `before`. An
    /// answer on another identifier than the exchange's keeps the matched count it was taken in
    /// with, since the entry's does not count that identifier.
    pub(crate) fn answer_histories(
        &self,
        evidence: &Evidence,
        entry: &LinkHistory,
        before: u64,
    ) -> Vec<LinkHistory> {
        evidence
            .answers
            .iter()
            .map(|answer| {
                let fresh = self.history_for(&evidence.as_answer(answer));
                let owners: Vec<u64> = fresh
                    .owners
                    .into_iter()
                    .filter(|owner| *owner < before)
                    .collect();
                let answered_same_match = if answer.rx_id == evidence.rx_id {
                    entry.answered_same_match
                } else {
                    answer
                        .history
                        .as_ref()
                        .map_or(fresh.answered_same_match, |own| own.answered_same_match)
                };
                LinkHistory {
                    answered: entry.answered,
                    answered_since_error: entry.answered_since_error,
                    answered_same_match,
                    owed_matching: owners.len() as u32,
                    this_exchange: fresh.this_exchange,
                    owners,
                }
            })
            .collect()
    }

    /// The link was opened again, the worker restarted or the VCI reconnected (design 8.11
    /// "Matched answer"): the matched answers start over, since the link may now be set up
    /// differently. The owed requests and the counts `answered` and `answered_since_error` stay
    /// ("Owed request": a response already on its way can still arrive).
    pub(crate) fn reopened(&mut self) {
        self.matched.clear();
        self.clears.matched += 1;
        // The worker gives an indication only to an open link, so one sent before the link was
        // opened again cannot arrive on it.
        self.unknown_sends = 0;
    }

    /// Whether an ended exchange on this link could still own a transmit indication: one whose
    /// `sent` is `unknown`, however long ago it ended, on the same opened link (design 8.11
    /// `sent_evidence`). A refusal makes `sent` `not_sent`, so a refused exchange owns none. An
    /// exchange a transmit error or a flow-control failure ended is `unknown` today and counts,
    /// although design 8.11 lets it own none: that only keeps later indications `unattached`.
    pub(crate) fn may_own_tx_indication(&self) -> bool {
        self.unknown_sends > 0
    }

    /// Whether an owed request on this link could still own a start-of-message indication with
    /// response identifier `rx_id` (design 8.11 "Link settings for the log", "Owed request"): one
    /// that expects a response on that identifier, however long ago it ended. The indication
    /// carries no response bytes, so the identifier is all there is to match. The owed requests
    /// outlive a reopened link, so one from before the reopening still counts: that only keeps
    /// the indication `unattached`.
    pub(crate) fn may_own_start_indication(&self, rx_id: u32) -> bool {
        self.owed.iter().any(|owed| owed.rx_id == rx_id)
    }

    /// The owed requests a response `response` with response identifier `rx_id` could belong
    /// to (design 8.11 "Owed request", `late_response`), by `seq`; with no identifier, every
    /// owed request it matches.
    pub(crate) fn late_response_owners(&self, rx_id: Option<u32>, response: &[u8]) -> Vec<u64> {
        self.owed
            .iter()
            .filter(|owed| {
                rx_id.is_none_or(|id| id == owed.rx_id) && owed.expected.matches(response)
            })
            .map(|owed| owed.seq)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Keeps what a settling entry needs of exchange `seq`, while it is owed.
    pub(crate) fn keep(&mut self, seq: u64, kept: Kept) {
        if self.owed.iter().any(|owed| owed.seq == seq) {
            self.kept.insert(seq, kept);
        }
    }

    /// What is kept of owed exchange `seq`.
    pub(crate) fn kept(&self, seq: u64) -> Option<&Kept> {
        self.kept.get(&seq)
    }

    /// [`LinkState::may_own_tx_indication`], leaving out one exchange whose `sent` was `sent`.
    pub(crate) fn may_own_tx_indication_besides(&self, sent: Sent) -> bool {
        self.unknown_sends > u32::from(sent == Sent::Unknown)
    }

    /// [`LinkState::may_own_start_indication`], leaving out the owed exchange `seq`.
    pub(crate) fn may_own_start_indication_besides(&self, rx_id: u32, seq: u64) -> bool {
        self.owed
            .iter()
            .any(|owed| owed.rx_id == rx_id && owed.seq != seq)
    }

    /// The kept exchanges an unbound transmit indication or loopback echo for request
    /// identifier `tx_id` could belong to (design 8.11 `late_item`): those whose `sent` is still
    /// `unknown`, by `seq`; and whether they are every exchange on the link that could, which
    /// they are not when an exchange the log keeps nothing of ended with `sent` `unknown`.
    pub(crate) fn late_transmit_owners(&self, tx_id: u32) -> (Vec<u64>, bool) {
        let mut owners: Vec<u64> = self
            .kept
            .iter()
            .filter(|(_, kept)| {
                kept.sent == Sent::Unknown
                    && matches!(kept.evidence.target, Target::Endpoint { tx_id: id, .. } if id == tx_id)
            })
            .map(|(seq, _)| *seq)
            .collect();
        owners.sort_unstable();
        let unknown_kept = self
            .kept
            .values()
            .filter(|kept| kept.sent == Sent::Unknown)
            .count();
        (owners, self.unknown_sends as usize == unknown_kept)
    }

    /// The kept exchanges an unbound start-of-message indication for response identifier
    /// `rx_id` could belong to (design 8.11 `late_item`): the owed requests that expect a
    /// response on it, by `seq`; and whether the log keeps every one of them.
    pub(crate) fn late_start_owners(&self, rx_id: u32) -> (Vec<u64>, bool) {
        let owed: Vec<u64> = self
            .owed
            .iter()
            .filter(|owed| owed.rx_id == rx_id)
            .map(|owed| owed.seq)
            .collect();
        let kept: Vec<u64> = owed
            .iter()
            .copied()
            .filter(|seq| self.kept.contains_key(seq))
            .collect();
        let all = kept.len() == owed.len();
        (kept, all)
    }

    /// The kept exchanges a forwarded pending `pending` with response identifier `rx_id`, bound to
    /// no primitive, could belong to (design 8.11 `late_item`): the owed requests that expect a
    /// response on it whose negative pattern it carries, by `seq`; and whether the log keeps
    /// every one of them.
    pub(crate) fn late_pending_owners(&self, rx_id: u32, pending: &[u8]) -> (Vec<u64>, bool) {
        let owed: Vec<u64> = self
            .owed
            .iter()
            .filter(|owed| owed.rx_id == rx_id && owed.expected.pending(pending))
            .map(|owed| owed.seq)
            .collect();
        let kept: Vec<u64> = owed
            .iter()
            .copied()
            .filter(|seq| self.kept.contains_key(seq))
            .collect();
        let all = kept.len() == owed.len();
        (kept, all)
    }

    /// Owed exchange `seq` got the pending at `at`, which a `late_item` gave it and judged
    /// `window` against the limit in force before it.
    pub(crate) fn add_kept_late_pending(
        &mut self,
        seq: u64,
        at: super::timing::At,
        window: LatePending,
    ) {
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.late_places.push(kept.pendings.len());
            kept.late_pendings.push((at, window));
        }
    }

    /// Owed exchange `seq` has `evidence` now, which a `late_item` gave it, with `sent`, its
    /// transmit done, and, for a start, how its latest start was judged: an indication that confirms the send
    /// can own a late transmit indication no more.
    pub(crate) fn update_kept(
        &mut self,
        seq: u64,
        evidence: Evidence,
        sent: Sent,
        transmit: super::timing::At,
        late_start: Option<Option<LateStart>>,
    ) {
        let Some(kept) = self.kept.get_mut(&seq) else {
            return;
        };
        let confirmed = kept.sent == Sent::Unknown && sent != Sent::Unknown;
        kept.evidence = evidence;
        kept.sent = sent;
        kept.transmit = transmit;
        if let Some(late_start) = late_start {
            kept.late_start = late_start;
        }
        if confirmed {
            self.unknown_sends = self.unknown_sends.saturating_sub(1);
        }
    }

    /// The floor exchange `seq` keeps whatever its history gives, with the reason its reason then
    /// ends with: its window entry's, or, once it has left the window, the one kept with it.
    pub(crate) fn floor_of(&self, seq: u64) -> Option<(Classification, String)> {
        self.window
            .iter()
            .find(|logged| logged.seq == seq)
            .and_then(|logged| logged.floor.clone())
            .or_else(|| self.kept.get(&seq).and_then(|kept| kept.floor.clone()))
    }

    /// Drops what is kept of exchange `seq`, which nothing will settle.
    pub(crate) fn release_kept(&mut self, seq: u64) {
        self.kept.remove(&seq);
    }

    /// [`LinkState::release_kept`] for an exchange whose primitive just ended, except one that is
    /// still owed and that a late final can settle ([`Kept::late_final_settles`]): an identifier
    /// a pending is open on or was open on when it was kept, or a renewed `vci_ended`
    /// (ADR-429, ADR-431, ADR-436). `vci_ended` is true when the end read a receive
    /// timeout, or the finished status of a collecting window that took a result, usable or
    /// rejected (ADR-440), or of a primitive that is not collecting on a wrong echo with no
    /// usable answer (ADR-443), whatever else it read after (as an entry's is), so an end that
    /// read none (a cancel, or a link error, a lost VCI or an unknown end with no receive timeout) is
    /// released unless a pending is open on it or the kept record notes one open at its keep or
    /// after the end's renewal ([`Kept::pending_at_end`]). A record is kept as well while the
    /// exchange is still owed through an owner that could own its final answer, for its rows
    /// (ADR-445), derived from the rows and the evidence with no stored flag.
    pub(crate) fn release_kept_ended(&mut self, seq: u64) {
        let open = self.owed.iter().any(|owed| owed.seq == seq)
            && self.kept.get(&seq).is_some_and(|kept| {
                kept.late_final_settles()
                    || self
                        .kept_entry(seq, &kept.evidence, kept.classification)
                        .owed_through_owners()
            });
        if !open {
            self.kept.remove(&seq);
        }
    }

    /// Records on the kept exchange `seq` that a pending is open on its evidence now: the flag
    /// [`Kept::pending_at_end`] only rises, so a settlement or an end that folds the pending
    /// out of the evidence does not take the exchange's claim on its other identifiers away
    /// (ADR-436). Called when the exchange is kept and when `primitive_ended` renews it.
    pub(crate) fn note_pending_at_end(&mut self, seq: u64) {
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.pending_at_end |= !kept.evidence.open_pending_ids().is_empty();
        }
    }

    /// The history of `evidence`, an exchange's evidence that a settling entry for exchange
    /// `seq` gives: the exchange is not an owed request its own answer could belong to.
    /// (Answer histories: [`LinkState::answer_histories`].)
    pub(crate) fn history_settling(&self, evidence: &Evidence, seq: u64) -> LinkHistory {
        let mut history = self.history_for(evidence);
        history.owners.retain(|owner| *owner != seq);
        history.owed_matching = history.owners.len() as u32;
        history
    }

    /// Exchange `seq` got its final response: it is owed no more, and what was kept of it is
    /// given back.
    pub(crate) fn settle_owed(&mut self, seq: u64) -> Option<Kept> {
        self.owed.retain(|owed| owed.seq != seq);
        let kept = self.kept.remove(&seq)?;
        // The answer confirms its send: it can own a late transmit indication no more.
        if kept.sent == Sent::Unknown {
            self.unknown_sends = self.unknown_sends.saturating_sub(1);
        }
        Some(kept)
    }

    /// A link error came on the physical channel this link shares, logged as line `seq` (design
    /// 8.11 "Link error", "Matched answer"): the matched answers and the count since the error
    /// end, the owed requests stay. The log calls it on every target's state on that channel,
    /// so a target that sent nothing during the error still loses its matched answers;
    /// [`LinkState::record`] calls it for an error the exchange it takes in saw.
    pub(crate) fn channel_link_error(&mut self, seq: u64, kind: LinkErrorKind) {
        self.matched.clear();
        self.answered_since_error = 0;
        self.clears.matched += 1;
        self.clears.errors += 1;
        self.last_link_error = Some((seq, kind));
    }

    /// Takes in an exchange that was logged.
    ///
    /// - An answer counts in `answered` and `answered_since_error`. It is a matched answer for
    ///   its key only when it is a final response or a pending (a response start alone is not,
    ///   ADR-354), no owed request could own it and it did not accept an ECUReset or a session
    ///   change (that answer does not count for the exchanges after it).
    /// - A link error, on the exchange's own link or on another link of its physical channel,
    ///   ends the matched answers and the count since the error, not the owed requests
    ///   ([`LinkState::channel_link_error`], which the log also calls on the states of the
    ///   channel's other targets). An answer that every own-link error of its exchange preceded
    ///   (`Evidence::link_errors_before_answer`) counts as an answer since that error; any
    ///   other error ends the answer of its exchange.
    /// - An accepted DiagnosticSessionControl (0x10) or ECUReset (0x11) of any type (ADR-346
    ///   item 6), or one sent with its positive response suppressed and classified `none`, ends
    ///   the matched answers and the owed requests: all of them when its answer could belong to
    ///   none, otherwise those sent before the earliest request it could belong to.
    /// - A response to such a request that was accepted but is unusable (a bad field), or such
    ///   a request sent with its positive response suppressed whose silence is not classified
    ///   `none` (ADR-376), ends the matched answers only, since the ECU may be restarting but
    ///   the agent cannot tell.
    /// - The exchange is owed when its request may have reached the ECU whole (`sent`
    ///   `confirmed` or `unknown`) and no final response is placed with it alone.
    pub(crate) fn record(&mut self, entry: &ExchangeEntry) {
        let effect = &entry.effect;
        let accepting = effect.restarts_ecu
            && ((effect.positive && effect.final_response)
                || (effect.suppressed && entry.classification == Classification::None));
        let unsure_restart = effect.unsure_restart(entry.classification);
        // An error that came before the final response leaves the response counted since it:
        // the error is applied first. Any other error ends the answer of its exchange.
        if let (true, Some(kind)) = (effect.answer_after_error, effect.link_error) {
            self.channel_link_error(entry.seq, kind);
        }
        if effect.this_exchange {
            self.answered += 1;
            self.answered_since_error += 1;
            if effect.bound_answer && !accepting && !unsure_restart && effect.owners.is_empty() {
                *self.matched.entry(effect.key.clone()).or_insert(0) += 1;
            }
        }
        if let (false, Some(kind)) = (effect.answer_after_error, effect.link_error) {
            self.channel_link_error(entry.seq, kind);
        }
        if accepting {
            match effect.owners.iter().min() {
                None => self.owed.clear(),
                Some(&earliest) => self.owed.retain(|owed| owed.seq >= earliest),
            }
            let owed = &self.owed;
            self.kept
                .retain(|seq, _| owed.iter().any(|owed| owed.seq == *seq));
            // The exchanges that survive it, `earliest` up to it, lose the owners dropped
            // above in their rows (ADR-441). No exchange after it is logged yet, so the
            // answers they free are no one's to take, and its own entry stands as written.
            if let Some(&earliest) = effect.owners.iter().min() {
                self.refresh_kept_beyond_window(earliest..entry.seq, Place::BeyondRows);
                self.refresh_window_survivors(earliest..entry.seq);
            }
            self.matched.clear();
            self.clears.matched += 1;
            self.last_reset = Some(entry.seq);
            // No answer from before it counts for the exchanges after it, and the requests
            // before it are owed no more, so nothing settles the window's exchanges again.
            if let Some(last) = self.window.back() {
                self.left_window = Some(last.seq);
            }
            self.window.clear();
        } else if unsure_restart {
            self.matched.clear();
            self.clears.matched += 1;
        }
        if effect.sent == Sent::Unknown {
            self.unknown_sends += 1;
        }
        let placed_alone = entry.placed_alone();
        let may_have_arrived = matches!(effect.sent, Sent::Confirmed | Sent::Unknown);
        if let (true, false, Some(sid)) = (may_have_arrived, placed_alone, effect.sid) {
            self.set_owed_rows(entry.seq, sid, &effect.expected, &effect.owed_ids);
        }
    }

    /// The response identifiers exchange `seq` has an owed row on, in the order the rows were set.
    pub(crate) fn owed_row_ids(&self, seq: u64) -> Vec<u32> {
        self.owed
            .iter()
            .filter(|owed| owed.seq == seq)
            .map(|owed| owed.rx_id)
            .collect()
    }

    /// Owed exchange `seq` got a final answer on some of its identifiers and stays owed on the
    /// others (design 8.11 "Owed request", `late_response`): its rows are set again from
    /// `evidence`, which restates the answer, and the log keeps `evidence` with the places the
    /// host read its answers in (`answer_order`, `answer_ends`) and `sent`, so the answers
    /// after it are judged against the answers it has now. Returns the identifiers it is owed on
    /// no more. Nothing for an exchange that is not owed.
    pub(crate) fn settle_on(
        &mut self,
        seq: u64,
        evidence: Evidence,
        sent: Sent,
        transmit: super::timing::At,
        placed: (Vec<usize>, Vec<super::timing::At>),
        verdict: Classification,
    ) -> Vec<u32> {
        let Some(sid) = self
            .owed
            .iter()
            .find(|owed| owed.seq == seq)
            .map(|owed| owed.sid)
        else {
            return Vec::new();
        };
        let before = self.owed_row_ids(seq);
        self.set_owed_rows(seq, sid, &evidence.expected, &evidence.owed_ids());
        let after = self.owed_row_ids(seq);
        self.update_kept(seq, evidence, sent, transmit, None);
        if let Some(kept) = self.kept.get_mut(&seq) {
            (kept.answer_order, kept.answer_ends) = placed;
            kept.partial = Some(kept.partial.map_or(verdict, |had| had.max(verdict)));
        }
        before
            .into_iter()
            .filter(|id| !after.contains(id))
            .collect()
    }

    /// Owed exchange `seq`: its answers' histories are taken again from the owed requests as they
    /// are now (`history` is the entry's), in the evidence the log kept of it (else `fallback`),
    /// and its rows are set again from them. Gives the identifiers it is owed on no more, and
    /// whether none is left, in which case it is released as one placed alone is.
    fn refresh_owed_rows(
        &mut self,
        seq: u64,
        fallback: &Evidence,
        history: &LinkHistory,
    ) -> (Vec<u32>, bool) {
        let Some(sid) = self
            .owed
            .iter()
            .find(|owed| owed.seq == seq)
            .map(|owed| owed.sid)
        else {
            return (Vec::new(), false);
        };
        let mut evidence = self
            .kept
            .get(&seq)
            .map_or_else(|| fallback.clone(), |kept| kept.evidence.clone());
        evidence.set_answer_histories(self.answer_histories(&evidence, history, seq));
        let before = self.owed_row_ids(seq);
        // Only rows go: what it was owed on and still is (a refresh never adds one, since the
        // answers' owners it reads can only have shrunk).
        let had = self.owed_row_ids(seq);
        let ids: Vec<u32> = evidence
            .owed_ids()
            .into_iter()
            .filter(|id| had.contains(id))
            .collect();
        let emptied = ids.is_empty();
        if emptied {
            self.owed.retain(|owed| owed.seq != seq);
            if self
                .kept
                .remove(&seq)
                .is_some_and(|kept| kept.sent == Sent::Unknown)
            {
                self.unknown_sends = self.unknown_sends.saturating_sub(1);
            }
        } else {
            self.set_owed_rows(seq, sid, &evidence.expected, &ids);
            if let Some(kept) = self.kept.get_mut(&seq) {
                kept.evidence = evidence;
            }
        }
        let after = self.owed_row_ids(seq);
        let dropped = before
            .into_iter()
            .filter(|id| !after.contains(id))
            .collect();
        (dropped, emptied)
    }

    /// What a new classification `entry` of the owed exchange `entry.seq` does to the owed requests
    /// and to the link's counts, shared by the window ([`LinkState::reclassify_one`]) and the
    /// kept exchanges beyond it ([`LinkState::refresh_kept_beyond_window`], ADR-439) and the
    /// survivors of an accepted restart ([`LinkState::refresh_window_survivors`], both through
    /// [`LinkState::refresh_row_owners`], ADR-441),
    /// `before` being the exchange's `sent` and whether its answer was a matched answer before. An answer no owed request can own any more is a matched
    /// answer for the exchanges after it, a pending too, as `record` would have counted it; it
    /// confirms the send; a final response is placed with its exchange alone, which is then owed
    /// no more and no owner of theirs; an exchange that stays owed re-derives its rows from its
    /// answers' owners as they are now (ADR-435), and with no row left it is owed no more, like
    /// one placed alone (design 8.11 "Matched answer", "Owed request"). `clears` are the link's
    /// clears before and after the exchange's entry; `place` tells a kept exchange
    /// beyond the window, which has the latter only, so an answer a link error may have followed
    /// is not counted as matched there. `settled`: a final response settled it, so it keeps its
    /// rows and its answer is only freed.
    ///
    /// Gives the answer it frees for the exchanges after it.
    #[expect(clippy::too_many_arguments, reason = "one step shared by two callers")]
    fn free_owner_answer(
        &mut self,
        before: (Sent, bool),
        entry: &ExchangeEntry,
        settled: bool,
        fallback: &Evidence,
        answer_at: AnswerTime,
        clears: (Clears, Clears),
        place: Place,
    ) -> Option<SettledAnswer> {
        let (old_sent, old_matched) = before;
        let seq = entry.seq;
        // An answer no owed request can own any more is a matched answer for the exchanges
        // after it, a pending too, as `record` would have counted it; a final response is placed
        // with its exchange alone, which is then owed no more and no owner of theirs (design
        // 8.11 "Matched answer", "Owed request").
        let mut alone =
            !settled && entry.placed_alone() && self.owed.iter().any(|owed| owed.seq == seq);
        // An answer no owed request can own confirms its send (`sent_of`): it can own a late
        // transmit indication no more.
        if !settled && old_sent == Sent::Unknown && entry.sent == Sent::Confirmed {
            self.unknown_sends = self.unknown_sends.saturating_sub(1);
            if let Some(kept) = self.kept.get_mut(&seq) {
                kept.sent = Sent::Confirmed;
            }
        }
        // An exchange that stays owed re-derives its rows from its answers' owners as they are
        // now: an identifier it was owed on only because an owner could own its final answer
        // there goes when that owner no longer can (ADR-435). With no row left it is owed no
        // more, like one placed alone.
        let mut dropped_rows: Vec<u32> = Vec::new();
        if !settled && !alone && self.owed.iter().any(|owed| owed.seq == seq) {
            let (dropped, emptied) = self.refresh_owed_rows(seq, fallback, &entry.link_history);
            dropped_rows = dropped;
            alone = emptied;
        }
        let now_matched = entry.matched_answer()
            && match place {
                Place::Window | Place::Beyond => !old_matched,
                Place::BeyondRows => false,
                // Counted when the visit releases the exchange (ADR-441), since no later visit
                // can count it then, as a settlement's would, and even when it counted at its
                // record: the restart's recount starts the new epoch over.
                Place::BeyondReleased => alone,
            };
        // The freed answer counts as a matched answer only when its stored time is shown after
        // every late link error that may have come after it, as when it was offered before
        // (ADR-384, ADR-385). In the window an answer is counted when its time passes; beyond
        // it a link error the entry saw after its answer ends it too, as `record` applies it.
        let since = self.since_late_error(entry.seq, answer_at);
        let counts = since && (place == Place::Window || !entry.error_after_answer());
        if now_matched
            && since
            && let Some(kept) = self.kept.get_mut(&seq)
        {
            // Counted once: its final response later is no new matched answer.
            kept.matched = true;
        }
        if alone {
            self.owed.retain(|owed| owed.seq != seq);
            if self
                .kept
                .remove(&seq)
                .is_some_and(|kept| kept.sent == Sent::Unknown)
            {
                self.unknown_sends = self.unknown_sends.saturating_sub(1);
            }
        }
        // Beyond the window a matched answer that does not count (no time shown after a late
        // link error, or a link error after it) changes nothing for the later exchanges and
        // would be offered again at every visit, so it is not emitted.
        let counted = now_matched && counts;
        let emit = match place {
            Place::Window => alone || now_matched || !dropped_rows.is_empty(),
            Place::Beyond | Place::BeyondRows | Place::BeyondReleased => {
                alone || counted || !dropped_rows.is_empty()
            }
        };
        emit.then(|| SettledAnswer {
            seq,
            key: entry.effect.key.clone(),
            newly_answered: false,
            newly_matched: counted,
            final_response: alone,
            // A link error after the answer, or in no known order with it, ended its matched
            // answer, as `record` applies it: the answer comes before.
            clears: if entry.error_after_answer() {
                clears.0
            } else {
                clears.1
            },
            since_error: since,
            settled_ids: dropped_rows,
        })
    }

    /// The entry of the kept exchange `seq` as the live rows' owners read it now, not classified
    /// again: the standing `classification` is the entry's (ADR-368). Only the requests sent
    /// before the exchange can own its answer.
    fn kept_entry(
        &self,
        seq: u64,
        evidence: &Evidence,
        classification: Classification,
    ) -> ExchangeEntry {
        let mut history = self.history_settling(evidence, seq);
        history.owners.retain(|owner| *owner < seq);
        history.owed_matching = history.owners.len() as u32;
        // The entry as the owners are now, set against the evidence as it was: the rows and the
        // kept record change, the classification and the logged history do not (ADR-368).
        let mut judged = evidence.clone();
        let owners = self.answer_histories(&judged, &history, seq);
        judged.set_answer_histories(owners);
        let mut entry = ExchangeEntry::build(seq, judged, history);
        // Not classified again: the standing classification, floor included, is what the
        // entry's reads (an accepted restart, a restart to be unsure of) go by.
        entry.classification = classification;
        entry
    }

    /// The matched answer of the kept exchange `seq` beyond the window, with its key and the
    /// `Clears::matched` epoch it counted in, when it counts as one for the exchanges after it:
    /// the record says so and no link error the record saw came after its answer. A late link
    /// error read later has ended the record's answer already when its stored time does not show
    /// it after the error ([`LinkState::error_after`] clears `Kept::matched`, as it does for a
    /// window exchange, the exchange the error falls after included, ADR-444), so the answer's
    /// time is not tested here.
    fn kept_matched_after(&self, seq: u64) -> Option<(MatchKey, u64)> {
        let kept = self.kept.get(&seq).filter(|kept| kept.matched)?;
        (!kept.evidence.error_after_answer())
            .then(|| (MatchKey::of(&kept.evidence), kept.clears.matched))
    }

    /// The owed exchanges with a `seq` in `seqs` that have left the window and are kept: their
    /// rows are taken again from their answers' owners as they are now, in `seq` order
    /// (ADR-439), and the other steps the window takes for an exchange whose owners changed
    /// follow ([`LinkState::free_owner_answer`]): the send confirmed, the exchange released when
    /// placed alone, its answer counted as a matched answer once. Their classification is not
    /// re-derived (ADR-368): the entry's history is not kept, so the owners are read from the
    /// live rows ([`LinkState::history_settling`], those sent before the exchange). Gives the
    /// answer each adds for the exchanges after it when it drops a row, is owed no more or
    /// turns into a counted matched answer. With `place` [`Place::BeyondRows`] (the survivors of an
    /// accepted restart, whose matched answers are from before it) a matched answer is neither
    /// counted nor noted in the record; with [`Place::BeyondReleased`] (the exchanges after the
    /// restart's) it is counted when the exchange is released, which the restart's recount
    /// from the window cannot do (ADR-441).
    fn refresh_kept_beyond_window(&mut self, seqs: Range<u64>, place: Place) -> Vec<SettledAnswer> {
        let mut beyond: Vec<u64> = self
            .kept
            .keys()
            .copied()
            .filter(|seq| {
                seqs.contains(seq) && !self.window.iter().any(|logged| logged.seq == *seq)
            })
            .collect();
        // In sequence order (ADR-439 decision 1): an exchange's earlier owners are visited, and
        // released, before it, whatever the map's order.
        beyond.sort_unstable();
        let mut freed = Vec::new();
        for seq in beyond {
            let Some(kept) = self.kept.get(&seq) else {
                continue;
            };
            let (clears, answer_at) = (kept.clears, kept.answer_at);
            let before = (kept.sent, kept.matched);
            let (classification, evidence) = (kept.classification, kept.evidence.clone());
            freed.extend(self.refresh_row_owners(
                seq,
                evidence,
                classification,
                before,
                (clears, answer_at),
                place,
            ));
        }
        freed
    }

    /// The owed exchanges in the link's window with a `seq` in `seqs`, which an accepted restart
    /// leaves out of the owners of their answers (ADR-441): their rows are taken again from the
    /// owners that are left, rows only (as [`LinkState::refresh_kept_beyond_window`] with
    /// [`Place::BeyondRows`]), before the
    /// window forgets them or the restart's own exchange settles the later ones. An exchange the
    /// log keeps a record of is read from it, any other from its window entry. Its entry stands
    /// as written. In `seq` order, as the window is.
    fn refresh_window_survivors(&mut self, seqs: Range<u64>) -> Vec<SettledAnswer> {
        let survivors: Vec<u64> = self
            .window
            .iter()
            .map(|logged| logged.seq)
            .filter(|seq| seqs.contains(seq) && self.owed.iter().any(|owed| owed.seq == *seq))
            .collect();
        let mut freed = Vec::new();
        for seq in survivors {
            let Some(logged) = self.window.iter().find(|logged| logged.seq == seq) else {
                continue;
            };
            let (classification, evidence, before, clears, answer_at) = match self.kept.get(&seq) {
                Some(kept) => (
                    kept.classification,
                    kept.evidence.clone(),
                    (kept.sent, kept.matched),
                    kept.clears,
                    kept.answer_at,
                ),
                None => {
                    let old =
                        ExchangeEntry::build(seq, logged.evidence.clone(), logged.history.clone());
                    (
                        logged.classification,
                        logged.evidence.clone(),
                        (old.sent, old.matched_answer()),
                        logged.clears,
                        logged.answer_at,
                    )
                }
            };
            freed.extend(self.refresh_row_owners(
                seq,
                evidence,
                classification,
                before,
                (clears, answer_at),
                Place::BeyondRows,
            ));
        }
        freed
    }

    /// Takes the rows of the owed exchange `seq` again from the owners that are left, with
    /// `evidence` as it was kept and the classification it stands with (not derived again,
    /// ADR-368): the step [`LinkState::refresh_kept_beyond_window`] takes for each exchange, and
    /// [`LinkState::refresh_window_survivors`] for each exchange of the window. `before` is its
    /// `sent` and whether its answer was a matched answer.
    fn refresh_row_owners(
        &mut self,
        seq: u64,
        evidence: Evidence,
        classification: Classification,
        before: (Sent, bool),
        at: (Clears, AnswerTime),
        place: Place,
    ) -> Option<SettledAnswer> {
        let (clears, answer_at) = at;
        let entry = self.kept_entry(seq, &evidence, classification);
        self.free_owner_answer(
            before,
            &entry,
            false,
            &evidence,
            answer_at,
            (clears, clears),
            place,
        )
    }

    /// Sets the rows exchange `seq` is owed on, replacing any it had: one for each identifier in
    /// `owed_ids` ([`Evidence::owed_ids`], ADR-429, ADR-432), once each.
    fn set_owed_rows(&mut self, seq: u64, sid: u8, expected: &Expected, owed_ids: &[u32]) {
        self.owed.retain(|owed| owed.seq != seq);
        let mut done: Vec<u32> = Vec::new();
        for &rx_id in owed_ids {
            if done.contains(&rx_id) {
                continue;
            }
            done.push(rx_id);
            self.owed.push(Owed {
                seq,
                rx_id,
                sid,
                expected: expected.clone(),
            });
        }
    }

    /// Exchange `seq`, still owed after its primitive ended (collecting or not, ADR-431), has
    /// `evidence` and the placed items of it now: it is owed on the identifiers it is owed on in
    /// them (a pending open, or no final answer, ADR-435), and a late item is judged against
    /// them (ADR-429). Its late pendings are in the placed pendings already for a collecting
    /// exchange only; an ordinary exchange keeps them, with their verdicts (ADR-431). Nothing for
    /// an exchange that is not owed.
    pub(crate) fn renew_owed(
        &mut self,
        seq: u64,
        evidence: &Evidence,
        placed: (
            Vec<super::timing::At>,
            Vec<usize>,
            Vec<usize>,
            Vec<super::timing::At>,
        ),
    ) {
        let Some(sid) = self
            .owed
            .iter()
            .find(|owed| owed.seq == seq)
            .map(|owed| owed.sid)
        else {
            return;
        };
        self.set_owed_rows(seq, sid, &evidence.expected, &evidence.owed_ids());
        if let Some(kept) = self.kept.get_mut(&seq) {
            // A collecting exchange's placed pendings include the late ones already, in the
            // evidence's order (`Kept::all_pendings`); an ordinary exchange's are its entry's
            // and the end's own, and its late pendings keep their verdicts (ADR-431).
            if evidence.collecting {
                kept.late_pendings.clear();
                kept.late_places.clear();
            }
            kept.evidence = evidence.clone();
            (
                kept.pendings,
                kept.pending_order,
                kept.answer_order,
                kept.answer_ends,
            ) = placed;
        }
    }

    /// The link's clears now.
    pub(crate) fn clears(&self) -> Clears {
        self.clears
    }

    /// Takes `entry`, logged from `evidence` with the history it was classified with and taken
    /// in by [`LinkState::record`], into the window; `clears` are the link's clears when its
    /// history was taken; `answer_at` is the time of the item that answers it (ADR-385). The
    /// oldest exchange leaves a full window.
    pub(crate) fn log(
        &mut self,
        entry: &ExchangeEntry,
        evidence: Evidence,
        clears: Clears,
        answer_at: AnswerTime,
    ) {
        if self.window.len() == WINDOW
            && let Some(oldest) = self.window.pop_front()
        {
            self.left_window = Some(oldest.seq);
        }
        self.window.push_back(Logged {
            seq: entry.seq,
            history: entry.link_history.clone(),
            origin: evidence.origin,
            request: Request::of(&evidence),
            evidence,
            classification: entry.classification,
            reason: entry.reason.clone(),
            clears,
            after: self.clears,
            floor: None,
            settled: false,
            answer_at,
            matched_at: entry.matched_answer().then(|| {
                if entry.error_after_answer() {
                    clears.matched
                } else {
                    self.clears.matched
                }
            }),
        });
    }

    /// Exchange `seq` keeps at least `classification` whatever its history gives, for `reason`,
    /// which its reason then ends with.
    pub(crate) fn floor(&mut self, seq: u64, classification: Classification, reason: &str) {
        if let Some(logged) = self.window.iter_mut().find(|logged| logged.seq == seq) {
            logged.floor = Some((classification, reason.to_owned()));
        }
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.floor = Some((classification, reason.to_owned()));
        }
    }

    /// Exchange `seq`'s standing classification is now `classification`, which a settling entry
    /// of its own gave it.
    ///
    /// `evidence` is its evidence with what the entry added. A final response settles its
    /// classification for good. Any other end leaves it owed, so it is still reclassified from
    /// its history, keeping at least the classification its settling entry gave it.
    pub(crate) fn settled(
        &mut self,
        seq: u64,
        classification: Classification,
        reason: &str,
        evidence: Evidence,
        final_response: bool,
    ) {
        if let Some(logged) = self.window.iter_mut().find(|logged| logged.seq == seq) {
            if final_response {
                logged.settled = true;
            } else {
                logged.floor = Some((classification, String::new()));
            }
        }
        self.settled_from_evidence(seq, classification, reason, evidence);
    }

    /// [`LinkState::settled`] for a settling entry whose classification its evidence alone gives
    /// (a `late_item`, ADR-370): it sets no floor, so a later item that refutes what it inferred
    /// (a start after a timeout it judged early) can lower it; a floor already set stands.
    pub(crate) fn settled_from_evidence(
        &mut self,
        seq: u64,
        classification: Classification,
        reason: &str,
        evidence: Evidence,
    ) {
        if let Some(logged) = self.window.iter_mut().find(|logged| logged.seq == seq) {
            logged.classification = classification;
            logged.reason = reason.to_owned();
            logged.evidence = evidence;
        }
        // What the entry added gives the exchange an answer, and owners from the owed requests
        // logged before it, as its history would have had them, so its answer is freed once
        // none of them can own it.
        if let Some(at) = self.window.iter().position(|logged| logged.seq == seq) {
            let fresh = self.history_for(&self.window[at].evidence);
            let history = &mut self.window[at].history;
            history.this_exchange = fresh.this_exchange;
            history.owners = fresh
                .owners
                .into_iter()
                .filter(|owner| *owner < seq)
                .collect();
            history.owed_matching = history.owners.len() as u32;
        }
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.classification = classification;
        }
    }

    /// Exchange `seq` is now at least `floor`, with `reason`, which an ambiguous response gave it;
    /// it stays owed and is still reclassified from its history, keeping that floor, whose
    /// reason then ends with `suffix`.
    pub(crate) fn raise(&mut self, seq: u64, floor: Classification, reason: &str, suffix: &str) {
        if let Some(logged) = self.window.iter_mut().find(|logged| logged.seq == seq) {
            if logged.classification < floor {
                logged.classification = floor;
                logged.reason = reason.to_owned();
            }
            // The higher floor stands: an ambiguity never lowers what an earlier settlement or
            // item kept (ADR-433).
            if logged.floor.as_ref().is_none_or(|(had, _)| *had <= floor) {
                logged.floor = Some((floor, suffix.to_owned()));
            }
        }
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.classification = kept.classification.max(floor);
            if kept.floor.as_ref().is_none_or(|(had, _)| *had <= floor) {
                kept.floor = Some((floor, suffix.to_owned()));
            }
        }
    }

    /// Reclassifies the later exchanges in the window with `answer`, which a settling entry adds
    /// to an earlier exchange (design 8.11 "Settlement"):
    /// - one more answered exchange, and one since the last link error when none came between
    ///   them;
    /// - a matched answer for each with the same match, when none of the link's matched answers
    ///   ended between them;
    /// - a final response ends the earlier exchange as an owed request their answers could
    ///   belong to, and an answer that ends it on some identifiers only ends it for the
    ///   answers on those (`settled_ids`).
    ///
    /// Gives each whose classification or reason this changes, in `seq` order, and the largest
    /// `seq` of a later exchange that had left the window, which is not reclassified: its owed
    /// rows still follow the owners it loses (ADR-439).
    pub(crate) fn reclassify_after(
        &mut self,
        answer: &SettledAnswer,
    ) -> (Vec<Reclassified>, Option<u64>) {
        let mut changed: Vec<Reclassified> = Vec::new();
        let mut answers = vec![answer.clone()];
        while let Some(answer) = answers.pop() {
            self.take_answer(&answer);
            // The owed exchanges beyond the window follow the owners they lost in their rows
            // only (ADR-439); each such freed answer comes through this loop in turn.
            answers
                .extend(self.refresh_kept_beyond_window(answer.seq + 1..u64::MAX, Place::Beyond));
            for at in 0..self.window.len() {
                let logged = &self.window[at];
                if logged.seq <= answer.seq {
                    continue;
                }
                let mut history = logged.history.clone();
                if answer.newly_answered {
                    history.answered += 1;
                    if answer.since_error && logged.clears.errors == answer.clears.errors {
                        history.answered_since_error += 1;
                    }
                }
                if answer.newly_matched
                    && logged.clears.matched == answer.clears.matched
                    && MatchKey::of(&logged.evidence) == answer.key
                {
                    history.answered_same_match += 1;
                }
                // An exchange owed on fewer identifiers than before owns the later answers on
                // the others no more: the owner goes when the rows it has now no longer list it.
                if self.frees_owner(&answer, &logged.evidence) {
                    history.owners.retain(|owner| *owner != answer.seq);
                    history.owed_matching = history.owners.len() as u32;
                }
                let (freed, reclassified) = self.reclassify_one(at, history);
                answers.extend(freed);
                if let Some(reclassified) = reclassified {
                    changed.retain(|earlier| earlier.seq != reclassified.seq);
                    changed.push(reclassified);
                }
            }
        }
        changed.sort_by_key(|later| later.seq);
        let not_reclassified = self.left_window.filter(|left| *left > answer.seq);
        (changed, not_reclassified)
    }

    /// Whether `answer`, a settling entry's answer of an earlier exchange, frees that exchange
    /// from owning the answers of the exchange with `evidence`: it was placed alone, or it is
    /// owed on fewer identifiers and its rows no longer list it for those answers.
    fn frees_owner(&self, answer: &SettledAnswer, evidence: &Evidence) -> bool {
        answer.final_response
            || (!answer.settled_ids.is_empty()
                && !self.history_for(evidence).owners.contains(&answer.seq))
    }

    /// Exchange `seq`'s final response, read after its entry, accepts its ECUReset or session
    /// change (`accepting`) or is a positive response the agent cannot use: it is applied as
    /// [`LinkState::record`] would have applied it in time (design 8.11 "Owed request",
    /// "Matched answer", ADR-369).
    ///
    /// - The matched answers from before it end: the exchanges logged after it count their
    ///   matched answers again from the window's exchanges after it and from the kept exchanges
    ///   beyond the window after it whose answer still counts (the ones an accepting visit
    ///   releases, and those that stay kept: [`LinkState::kept_matched_after`]), in both cases.
    /// - Accepting, the owed requests sent before `earliest`, the earliest request its answer
    ///   could belong to, end, and are no owners of the later exchanges' answers; the window
    ///   keeps it and the exchanges after it.
    ///   The owed exchanges from `earliest` up to it, beyond the window and in it, take their
    ///   rows again from the owners that are left (ADR-441), and what they free reaches the
    ///   exchanges after it.
    ///
    /// Gives each later exchange whose classification or reason this changes, in `seq` order,
    /// and the largest `seq` of a later exchange that had left the window, which is not
    /// reclassified: its owed rows still lose the owners before `earliest` (ADR-439).
    pub(crate) fn reset_after(
        &mut self,
        seq: u64,
        earliest: u64,
        accepting: bool,
    ) -> (Vec<Reclassified>, Option<u64>) {
        // The end of the matched answers comes at its entry, before every later one's history.
        // No answer is tested against a late link error here: a late link error ended the
        // answer of a window exchange or a kept record it came after when it was read
        // ([`LinkState::error_after`]), and the shift moves an exchange's `matched_at` and its
        // later exchanges' `clears.matched` together, so an answer it excluded stays excluded
        // (ADR-385).
        self.clears.matched += 1;
        for logged in &mut self.window {
            if logged.seq > seq {
                logged.clears.matched += 1;
                if let Some(at) = &mut logged.matched_at {
                    *at += 1;
                }
            }
            if logged.seq >= seq {
                logged.after.matched += 1;
            }
        }
        for (kept_seq, kept) in &mut self.kept {
            if *kept_seq >= seq {
                kept.clears.matched += 1;
            }
        }
        if accepting {
            self.owed.retain(|owed| owed.seq >= earliest);
            let owed = &self.owed;
            self.kept
                .retain(|kept, _| owed.iter().any(|owed| owed.seq == *kept));
            self.last_reset = self.last_reset.max(Some(seq));
        }
        let mut changed = Vec::new();
        // Exchanges after it whose rows the reset dropped, wholly or in part: no longer owners
        // of the later answers on those identifiers.
        let mut freed_answers: Vec<SettledAnswer> = Vec::new();
        let mut released: Vec<SettledAnswer> = Vec::new();
        if accepting {
            // The owed exchanges that survive it, between `earliest` and it, lose the owners
            // before `earliest` in their rows (ADR-441), the ones beyond the window and then
            // the window's, which `seq` order puts before it; their freed answers reach the
            // exchanges after it as any other.
            freed_answers.extend(self.refresh_kept_beyond_window(earliest..seq, Place::BeyondRows));
            freed_answers.extend(self.refresh_window_survivors(earliest..seq));
            // The owed exchanges beyond the window lose the owners before `earliest` in their
            // rows too (ADR-439); they are not reclassified.
            // One the visit releases brings its matched answer, which the recount from the
            // window below cannot find: it is counted for the exchanges after it as a
            // settlement's would be (ADR-441).
            released = self.refresh_kept_beyond_window(seq + 1..u64::MAX, Place::BeyondReleased);
            freed_answers.extend(released.iter().cloned());
        }
        // The matched answers of the exchanges after it, in its epoch, which the window's
        // `matched_at` does not hold: those the visit released, and the kept ones beyond the
        // window that counted at their record (their epoch shifted above). An exchange the
        // visit released is no longer kept, so none is counted twice (ADR-441).
        let mut beyond: Vec<(u64, MatchKey, u64)> = released
            .iter()
            .filter(|answer| answer.newly_matched)
            .map(|answer| (answer.seq, answer.key.clone(), answer.clears.matched))
            .collect();
        beyond.extend(
            self.kept
                .keys()
                .filter(|kept_seq| {
                    **kept_seq > seq && !self.window.iter().any(|logged| logged.seq == **kept_seq)
                })
                .filter_map(|kept_seq| {
                    let (key, epoch) = self.kept_matched_after(*kept_seq)?;
                    Some((*kept_seq, key, epoch))
                }),
        );
        for at in 0..self.window.len() {
            let logged = &self.window[at];
            if logged.seq <= seq {
                continue;
            }
            let mut history = logged.history.clone();
            let goes: Vec<u64> = freed_answers
                .iter()
                .filter(|freed| self.frees_owner(freed, &logged.evidence))
                .map(|freed| freed.seq)
                .collect();
            history
                .owners
                .retain(|owner| (!accepting || *owner >= earliest) && !goes.contains(owner));
            history.owed_matching = history.owners.len() as u32;
            let key = MatchKey::of(&logged.evidence);
            let (later, epoch) = (logged.seq, logged.clears.matched);
            history.answered_same_match = self
                .window
                .iter()
                .filter(|earlier| {
                    earlier.seq < later
                        && earlier.matched_at == Some(epoch)
                        && MatchKey::of(&earlier.evidence) == key
                })
                .count() as u32;
            history.answered_same_match += beyond
                .iter()
                .filter(|(earlier, earlier_key, at)| {
                    *earlier < later && *at == epoch && *earlier_key == key
                })
                .count() as u32;
            let (freed, reclassified) = self.reclassify_one(at, history);
            if let Some(freed) = freed {
                if freed.newly_matched {
                    self.window[at].matched_at = Some(freed.clears.matched);
                }
                freed_answers.push(freed);
            }
            changed.extend(reclassified);
        }
        self.matched.clear();
        for logged in &self.window {
            if logged.matched_at == Some(self.clears.matched) {
                *self
                    .matched
                    .entry(MatchKey::of(&logged.evidence))
                    .or_insert(0) += 1;
            }
        }
        for (_, key, at) in beyond {
            if at == self.clears.matched {
                *self.matched.entry(key).or_insert(0) += 1;
            }
        }
        let not_reclassified = self.left_window.filter(|left| *left > seq);
        if accepting {
            // No answer from before it counts for the exchanges after it, and the requests
            // before it are owed no more, so nothing settles those exchanges again.
            while let Some(front) = self.window.front()
                && front.seq < seq
            {
                self.left_window = self.left_window.max(Some(front.seq));
                self.window.pop_front();
            }
        }
        (changed, not_reclassified)
    }

    /// Where the VCI time `at` falls among the exchanges the link keeps (design 8.11
    /// `late_item`): within an exchange, from its earliest placed VCI time (its transmit done or
    /// the start of its response) to its latest, on one clock and epoch; else after the latest
    /// exchange shown to start before it. An exchange whose times cannot be set against `at` is
    /// taken as after it; when no exchange the link keeps can be, `at` is not placed. An
    /// exchange with a placed time but no placed start is after the error when its latest time
    /// is before it, else the error may have come during it ([`ErrorPoint::MaybeWithin`]).
    pub(crate) fn place_error(&self, at: (Clock, super::clocks::Placed)) -> ErrorPoint {
        let (clock, at) = at;
        let on_clock = |time: &Option<VciTime>| {
            time.and_then(|time| time.placed())
                .filter(|(own, placed)| *own == clock && placed.epoch == at.epoch)
                .map(|(_, placed)| placed.unwrapped)
        };
        // The latest placed VCI time of an exchange on the error's clock and epoch: its entry's
        // evidence's. Not the stored time of an answer a settling entry added later: such an
        // answer can come after later exchanges began, and the link's exchanges would no longer
        // be sequential in their times (ADR-385).
        let latest = |evidence: &Evidence, _answer: AnswerTime| {
            evidence.times.vci.as_ref().and_then(|vci| {
                [
                    on_clock(&vci.tx_done),
                    on_clock(&vci.rx_start),
                    on_clock(&vci.rx_end),
                    on_clock(&vci.rx_timeout),
                    on_clock(&vci.error),
                ]
                .into_iter()
                .flatten()
                .max()
            })
        };
        let span = |evidence: &Evidence, answer: AnswerTime| {
            let vci = evidence.times.vci.as_ref()?;
            let start = [on_clock(&vci.tx_done), on_clock(&vci.rx_start)]
                .into_iter()
                .flatten()
                .min()?;
            Some((start, latest(evidence, answer).unwrap_or(start)))
        };
        let point = |seq: u64, evidence: &Evidence, answer: AnswerTime| {
            span(evidence, answer)
                .filter(|(start, _)| *start <= at.unwrapped)
                .map(|(_, end)| {
                    if at.unwrapped <= end {
                        ErrorPoint::Within(seq)
                    } else {
                        ErrorPoint::After(seq)
                    }
                })
        };
        if !self
            .window
            .iter()
            .map(|logged| (&logged.evidence, logged.answer_at))
            .chain(
                self.kept
                    .values()
                    .map(|kept| (&kept.evidence, kept.answer_at)),
            )
            .any(|(evidence, answer)| latest(evidence, answer).is_some())
        {
            return ErrorPoint::NotPlaced;
        }
        // An exchange with a time on the clock but no placed start (a single-frame response
        // with no transmit indication has only its end) has no span: the error is after it when
        // it comes after its latest time, else it may have come during it, unless an earlier
        // exchange's latest time is not before the error (the link is sequential, so it came
        // during or before that one) (ADR-384).
        let maybe_within = |seq: u64, evidence: &Evidence, answer: AnswerTime| {
            if span(evidence, answer).is_some() {
                return None;
            }
            let last = latest(evidence, answer)?;
            // With an exchange gone from the window, an error at this exchange's latest time
            // is not shown before its answer either: the answer is not counted after it.
            if at.unwrapped > last || (self.left_window.is_some() && at.unwrapped == last) {
                return Some(ErrorPoint::After(seq));
            }
            // The earlier exchanges' latest times must all be known to compare with: with one
            // gone from the window, the error is not shown to fall within this exchange.
            if self.left_window.is_some() {
                return None;
            }
            let earlier = self
                .window
                .iter()
                .map(|logged| (logged.seq, &logged.evidence, logged.answer_at))
                .chain(
                    self.kept
                        .iter()
                        .map(|(seq, kept)| (*seq, &kept.evidence, kept.answer_at)),
                )
                .filter(|(other, _, _)| *other < seq)
                .filter_map(|(_, evidence, answer)| latest(evidence, answer))
                .max();
            earlier
                .is_none_or(|earlier| earlier < at.unwrapped)
                .then_some(ErrorPoint::MaybeWithin(seq))
        };
        if let Some(found) = self.window.iter().rev().find_map(|logged| {
            point(logged.seq, &logged.evidence, logged.answer_at)
                .or_else(|| maybe_within(logged.seq, &logged.evidence, logged.answer_at))
        }) {
            return found;
        }
        // Before the window: an owed exchange kept beyond it may hold it.
        let oldest = self.window.front().map(|logged| logged.seq);
        let mut kept: Vec<_> = self
            .kept
            .iter()
            .filter(|(seq, _)| oldest.is_none_or(|oldest| **seq < oldest))
            .collect();
        kept.sort_by_key(|(seq, _)| std::cmp::Reverse(**seq));
        let within = kept
            .into_iter()
            .find_map(|(seq, kept)| point(*seq, &kept.evidence, kept.answer_at))
            .and_then(|found| match found {
                ErrorPoint::Within(seq) => Some(seq),
                _ => None,
            });
        ErrorPoint::BeforeWindow { within }
    }

    /// The window's exchanges whose entries were written after the agent's monotonic time
    /// `since_us` on agent epoch `epoch`, oldest first: those a link error whose time cannot be
    /// placed may have come during (design 8.11 `late_item`). Each ended at or after that time,
    /// and an entry is written after its exchange's end is read. A refused exchange sent nothing
    /// a link error could hit.
    pub(crate) fn written_since(&self, epoch: u32, since_us: Option<u64>) -> Vec<u64> {
        self.window
            .iter()
            .filter(|logged| {
                !matches!(
                    logged.evidence.end,
                    End::NotSent(_)
                        | End::VciLost {
                            refused_after_loss: true
                        }
                )
            })
            .filter(|logged| {
                let agent = &logged.evidence.times.agent;
                agent.epoch != epoch || since_us.is_none_or(|since| agent.end_us >= since)
            })
            .map(|logged| logged.seq)
            .collect()
    }

    /// Whether the exchange logged as `seq` is in the window, or kept beyond it.
    pub(crate) fn holds(&self, seq: u64) -> bool {
        self.window.iter().any(|logged| logged.seq == seq) || self.kept(seq).is_some()
    }

    /// The classification the exchange logged as `seq` stands at now, in the window or kept
    /// beyond it.
    pub(crate) fn standing(&self, seq: u64) -> Option<Classification> {
        self.window
            .iter()
            .find(|logged| logged.seq == seq)
            .map(|logged| logged.classification)
            .or_else(|| self.kept(seq).map(|kept| kept.classification))
    }

    /// The evidence the window holds for the exchange logged as `seq`, as the last entry about it
    /// left it.
    #[cfg(test)]
    pub(crate) fn logged_evidence(&self, seq: u64) -> Option<&Evidence> {
        self.window
            .iter()
            .find(|logged| logged.seq == seq)
            .map(|logged| &logged.evidence)
    }

    /// The history the window holds for logged exchange `seq`.
    #[cfg(test)]
    pub(crate) fn logged_history(&self, seq: u64) -> Option<&LinkHistory> {
        self.window
            .iter()
            .find(|logged| logged.seq == seq)
            .map(|logged| &logged.history)
    }

    /// The window's newest exchange.
    pub(crate) fn window_last(&self) -> Option<u64> {
        self.window.back().map(|logged| logged.seq)
    }

    /// A link error read late, on link `link`, came during exchange `seq` (design 8.11
    /// `late_item`): on the exchange's own link, a fault seen in it, else a link error on its physical channel (H3). Its
    /// evidence takes the error, read after its answer, and it is classified again; a final
    /// response that settled it keeps the most severe of its classification and the error's.
    ///
    /// Gives it with its classification and reason, and whether the error changed them. The
    /// error was on link `link`, at a time placed within the exchange (`placed`); one that may
    /// only have come during it is H3 evidence wherever it was.
    ///
    /// `before_answer`: the host read the error before the exchange's final response (ADR-386;
    /// an error read after the exchange's entry is otherwise read after its answer), so the
    /// answer still counts as one since the error, as an error the exchange saw before its
    /// response does ([`LinkState::record`]), when every link error the evidence holds is.
    pub(crate) fn error_in(
        &mut self,
        seq: u64,
        kind: LinkErrorKind,
        link: u32,
        placed: bool,
        before_answer: bool,
    ) -> Option<(Reclassified, bool)> {
        let add = |evidence: &mut Evidence| {
            if placed && evidence.link == link {
                // Read after the exchange's answer, it ends that answer (`record`).
                evidence.link_errors_before_answer = before_answer
                    && (evidence.link_errors.is_empty() || evidence.link_errors_before_answer);
                evidence.link_errors.push(kind);
            } else {
                evidence.channel_link_errors.push(kind);
            }
        };
        if let Some(at) = self.window.iter().position(|logged| logged.seq == seq) {
            let logged = &mut self.window[at];
            add(&mut logged.evidence);
            let mut entry =
                ExchangeEntry::build(seq, logged.evidence.clone(), logged.history.clone());
            if let Some((floor, reason)) = &logged.floor
                && entry.classification < *floor
            {
                entry.classification = *floor;
                entry.reason.push_str(reason);
            }
            if logged.settled && entry.classification < logged.classification {
                entry.classification = logged.classification;
                entry.reason.clone_from(&logged.reason);
            }
            let evidence = logged.evidence.clone();
            let changed =
                entry.classification != logged.classification || entry.reason != logged.reason;
            logged.classification = entry.classification;
            logged.reason.clone_from(&entry.reason);
            let reclassified = Reclassified {
                seq,
                link: logged.evidence.link,
                origin: logged.origin,
                request: logged.request.clone(),
                classification: entry.classification,
                reason: entry.reason,
            };
            if let Some(kept) = self.kept.get_mut(&seq) {
                kept.evidence = evidence;
                kept.classification = reclassified.classification;
            }
            return Some((reclassified, changed));
        }
        // An owed exchange beyond the window, classified again from what is kept of it.
        let mut evidence = self.kept.get(&seq)?.evidence.clone();
        add(&mut evidence);
        let history = self.history_settling(&evidence, seq);
        let answer_histories = self.answer_histories(&evidence, &history, seq);
        evidence.set_answer_histories(answer_histories);
        let mut entry = ExchangeEntry::build(seq, evidence.clone(), history);
        let kept = self.kept.get_mut(&seq)?;
        if let Some((floor, reason)) = &kept.floor
            && entry.classification < *floor
        {
            entry.classification = *floor;
            entry.reason.push_str(reason);
        }
        let changed = entry.classification != kept.classification;
        kept.evidence = evidence;
        kept.classification = entry.classification;
        Some((
            Reclassified {
                seq,
                link: kept.evidence.link,
                origin: kept.origin,
                request: kept.request.clone(),
                classification: entry.classification,
                reason: entry.reason,
            },
            changed,
        ))
    }

    /// A link error read late, logged as line `written`, came on the link's physical channel
    /// after exchange `seq`'s answer (`Some`), or before every exchange the window holds
    /// (`None`), as [`LinkState::record`] would have applied it in time (design 8.11
    /// "Link error", "Matched answer"): the matched answers and the answers since an error from
    /// before it end; the owed requests stay.
    ///
    /// - After `seq`: the exchanges logged after it count their matched answers and their answers
    ///   since an error again from the window's exchanges after `seq`, and from `seq` itself when
    ///   its answer counts by its stored time (ADR-444), and are reclassified.
    /// - Before the window: every exchange the window holds is after the error. The link's
    ///   counts and the window's exchanges' counts start again from the window alone, and the
    ///   exchanges are reclassified, since what they counted from before the window no longer
    ///   counts (ADR-384).
    ///
    /// `time` is the error's placed time when it is placed against this link's exchanges (else
    /// it is kept as one that could not be), which [`LinkState::since_late_error`] uses. An
    /// exchange's answer counts again, as a matched answer and as an answer since the error,
    /// only when its stored time passes that test (ADR-385), the exchange the error falls after
    /// included (ADR-444). Gives
    /// each later exchange whose classification or reason this changes, in `seq` order, and the
    /// largest `seq` of a later exchange it does not reclassify.
    pub(crate) fn error_after(
        &mut self,
        seq: Option<u64>,
        written: u64,
        kind: LinkErrorKind,
        time: Option<(Clock, super::clocks::Placed)>,
    ) -> (Vec<Reclassified>, Option<u64>) {
        self.clears.matched += 1;
        self.clears.errors += 1;
        self.last_link_error = Some((written, kind));
        self.note_late_error(written, time);
        let after = |other: u64| seq.is_none_or(|seq| other > seq);
        // Whether an exchange's answer counts since the error: it is its exchange's own and its
        // stored time is shown after every late link error (ADR-385). The matched answer of one
        // that does not stays out of the new count, which keeps it out of the exchanges after it.
        let counts: Vec<bool> = self
            .window
            .iter()
            .map(|logged| self.counts_since_error(logged))
            .collect();
        // The exchange the error falls after, when its answer counts too: its stored time passes
        // the same test as every other exchange's, and no link error of its own entry came after
        // the answer, the test its record applies ([`LinkState::kept_matched_after`], ADR-444).
        let answers_after: Option<u64> = seq.filter(|seq| {
            self.window.iter().zip(&counts).any(|(logged, counts)| {
                logged.seq == *seq && *counts && !logged.evidence.error_after_answer()
            })
        });
        // An owed exchange whose matched answer stops counting has none counted any more: a
        // later answering item of its own, shown after the error, is a new matched answer.
        let window_ends: Vec<u64> = self
            .window
            .iter()
            .zip(&counts)
            .filter(|(logged, counts)| after(logged.seq) && !**counts)
            .map(|(logged, _)| logged.seq)
            .collect();
        self.end_kept_matched(window_ends);
        for (logged, counts) in self.window.iter_mut().zip(counts) {
            if after(logged.seq) {
                logged.clears.matched += 1;
                logged.clears.errors += 1;
            }
            // An exchange's matched answer moves with the epoch when it counts: a later
            // exchange's, and the answer of the exchange the error falls after (ADR-444). That
            // exchange's own `clears` stay, the epoch its history was taken in.
            if let Some(at) = &mut logged.matched_at
                && ((after(logged.seq) && counts) || answers_after == Some(logged.seq))
            {
                *at += 1;
            }
            // `after` follows the record's `Kept::clears`, which the error shifts from the
            // exchange it falls after on, that exchange included (ADR-444).
            if seq.is_none_or(|seq| logged.seq >= seq) {
                logged.after.matched += 1;
                logged.after.errors += 1;
            }
        }
        // An owed exchange at or after the error's place, in the window or beyond it, may have
        // answered before the error or after it: its answer counts only when its own stored time
        // shows it after the error, for the window's exchanges after it and, for the exchange the
        // error falls after, by the same test (ADR-444). `Kept::matched` means the answer counts
        // in the epoch of the record's clears, which the error shifts for each of them below, so
        // the matched answer ends for each of them whose own time does not show it after the
        // error (ADR-385).
        let oldest = self.window.front().map(|logged| logged.seq);
        let from = seq.or(oldest);
        let kept_ends: Vec<u64> = self
            .kept
            .iter()
            .filter(|(kept_seq, kept)| {
                from.is_some_and(|from| **kept_seq >= from)
                    && !self.since_late_error(**kept_seq, kept.answer_at)
            })
            .map(|(kept_seq, _)| *kept_seq)
            .collect();
        for (kept_seq, kept) in &mut self.kept {
            if from.is_some_and(|from| *kept_seq >= from) {
                kept.clears.matched += 1;
                kept.clears.errors += 1;
            }
        }
        self.end_kept_matched(kept_ends);
        // The `Clears::errors` an exchange's answer counted in: an error it saw after its
        // answer, or in no known order with it, ends that answer (`record`).
        let answered_in = |logged: &Logged| {
            let entry =
                ExchangeEntry::build(logged.seq, logged.evidence.clone(), logged.history.clone());
            if entry.error_after_answer() {
                logged.clears.errors
            } else {
                logged.after.errors
            }
        };
        let mut changed = Vec::new();
        let mut freed_answers: Vec<SettledAnswer> = Vec::new();
        for at in 0..self.window.len() {
            let logged = &self.window[at];
            if !after(logged.seq) {
                continue;
            }
            let mut history = logged.history.clone();
            let goes: Vec<u64> = freed_answers
                .iter()
                .filter(|freed| self.frees_owner(freed, &logged.evidence))
                .map(|freed| freed.seq)
                .collect();
            if !goes.is_empty() {
                history.owners.retain(|owner| !goes.contains(owner));
                history.owed_matching = history.owners.len() as u32;
            }
            let key = MatchKey::of(&logged.evidence);
            let (later, clears) = (logged.seq, logged.clears);
            let since: Vec<&Logged> = self
                .window
                .iter()
                .filter(|earlier| {
                    (after(earlier.seq) || answers_after == Some(earlier.seq))
                        && earlier.seq < later
                })
                .collect();
            history.answered_same_match = since
                .iter()
                .filter(|earlier| {
                    earlier.matched_at == Some(clears.matched)
                        && MatchKey::of(&earlier.evidence) == key
                })
                .count() as u32;
            history.answered_since_error = since
                .iter()
                .filter(|earlier| {
                    self.counts_since_error(earlier) && answered_in(earlier) == clears.errors
                })
                .count() as u32;
            let (freed, reclassified) = self.reclassify_one(at, history);
            if let Some(freed) = freed {
                if freed.newly_matched {
                    self.window[at].matched_at = Some(freed.clears.matched);
                }
                freed_answers.push(freed);
            }
            changed.extend(reclassified);
        }
        // What left the window is past the exchanges reclassified.
        let not_reclassified = match seq {
            Some(seq) => self.left_window.filter(|left| *left > seq),
            None => self.left_window,
        };
        let first = |logged: &&Logged| after(logged.seq) || answers_after == Some(logged.seq);
        self.matched.clear();
        for logged in self.window.iter().filter(first) {
            if logged.matched_at == Some(self.clears.matched) {
                *self
                    .matched
                    .entry(MatchKey::of(&logged.evidence))
                    .or_insert(0) += 1;
            }
        }
        self.answered_since_error = self
            .window
            .iter()
            .filter(first)
            .filter(|logged| {
                self.counts_since_error(logged) && answered_in(logged) == self.clears.errors
            })
            .count() as u32;
        (changed, not_reclassified)
    }

    /// An owed exchange in `seqs` whose answer stops counting after a late link error has no
    /// matched answer counted any more: a later answering item of its own, shown after the
    /// error, is a new matched answer. The one step [`LinkState::error_after`] takes for the
    /// window's exchanges and for the records beyond it.
    fn end_kept_matched(&mut self, seqs: Vec<u64>) {
        for seq in seqs {
            if let Some(kept) = self.kept.get_mut(&seq) {
                kept.matched = false;
            }
        }
    }

    /// Keeps a late link error written as line `written`, placed at `time` or not, for
    /// [`LinkState::since_late_error`]: the answers of the exchanges known when it is read count
    /// since it only when their own time shows it (ADR-384). An
    /// error that is not placed against this link's exchanges may have come after the answer of
    /// any exchange logged before it.
    fn note_late_error(&mut self, written: u64, time: Option<(Clock, super::clocks::Placed)>) {
        let Some((clock, placed)) = time else {
            self.late_error_unplaced = self.late_error_unplaced.max(Some(written));
            return;
        };
        // Every exchange the link knows when the error is read may have answered before it:
        // one placed after the error by its position alone (an exchange with no time on the
        // error's clock is taken as after it) is not shown to have answered after it. Only an
        // exchange logged later, whose request followed the error's read, is (ADR-384). An
        // answer of a known exchange counts since the error only when its own time shows it.
        let through = self
            .window
            .back()
            .map(|logged| logged.seq)
            .into_iter()
            .chain(self.kept.keys().copied())
            .max();
        let Some(through) = through else {
            return;
        };
        match self
            .late_errors
            .iter_mut()
            .find(|error| error.clock == clock && error.epoch == placed.epoch)
        {
            // Merged, the pair can only count fewer answers since an error.
            Some(error) => {
                error.unwrapped = error.unwrapped.max(placed.unwrapped);
                error.through = error.through.max(through);
            }
            None => self.late_errors.push(LateError {
                clock,
                epoch: placed.epoch,
                unwrapped: placed.unwrapped,
                through,
            }),
        }
    }

    /// Whether the answer of a window exchange counts since the last link error: it is its
    /// exchange's own and its stored time passes [`LinkState::since_late_error`] (ADR-385).
    fn counts_since_error(&self, logged: &Logged) -> bool {
        logged.history.this_exchange && self.since_late_error(logged.seq, logged.answer_at)
    }

    /// Whether an answer of exchange `seq`, placed at `time`, counts as an answer since the last
    /// link error: no late link error read after the exchange's entry may have come after it,
    /// or its time is shown after the error's on the error's clock and epoch (design 8.11
    /// `late_item`, ADR-384). An error that could not be placed, or an answer whose time cannot
    /// be set against it, leaves the answer counted as answered only. This is the one place an
    /// answer is compared with a late link error: a settling entry, a freed answer, a recount
    /// after a late error and a re-offer of an answer all ask it with the answer's stored time
    /// (ADR-385). Once it gives false for an answer it stays so, since the errors only merge to
    /// later times and newer exchanges.
    pub(crate) fn since_late_error(&self, seq: u64, time: AnswerTime) -> bool {
        if self
            .late_error_unplaced
            .is_some_and(|written| seq < written)
        {
            return false;
        }
        // An answer shown on an error's clock and epoch counts only when it is after the error,
        // whatever exchange the error was placed after (an exchange with no time on the clock
        // at its entry is placed as after the error, although its answer may be before it);
        // one that cannot be set against the error counts only when the error is not after its
        // exchange.
        self.late_errors.iter().all(|error| match time {
            AnswerTime::At(clock, placed)
                if clock == error.clock && placed.epoch == error.epoch =>
            {
                placed.unwrapped > error.unwrapped
            }
            _ => seq > error.through,
        })
    }

    /// Judges `answer`, which a settling entry adds to an owed exchange, from the time of its
    /// answering item ([`Kept::answer_time`]): it counts since a late link error only when
    /// shown after it ([`LinkState::since_late_error`]). The time is stored with the exchange,
    /// so a later offer of the answer, a recount or a placement judges the same time
    /// (ADR-385).
    pub(crate) fn judge_answer(&mut self, answer: &mut SettledAnswer, time: AnswerTime) {
        // The answering item's time is stored whatever the entry counts, so that a late error
        // read later tests it whichever came first; with no new item it is the stored one
        // (ADR-385).
        if let Some(logged) = self
            .window
            .iter_mut()
            .find(|logged| logged.seq == answer.seq)
        {
            logged.answer_at = time;
        }
        if let Some(kept) = self.kept.get_mut(&answer.seq) {
            kept.answer_at = time;
        }
        // A settling entry that adds no answer has nothing to judge.
        if !answer.newly_answered && !answer.newly_matched {
            return;
        }
        answer.set_since_error(self.since_late_error(answer.seq, time));
    }

    /// The response of owed exchange `seq` no longer counts as started in time: a later start
    /// that it or another exchange could own was read (design 8.11 `late_item`, ADR-384).
    pub(crate) fn forget_late_start(&mut self, seq: u64) {
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.late_start = None;
            kept.late_start_id = None;
        }
    }

    /// Records the identifier the latest late start of owed exchange `seq` was judged on.
    pub(crate) fn set_late_start_id(&mut self, seq: u64, rx_id: Option<u32>) {
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.late_start_id = rx_id;
        }
    }

    /// Classifies the window's exchange at `at` again with `history`, as
    /// [`LinkState::record`] would have taken it in with that history.
    ///
    /// Gives the answer it frees for the exchanges after it, when no owed request can own it any
    /// more, and its new classification when this changes it.
    fn reclassify_one(
        &mut self,
        at: usize,
        history: LinkHistory,
    ) -> (Option<SettledAnswer>, Option<Reclassified>) {
        let logged = &self.window[at];
        // The answers' owners follow the owed requests as they are now.
        let answer_histories = self.answer_histories(&logged.evidence, &history, logged.seq);
        let unchanged = logged
            .evidence
            .answers
            .iter()
            .zip(&answer_histories)
            .all(|(answer, fresh)| answer.history.as_ref() == Some(fresh));
        if history == logged.history && unchanged {
            return (None, None);
        }
        // One a final response settled keeps its classification, but its answer is still freed
        // for the exchanges after it.
        let settled = logged.settled;
        let old = ExchangeEntry::build(logged.seq, logged.evidence.clone(), logged.history.clone());
        let mut evidence = logged.evidence.clone();
        evidence.set_answer_histories(answer_histories.clone());
        let mut entry = ExchangeEntry::build(logged.seq, evidence, history);
        let seq = logged.seq;
        // The floor first: `record` sees the classification the entry was written with.
        if let Some((floor, reason)) = &logged.floor
            && entry.classification < *floor
        {
            entry.classification = *floor;
            entry.reason.push_str(reason);
        }
        let answer_at = self.window[at].answer_at;
        let clears = (self.window[at].clears, self.window[at].after);
        let fallback = self.window[at].evidence.clone();
        let ended = settled && !self.owed.iter().any(|owed| owed.seq == seq);
        let freed = self.free_owner_answer(
            (old.sent, old.matched_answer()),
            &entry,
            ended,
            &fallback,
            answer_at,
            clears,
            Place::Window,
        );
        let logged = &mut self.window[at];
        logged.history = entry.link_history.clone();
        logged.evidence.set_answer_histories(answer_histories);
        if settled
            || (entry.classification == logged.classification && entry.reason == logged.reason)
        {
            return (freed, None);
        }
        logged.classification = entry.classification;
        logged.reason.clone_from(&entry.reason);
        let reclassified = Reclassified {
            seq,
            link: logged.evidence.link,
            origin: logged.origin,
            request: logged.request.clone(),
            classification: entry.classification,
            reason: entry.reason,
        };
        if let Some(kept) = self.kept.get_mut(&seq) {
            kept.classification = reclassified.classification;
        }
        (freed, Some(reclassified))
    }

    /// Takes `answer` into the link's counts, for the exchanges logged after it, as
    /// [`LinkState::record`] would have counted it: one more answered exchange, one more since the
    /// last link error unless one came since, and a matched answer unless the matched answers
    /// ended since.
    fn take_answer(&mut self, answer: &SettledAnswer) {
        if answer.newly_answered {
            self.answered += 1;
            if answer.since_error && answer.clears.errors == self.clears.errors {
                self.answered_since_error += 1;
            }
        }
        if answer.newly_matched && answer.clears.matched == self.clears.matched {
            *self.matched.entry(answer.key.clone()).or_insert(0) += 1;
        }
        // Counted once: an exchange still owed after it (a pending its primitive's end added)
        // brings no new matched answer with its final response.
        if answer.newly_matched
            && let Some(kept) = self.kept.get_mut(&answer.seq)
        {
            kept.matched = true;
        }
        if answer.newly_matched
            && let Some(logged) = self
                .window
                .iter_mut()
                .find(|logged| logged.seq == answer.seq)
        {
            logged.matched_at = Some(answer.clears.matched);
        }
    }

    /// The `seq` and kind of the last link error recorded.
    #[cfg(test)]
    fn last_link_error(&self) -> Option<(u64, LinkErrorKind)> {
        self.last_link_error
    }

    /// The `seq` of the last accepted ECUReset or session change.
    #[cfg(test)]
    fn last_reset(&self) -> Option<u64> {
        self.last_reset
    }

    /// The evidence the window or the kept exchanges hold for the exchange logged as `seq`.
    #[cfg(test)]
    pub(crate) fn evidence_of(&self, seq: u64) -> Option<&Evidence> {
        self.window
            .iter()
            .find(|logged| logged.seq == seq)
            .map(|logged| &logged.evidence)
            .or_else(|| self.kept(seq).map(|kept| &kept.evidence))
    }
}

/// The `request` group of an entry (design 8.11).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<&'static str>,
    pub bytes: LoggedMessage,
    pub suppressed: bool,
}

impl Request {
    /// The `request` group of the exchange `evidence` gives.
    pub(crate) fn of(evidence: &Evidence) -> Self {
        let sid = evidence.sid();
        Self {
            sid: sid.map(|sid| format!("{sid:#04x}")),
            service: sid.and_then(service_name),
            bytes: redact(Direction::Request, &evidence.request),
            suppressed: evidence.suppressed,
        }
    }
}

/// What [`classify`] gives: the classification with its reason, and the entry's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub classification: Classification,
    pub reason: String,
    pub text: String,
}

/// One exchange entry (design 8.11, "Exchange entry"). The key set follows the table there;
/// `pending_visibility` is a key of its own, as the example logs show it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExchangeEntry {
    event: &'static str,
    pub seq: u64,
    pub target: Target,
    pub link: u32,
    pub origin: Origin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<At>,
    pub request: Request,
    pub sent: Sent,
    pub sent_evidence: SentEvidence,
    /// The exchange whose response caused a refusal (`not_sent`, `earlier_response`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<u64>,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<AnswerItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_lost: Option<EventsLost>,
    pub pending: Vec<Pending>,
    pub pending_visibility: PendingVisibility,
    pub timing: Timing,
    pub times: Times,
    pub link_history: LinkHistory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supply_voltage: Option<SupplyVoltage>,
    pub text: String,
    pub classification: Classification,
    pub reason: String,
    pub detail: Detail,
    #[serde(skip)]
    effect: LinkEffect,
}

impl ExchangeEntry {
    /// The link error the exchange saw, on its own link or another link of its channel, which
    /// ends the matched answers of the channel's other targets too ([`LinkState::record`]).
    pub(crate) fn link_error(&self) -> Option<LinkErrorKind> {
        self.effect.link_error
    }

    /// Whether the exchange has a final response and no identifier open on a pending or silent
    /// ([`LinkEffect::settles`], ADR-435).
    pub(crate) fn settles(&self) -> bool {
        self.effect.settles()
    }

    /// The answer this entry, a settling entry's view of the earlier exchange `kept` with its new
    /// evidence, adds for the later exchanges on its link ([`LinkState::reclassify_after`]),
    /// counted as [`LinkState::record`] counts an exchange's answer. `ends_owed` must be true only
    /// when the caller ends the exchange's owed request; it becomes
    /// [`SettledAnswer::final_response`], which `frees_owner` reads to free the exchange from
    /// later owners.
    pub(crate) fn settled_answer(&self, kept: &Kept, ends_owed: bool) -> SettledAnswer {
        let effect = &self.effect;
        let before = &kept.evidence;
        SettledAnswer {
            seq: self.seq,
            key: effect.key.clone(),
            newly_answered: effect.this_exchange
                && before.answer_head().is_none()
                && !before.response_started,
            newly_matched: self.matched_answer() && !kept.matched,
            final_response: ends_owed,
            clears: kept.clears,
            since_error: true,
            settled_ids: Vec::new(),
        }
    }

    /// Whether this entry, a settling entry's view of its earlier exchange, ends the evidence from
    /// before that exchange ([`LinkState::reset_after`]): `Some((true, earliest))` when its final
    /// response accepts the exchange's ECUReset or session change, `earliest` being the earliest
    /// request the response could belong to; `Some((false, _))` when the response is positive
    /// but unusable.
    pub(crate) fn late_restart(&self) -> Option<(bool, u64)> {
        let effect = &self.effect;
        let accepting = effect.positive && effect.final_response;
        (effect.restarts_ecu && (accepting || effect.unusable)).then(|| {
            let earliest = effect.owners.iter().copied().fold(self.seq, u64::min);
            (accepting, earliest)
        })
    }

    /// Whether it accepts an ECUReset or a session change, as [`LinkState::record`] reads it.
    fn accepting(&self) -> bool {
        let effect = &self.effect;
        effect.restarts_ecu
            && ((effect.positive && effect.final_response)
                || (effect.suppressed && self.classification == Classification::None))
    }

    /// Whether its answer is a matched answer, as [`LinkState::record`] counts it.
    pub(crate) fn matched_answer(&self) -> bool {
        let effect = &self.effect;
        let unsure_restart = effect.unsure_restart(self.classification);
        effect.this_exchange
            && effect.bound_answer
            && !self.accepting()
            && !unsure_restart
            && effect.owners.is_empty()
    }

    /// Whether a link error it saw came after its answer, or in no known order with it, so it
    /// ended that answer's matched answer ([`LinkState::record`]); [`Evidence::error_after_answer`]
    /// reads the same off the evidence.
    fn error_after_answer(&self) -> bool {
        self.effect.link_error.is_some() && !self.effect.answer_after_error
    }

    /// Whether its answer is placed with it alone, so it is owed nothing, as
    /// [`LinkState::record`] reads it.
    fn placed_alone(&self) -> bool {
        self.effect.owners.is_empty() && (self.accepting() || self.effect.settles())
    }

    /// Whether it is owed only because an owner could own its final answer: it settles, and an
    /// earlier owed request still stands that could own that answer (design 8.11, ADR-442). The
    /// log keeps its record for its rows until the owners let go. An exchange owed on no
    /// identifier has no row to keep, whoever could own an answer of it (ADR-445 decision 2).
    pub(crate) fn owed_through_owners(&self) -> bool {
        self.effect.settles()
            && !self.effect.owed_ids.is_empty()
            && self.effect.owners.iter().any(|owner| *owner < self.seq)
    }

    /// The entry for `evidence` as line `seq`, with `history` from
    /// [`LinkState::history_for`], classified by [`classify`].
    pub(crate) fn build(seq: u64, evidence: Evidence, history: LinkHistory) -> Self {
        let (sent, sent_evidence) = sent_of(&evidence, &history);
        let verdict = classify(&evidence, &history);
        let answers = answer_items(&evidence, &history);
        let sid = evidence.sid();
        let effect = LinkEffect {
            key: MatchKey::of(&evidence),
            sid,
            expected: evidence.expected.clone(),
            sent,
            this_exchange: history.this_exchange,
            bound_answer: evidence.bound_answer(),
            owners: history.owners.clone(),
            final_response: evidence.final_response().is_some() && !evidence.end_rejected,
            settles_owed: evidence.settles_owed(),
            owed_ids: evidence.owed_ids(),
            positive: matches!(evidence.end, End::Positive(_)),
            unusable: matches!(evidence.end, End::PositiveUnusable(_)),
            suppressed: evidence.suppressed,
            restarts_ecu: restarts_ecu(&evidence.request),
            link_error: evidence.first_link_error(),
            answer_after_error: evidence.answer_after_error(),
        };
        let request = Request::of(&evidence);
        Self {
            event: EVENT,
            seq,
            target: evidence.target,
            link: evidence.link,
            origin: evidence.origin,
            at: evidence.at,
            request,
            sent,
            sent_evidence,
            caused_by: evidence.caused_by,
            outcome: match &evidence.end {
                End::PositiveUnusable(bytes) if evidence.end_rejected => {
                    Outcome::PositiveUnusable {
                        response: redact_unanswering(bytes),
                    }
                }
                end => Outcome::of(end),
            },
            answers,
            rejected: evidence
                .rejected
                .into_iter()
                .map(|rejected| RejectedItem {
                    rx_id: rejected.rx_id,
                    response: redact_unanswering(&rejected.bytes),
                    vci_us: rejected.vci_us,
                    clock: rejected.clock,
                    epoch: rejected.epoch,
                    agent_us: rejected.agent_us,
                    reason: rejected.reason,
                    late_response: rejected.late_response,
                })
                .collect(),
            events_lost: evidence.events_lost,
            pending: evidence.pending,
            pending_visibility: evidence.pending_visibility,
            timing: evidence.timing,
            times: evidence.times,
            link_history: history,
            supply_voltage: evidence.supply_voltage,
            text: verdict.text,
            classification: verdict.classification,
            reason: verdict.reason,
            detail: evidence.detail,
            effect,
        }
    }
}

/// `sent` and what set it, from the evidence (design 8.11, `sent`):
/// - `not_sent` by a refusal of the agent, or of the worker after a lost VCI (ADR-294);
/// - `confirmed` by a transmit confirmation, or by an answer of the ECU that no owed request
///   could own;
/// - `partial` by a flow-control frame of the ECU, when the exchange ended without any response
///   of the ECU (no final response, no pending, no answer): an exchange that got one is not
///   `partial`;
/// - otherwise `unknown`.
pub(crate) fn sent_of(evidence: &Evidence, history: &LinkHistory) -> (Sent, SentEvidence) {
    if matches!(
        evidence.end,
        End::NotSent(_)
            | End::VciLost {
                refused_after_loss: true
            }
    ) {
        return (Sent::NotSent, SentEvidence::Refused);
    }
    if let Some(transmit) = evidence.transmit_evidence {
        return (Sent::Confirmed, transmit.into());
    }
    if history.this_exchange && history.owed_matching == 0 {
        return (Sent::Confirmed, SentEvidence::Answer);
    }
    if evidence.flow_control && !history.this_exchange && evidence.answers.is_empty() {
        return (Sent::Partial, SentEvidence::FlowControl);
    }
    (Sent::Unknown, SentEvidence::None)
}

/// The `sent` value of an exchange that ended as `end` with nothing else known, for a test
/// that holds another module's classification to the log's.
#[cfg(test)]
pub(crate) fn sent_of_end(request: &[u8], end: End) -> Sent {
    let evidence = Evidence::new(
        Origin::Procedure,
        Target::Endpoint {
            link: 1,
            tx_id: 0x7E0,
            rx_id: 0x7E8,
        },
        1,
        0x7E8,
        request.to_vec(),
        end,
    );
    sent_of(&evidence, &LinkHistory::empty()).0
}

/// The entry's text: one sentence in vehicle-communication terms.
pub(crate) fn text(evidence: &Evidence, history: &LinkHistory) -> String {
    let (sent, sent_evidence) = sent_of(evidence, history);
    if let End::NotSent(cause) = &evidence.end {
        return format!("Not sent: {}.", cause.phrase(evidence.caused_by));
    }
    let sent_phrase = match (sent, sent_evidence) {
        (Sent::Confirmed, SentEvidence::Answer) => {
            "Sent on the bus (the ECU's answer shows it arrived)"
        }
        (Sent::Confirmed, SentEvidence::TxIndication) => "Sent on the bus (transmit confirmed)",
        (Sent::Confirmed, SentEvidence::Loopback) => "Sent on the bus (its echo was seen)",
        (Sent::Confirmed, _) => "Sent on the bus (its transmit time was reported)",
        (Sent::Partial, _) => "The first frame of the request went out",
        (Sent::NotSent, _) => "Not sent",
        (Sent::Unknown, _) => "It is not known whether the request reached the bus",
    };
    let mut outcome = outcome_phrase(evidence);
    if evidence.pending_seen() {
        outcome.push_str(&format!(
            ", after {} response-pending message(s) (0x78)",
            evidence.pending.len()
        ));
        if let Some(place) = evidence.timing.late_pending {
            outcome.push_str(&format!(
                ", pending {} of them {} ms after the limit in force before it",
                place + 1,
                by_ms(&evidence.timing)
            ));
        }
    }
    format!("{sent_phrase}; {outcome}.")
}

/// What the response that ended the wait of an exchange whose end the log placed with earlier
/// exchanges (`Evidence::end_placed`) was, as a noun phrase the timing rows and the placed row
/// share (ADR-447): it names the `late_response` line and says the request's own answer is not
/// decided. Call it only when `end_placed` is `Some`.
fn placed_phrase(evidence: &Evidence) -> String {
    let placed = evidence.end_placed().unwrap_or_default();
    let cut =
        matches!(&evidence.end, End::PositiveUnusable(bytes) if evidence.own_cut_short(bytes));
    if cut {
        format!(
            "a response that is only the beginning of this request's positive response; the log placed it with the earlier exchange or exchanges that could own it as well (the late response logged as {placed}), so it could be this request's own response cut short or the response of an earlier exchange: whether the ECU answered this request is not decided"
        )
    } else {
        format!(
            "a response that does not begin as this request's positive response does; the log placed it with the earlier exchange or exchanges that could own it (the late response logged as {placed}), so it is no answer to this request: whether the ECU answered this request is not decided"
        )
    }
}

/// How the exchange ended, as the entry's text and a late pending's reason name it.
fn outcome_phrase(evidence: &Evidence) -> String {
    if evidence.end_placed().is_some() {
        return format!("it took {}", placed_phrase(evidence));
    }
    match &evidence.end {
        End::Positive(_) => {
            let timing = match evidence.timing.window {
                Window::NotJudged if !evidence.collecting => " (its time is not judged)".to_owned(),
                // A late pending's window names the pending, not the final response.
                Window::Late if evidence.timing.late_pending.is_none() => {
                    format!(" {} ms after the limit in force", by_ms(&evidence.timing))
                }
                _ => String::new(),
            };
            format!("the ECU responded positively{timing}")
        }
        End::PositiveUnusable(_) => {
            "the ECU's response cannot be used: it does not match what the definition expects"
                .to_owned()
        }
        End::Negative(response) => match negative_code(response) {
            Some(code) => match nrc_name(code) {
                Some(name) => format!("the ECU refused the request: {name} ({code:#04x})"),
                None => format!("the ECU refused the request with code {code:#04x}"),
            },
            None => "the ECU sent a negative response without a code".to_owned(),
        },
        End::NoResponse => match (evidence.timing.window, evidence.timing.p2_ms) {
            (Window::TimeoutAtLimit, Some(p2)) if !evidence.pending_seen() => {
                format!("no response within P2 ({p2} ms)")
            }
            (Window::TimeoutAtLimit, _) => "no response within the limit in force".to_owned(),
            (Window::TimeoutEarly, _) => format!(
                "the VCI stopped waiting {} ms before the limit in force",
                by_ms(&evidence.timing)
            ),
            (Window::TimeoutLate, _) => format!(
                "the VCI stopped waiting {} ms after the limit in force",
                by_ms(&evidence.timing)
            ),
            _ => "no response came".to_owned(),
        },
        End::ResponseIncomplete => "the ECU began a response and it did not complete".to_owned(),
        End::VciDidNotEnd => {
            "the agent stopped waiting before the VCI ended the exchange".to_owned()
        }
        End::LinkError(kind) => format!("the link reported {}", kind.phrase()),
        End::NotSent(cause) => format!("not sent: {}", cause.phrase(evidence.caused_by)),
        End::VciLost {
            refused_after_loss: true,
        } => "the VCI had been lost before the request, so the link refused it".to_owned(),
        End::VciLost { .. } => {
            "the connection to the VCI was lost while waiting for the response".to_owned()
        }
        End::Unknown(cause) => format!("the diagnostic system failed: {}", cause.phrase()),
    }
}

/// The entry's classification, reason and text: the first row of design 8.11's table that
/// matches, the most severe with a link error the exchange's own link reported during it, and
/// with `undetermined` when the exchange's answer could belong to an owed request (the log
/// never decides between possible owners).
///
/// The rows are in the table's order. A row that today's evidence cannot reach is named by a
/// comment where it would stand; evidence that only such a row could classify (a window
/// `timeout_early` or `timeout_late` on a response, a window `late` on a missing
/// response) takes the last row at the place where that row stands, so a row before it (a
/// refusal, a lost VCI, a failure of the system, a link error) still decides.
pub(crate) fn classify(evidence: &Evidence, history: &LinkHistory) -> Verdict {
    let mut owed_handled = false;
    let (mut classification, mut reason) = row(evidence, history, &mut owed_handled);
    for kind in &evidence.link_errors {
        let (fault, fault_reason) = kind.judge();
        classification = classification.max(fault);
        reason.push_str(&format!(
            " A link error also came during the exchange ({}), classified {}: {}",
            kind.phrase(),
            severity_word(fault),
            fault_reason
        ));
    }
    // The rows of the answers weigh the owed requests against their answers themselves.
    if !owed_handled {
        (classification, reason) = owed_tail(evidence, history, classification, reason);
    }
    Verdict {
        classification,
        reason,
        text: text(evidence, history),
    }
}

/// An answer that could belong to an owed request leaves the classification at least
/// `undetermined`: the log never decides between the possible owners. The one place that says so;
/// the rows of the answers (`collecting_row`, `several_answers_row`) weigh their answers against
/// the owed requests and call it themselves.
fn owed_tail(
    evidence: &Evidence,
    history: &LinkHistory,
    mut classification: Classification,
    mut reason: String,
) -> (Classification, String) {
    if history.this_exchange && history.owed_matching > 0 {
        classification = classification.max(Classification::Undetermined);
        reason.push_str(&format!(
            " The answer could also belong to {} earlier request(s) that never got a final response, and the log does not decide between them.",
            history.owed_matching
        ));
        if sent_of(evidence, history).0 != Sent::Confirmed {
            reason.push_str(" So it is not known whether this request reached the bus.");
        }
    }
    (classification, reason)
}

/// How a forwarded pending read after its exchange's entry stood against the limit in force
/// before it (ADR-371).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LatePending {
    /// Within the limit less the margin: the limit in force then runs from it.
    InTime,
    /// After the limit plus the margin: the VCI had ended its wait before it, so it carries no
    /// limit on, and a later response is judged against the limit before it.
    Late,
    /// Neither is shown: the limit in force after it is not known.
    Open,
}

impl Kept {
    /// The pendings the limit in force for a later late item or response runs from, in the
    /// evidence's order: its entry's and the end's own, and each in-time late pending; an open
    /// one leaves the limit after it unknown, and a late one is passed over.
    pub(crate) fn limit_pendings(&self) -> Vec<super::timing::At> {
        self.merged_pendings(|window, at| match window {
            LatePending::InTime => Some(at),
            LatePending::Open => Some(None),
            LatePending::Late => None,
        })
    }

    /// Every pending the exchange has, its entry's, the end's own and the late ones, placed in
    /// the order of `Evidence::pending`.
    pub(crate) fn all_pendings(&self) -> Vec<super::timing::At> {
        self.merged_pendings(|_, at| Some(at))
    }

    /// [`Kept::pendings`] with the late ones `keep` gives, each after the pendings its place
    /// says came before it.
    fn merged_pendings(
        &self,
        keep: impl Fn(LatePending, super::timing::At) -> Option<super::timing::At>,
    ) -> Vec<super::timing::At> {
        let mut merged = Vec::with_capacity(self.pendings.len() + self.late_pendings.len());
        let mut late = self.late_pendings.iter().zip(&self.late_places).peekable();
        for (index, pending) in self.pendings.iter().enumerate() {
            while let Some(((at, window), _)) = late.next_if(|(_, place)| **place <= index) {
                merged.extend(keep(*window, *at));
            }
            merged.push(*pending);
        }
        for ((at, window), _) in late {
            merged.extend(keep(*window, *at));
        }
        merged
    }

    /// The exchange's evidence with `response` as its end, judged by the rows that do not turn
    /// on timing: its window is taken as within the limit. For a collecting exchange the
    /// response is restated as an answer of its list, as the log restates a final answer read
    /// after the entry (ADR-425 decision 4, ADR-429): it folds the last pending of the
    /// identifier it came on into it, and the end is the first standing final answer.
    pub(crate) fn answered(&self, response: &[u8], late: &LateAnswer) -> Evidence {
        self.answered_placed(response, late, None).0
    }

    /// [`Kept::answered`], with the places the host read the folded answers in and where each
    /// ended (as [`Kept::answer_order`], [`Kept::answer_ends`]): the answer it adds is read last
    /// (after the late pendings), ending at `end`, and a pending it folds leaves both lists with
    /// it, so a later late item is judged against the answers as they stand (ADR-426, ADR-433).
    pub(crate) fn answered_placed(
        &self,
        response: &[u8],
        late: &LateAnswer,
        end: super::timing::At,
    ) -> (Evidence, Vec<usize>, Vec<super::timing::At>) {
        let negative = &self.evidence.expected.negative_pattern;
        let end_of = if !negative.is_empty() && response.starts_with(negative) {
            End::Negative(response.to_vec())
        } else {
            End::Positive(response.to_vec())
        };
        let mut timing = self.evidence.timing.clone();
        timing.window = Window::WithinP2;
        timing.window_by_ms = None;
        timing.not_judged_reason = None;
        timing.late_pending = None;
        if !self.evidence.collecting {
            // A final answer delivered late is a usable response, so a rejected end of the entry
            // is not rejected any more (ADR-443, ADR-435 decision 7).
            let mut restated = self.evidence.ended_as(end_of, timing);
            restated.end_rejected = false;
            return (
                restated,
                self.answer_order.clone(),
                self.answer_ends.clone(),
            );
        }
        let expected = &self.evidence.expected;
        // The identifier the response is keyed on, as the host keys an answer that names none
        // (`response_id`): the only identifier of a one-ECU table, else the link's own. On a
        // larger table it is 0, unless exactly one identifier is unanswered (silent, or with a
        // pending open), which the response is then taken to answer: keyed on it, it folds that
        // identifier's pending and covers it (ADR-433 decision 6, ADR-435); an identifier owed
        // only because an earlier exchange could own its answer does not count, and with several
        // unanswered it settles none.
        let own_id = late.rx_id.unwrap_or(match self.evidence.receive_ids[..] {
            [] => self.evidence.rx_id,
            [only] => only,
            _ => match self.evidence.unanswered_ids()[..] {
                [unanswered] => unanswered,
                _ => 0,
            },
        });
        let mut answers = self.evidence.answers.clone();
        let mut order = self.answer_order.clone();
        order.resize(answers.len(), usize::MAX);
        let mut ends = self.answer_ends.clone();
        ends.resize(answers.len(), None);
        // After every item the exchange holds, the late pendings (placed by `late_window`)
        // included.
        let base = self
            .pending_order
            .iter()
            .chain(&order)
            .filter(|&&place| place != usize::MAX)
            .max()
            .map_or(0, |last| last + 1);
        order.push(base + self.late_pendings.len());
        ends.push(end);
        answers.push(Answer {
            rx_id: own_id,
            end: end_of.clone(),
            vci_us: late.vci_us,
            clock: late.clock,
            epoch: late.epoch,
            start_us: None,
            tx_done_us: None,
            agent_us: late.agent_us,
            pending: Vec::new(),
            timing: timing.clone(),
            history: None,
        });
        // The one generic fold (ADR-425 decision 4): the pending of the same identifier leaves
        // the list and is the final answer's pending chain.
        // With the covering fold of an unnamed pending (ADR-433).
        let standing = standing_answers_covering(
            &answers,
            |a| a.rx_id,
            |a| a.is_pending(expected),
            Answer::is_rejected,
            &self.evidence.receive_ids,
        );
        let mut chain: Option<Vec<Pending>> = None;
        let mut folded = Vec::with_capacity(answers.len());
        let mut folded_order = Vec::with_capacity(answers.len());
        let mut folded_ends = Vec::with_capacity(answers.len());
        for (((answer, standing), place), ended) in
            answers.into_iter().zip(standing).zip(order).zip(ends)
        {
            if standing {
                folded.push(answer);
                folded_order.push(place);
                folded_ends.push(ended);
            } else {
                chain.get_or_insert_with(|| answer.as_exchange(expected).1);
            }
        }
        if let Some(last) = folded.last_mut() {
            last.pending = chain.unwrap_or_default();
        }
        let (end, end_rejected) = collecting_end(&folded, expected);
        // The window's end stays judged by the entry's timing (`Evidence::collecting`), which a
        // late answer does not change; only the answer it adds is taken as within the limit. A
        // late start judged at the limit showed that end was the VCI's at the limit.
        let mut window = self.evidence.timing.clone();
        if matches!(
            self.late_start_for(late.rx_id),
            Some(LateStart::Late { at_limit: true, .. })
        ) {
            window.window = Window::TimeoutAtLimit;
            window.not_judged_reason = None;
        }
        let mut evidence = self.evidence.ended_as(end, window);
        // Derived from the folded list as the entry derives it: a rejected result stands for the
        // end only while no usable answer does (ADR-359, ADR-437).
        evidence.end_rejected = end_rejected;
        evidence.answers = folded;
        (evidence, folded_order, folded_ends)
    }
}

/// The item of a late response that [`Kept::answered`] restates as an answer of a collecting
/// exchange: the identifier it names (`None` when it names none) and its times, as the host
/// gives an answer's.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LateAnswer {
    pub rx_id: Option<u32>,
    pub vci_us: Option<u64>,
    pub clock: Option<Clock>,
    pub epoch: Option<u32>,
    pub agent_us: u64,
}

/// The end of the reason of an exchange that a late final answer settled on some identifiers
/// only, which keeps `classification` as the least its later final answers give (ADR-433).
pub(crate) fn per_identifier_floor(classification: Classification) -> String {
    format!(
        " An earlier final answer of the exchange left it at {}, which stands.",
        severity_word(classification)
    )
}

/// The classification and reason a `late_response` gives the exchange it settles (design 8.11
/// "Settlement", ADR-365): the response, which came after the VCI ended that exchange, judged
/// by `window` against its limit in force. `answered` is [`Kept::answered`], and `history` the
/// link's history for it as of the settling entry, and `response` the late response's bytes.
pub(crate) fn settle_late_response(
    kept: &Kept,
    answered: &Evidence,
    window: super::timing::LateWindow,
    history: &LinkHistory,
    response_id: Option<u32>,
    response: &[u8],
) -> (Classification, String) {
    use super::timing::LateWindow;
    use Classification::{System, Undetermined, Vehicle};
    let base = classify(answered, history);
    let late_start = kept.late_start_for(response_id);
    // A start of the response read late (ADR-370): one before the limit less the margin means
    // the response started in time, so the system missed it, but only for a response that
    // spans frames, since one that fits one frame came after a transfer the ECU aborted; one
    // after the limit plus the margin means the ECU started late, which any response completing
    // after it is judged as (design 8.11 "Settlement").
    // It is the late response's own bytes: a collecting exchange's end may be another
    // identifier's final answer.
    let spans_frames = response.len() > super::SINGLE_FRAME_MAX;
    let window = match (window, late_start) {
        (LateWindow::Late { .. } | LateWindow::Open, Some(LateStart::InTime { at_us }))
            if spans_frames =>
        {
            return started_in_time(at_us);
        }
        (LateWindow::Open, Some(LateStart::Late { by_us, .. })) => LateWindow::Late { by_us },
        (window, _) => window,
    };
    let at_limit = kept.evidence.timing.window == Window::TimeoutAtLimit
        || matches!(late_start, Some(LateStart::Late { at_limit: true, .. }));
    let (classification, mut reason) = match window {
        LateWindow::InTime { at_us } => (
            System,
            format!(
                "The ECU's response came {:.1} ms after the transmit, within the limit in force less the margin, and was delivered after the exchange had ended: the system missed a response that came in time.",
                at_us as f64 / 1000.0
            ),
        ),
        LateWindow::Late { by_us } => {
            let late = format!(
                "The ECU responded at least {:.1} ms after the limit in force.",
                by_us as f64 / 1000.0
            );
            if base.classification >= Undetermined {
                (base.classification, format!("{late} {}", base.reason))
            } else if at_limit && kept.evidence.absorbing() {
                // A pending the VCI absorbed would have put the limit off (ADR-376).
                (
                    Undetermined,
                    format!(
                        "{late} The VCI ended its wait at P2, but it absorbs pendings: after a pending it absorbed it should have waited longer ({}); the two give the same times (H4).",
                        severity_word(System)
                    ),
                )
            } else if at_limit {
                (
                    Vehicle,
                    format!("{late} The VCI had ended its wait at the limit."),
                )
            } else {
                // The VCI's timeout was not judged at the limit (on a link without
                // start-of-message indications it is not judged at all): it may have ended its
                // wait early, before the response began.
                (
                    Undetermined,
                    format!(
                        "{late} Whether the VCI ended its wait at the limit, or before, is not shown."
                    ),
                )
            }
        }
        LateWindow::Open => (
            base.classification.max(Undetermined),
            format!(
                "A response was delivered after the exchange ended, and its time does not show whether the ECU answered within the limit in force. With it as the exchange's response: {}",
                base.reason
            ),
        ),
    };
    // What an earlier late final answer of the exchange, on another identifier, was judged
    // stands: the exchange's classification is the most severe of its per-identifier verdicts
    // (design 8.11 `late_response`, ADR-433).
    if let Some(partial) = kept.partial
        && classification < partial
    {
        reason.push_str(&per_identifier_floor(partial));
        return (partial, reason);
    }
    // A fault of the system the exchange's entry observed stands (design 8.11 "Settlement").
    if kept.classification == System && classification != System {
        reason.push_str(
            " The exchange's own entry had observed a fault of the system, which stands.",
        );
        return (System, reason);
    }
    (classification, reason)
}

/// What a forwarded pending read after its exchange's entry gives that exchange (design 8.11
/// "Settlement" `late_item`, ADR-371), judged by `window` against the limit in force before it,
/// with `base` and `base_reason`, what the exchange's own rows give it, as
/// [`settle_late_response`] judges a late response.
pub(crate) fn settle_late_pending(
    kept: &Kept,
    window: super::timing::LateWindow,
    base: Classification,
    base_reason: &str,
    pending_id: Option<u32>,
) -> (Classification, String) {
    use super::timing::LateWindow;
    use Classification::{System, Undetermined, Vehicle};
    match window {
        LateWindow::InTime { at_us } => (
            System,
            format!(
                "The ECU answered with a pending {:.1} ms after the transmit, within the limit in force less the margin, and it reached the agent after the exchange had ended: the system missed an answer that came in time.",
                at_us as f64 / 1000.0
            ),
        ),
        LateWindow::Late { by_us } => {
            let late = format!(
                "The ECU answered with a pending at least {:.1} ms after the limit in force plus the margin.",
                by_us as f64 / 1000.0
            );
            let at_limit = kept.evidence.timing.window == Window::TimeoutAtLimit
                || matches!(
                    kept.late_start_for(pending_id),
                    Some(LateStart::Late { at_limit: true, .. })
                );
            if base >= Undetermined {
                (base, format!("{late} {base_reason}"))
            } else if at_limit {
                (
                    Vehicle,
                    format!("{late} The VCI had ended its wait at the limit."),
                )
            } else {
                (
                    Undetermined,
                    format!(
                        "{late} Whether the VCI ended its wait at the limit, or before, is not shown."
                    ),
                )
            }
        }
        LateWindow::Open => (
            base.max(Undetermined),
            format!(
                "The ECU answered with a pending after the exchange had ended, and its time does not show whether it came within the limit in force. {base_reason}"
            ),
        ),
    }
}

/// The reason a floor a late pending set ends with, when the exchange is classified again.
pub(crate) const LATE_PENDING_FLOOR: &str = " A pending the ECU sent for it reached the agent after it had ended, and how it stood against the limit in force stands.";

/// What [`settle_late_response`] gives an exchange whose response a late start showed beginning
/// `at_us` after the transmit, within the limit in force less the margin.
fn started_in_time(at_us: i64) -> (Classification, String) {
    (
        Classification::System,
        format!(
            "The ECU's response started {:.1} ms after the transmit, within the limit in force less the margin, and was delivered after the exchange had ended: the system missed a response that started in time.",
            at_us as f64 / 1000.0
        ),
    )
}

/// The classification and reason a `primitive_ended` gives the exchange it settles (design 8.11
/// "Settlement", ADR-366): `entry`, built from the exchange's evidence with the primitive's end,
/// as if the agent had waited for it, except that an end within the limit in force is `system`,
/// since the job gave up on an exchange that ended in time. A lost VCI and a failure of the
/// system keep their own rows.
///
/// A final response is `system` whatever its time: within the limit the job gave up on it, and
/// after it the late row is `system` too. `observed` is the earlier entry's `system` that
/// rested on the VCI's own clock past its limits, which an end that shows no response under way
/// at that reading does not refute. `unanswered` is an exchange whose start the worker did not
/// answer (ADR-416): its `system` stands under any end.
pub(crate) fn settle_primitive_ended(
    entry: &ExchangeEntry,
    observed: bool,
    unanswered: bool,
) -> (Classification, String) {
    let final_response = matches!(
        entry.outcome,
        Outcome::Positive { .. } | Outcome::PositiveUnusable { .. } | Outcome::Negative { .. }
    );
    // A receive timeout at the limit is `system` only when it ended the exchange: a lost VCI or
    // a link error read after it keeps its own row.
    let timeout_at_limit = entry.timing.window == Window::TimeoutAtLimit
        && matches!(
            entry.outcome,
            Outcome::NoResponse | Outcome::ResponseIncomplete
        );
    if final_response || timeout_at_limit {
        return (
            Classification::System,
            format!(
                "The exchange ended after the agent had stopped waiting for it, with a response or at the limit in force: the job gave up on an exchange that ended. As if the agent had waited: {}",
                entry.reason
            ),
        );
    }
    if observed && entry.classification != Classification::System {
        return (
            Classification::System,
            format!(
                "{} The VCI's clock, read when the agent stopped waiting, had shown it past its own limits with no response under way, which this end does not refute.",
                entry.reason
            ),
        );
    }
    // The start the worker did not answer was a system fault, which the end of the primitive it
    // may have started does not undo: the more severe of the two stands (ADR-416).
    if unanswered && entry.classification != Classification::System {
        return (
            Classification::System,
            format!(
                "{} The worker had not answered the request's start in time, which the end of its primitive, read later, does not refute.",
                entry.reason
            ),
        );
    }
    (entry.classification, entry.reason.clone())
}

fn severity_word(classification: Classification) -> &'static str {
    match classification {
        Classification::None => "none",
        Classification::Vehicle => "vehicle",
        Classification::Undetermined => "undetermined",
        Classification::System => "system",
    }
}

const READING_H1: &str = "the request may not have reached the bus (H1)";
const READING_H2: &str = "a wrong address or filter may have hidden a negative response (H2)";
const READING_H3: &str = "a link error may have lost a frame (H3)";

/// The hidden causes H1 to H3 (design 8.11 "Classification") that the evidence leaves open for
/// an exchange: H1 unless `sent` is `confirmed` or `partial`, H2 unless the worker bound an
/// answer to this exchange (a response start alone does not exclude it, ADR-354) or a matched
/// answer counts, H3 when a link error was reported on the exchange's
/// link or on another link of its channel. H4 is the caller's, since its test depends on the
/// row.
fn open_readings(
    evidence: &Evidence,
    history: &LinkHistory,
    sent: Sent,
    with_h1: bool,
) -> Vec<&'static str> {
    let mut open = Vec::new();
    if with_h1 && !matches!(sent, Sent::Confirmed | Sent::Partial) {
        open.push(READING_H1);
    }
    if !evidence.bound_answer() && history.answered_same_match == 0 {
        open.push(READING_H2);
    }
    if evidence.any_link_error() {
        open.push(READING_H3);
    }
    open
}

/// The code of a negative response with its name, as a reason gives it.
fn named_code(code: Option<u8>) -> String {
    match code {
        Some(code) => match nrc_name(code) {
            Some(name) => format!("{name} ({code:#04x})"),
            None => format!("code {code:#04x}"),
        },
        None => "no code".to_owned(),
    }
}

/// The bytes a message shows in the log: the part of its redacted form that stays in clear.
fn clear_bytes(direction: Direction, message: &[u8]) -> String {
    redact(direction, message).clear().to_owned()
}

/// What a reason adds for a negative response code that makes an exchange `undetermined`.
fn negative_note(code: Option<u8>) -> &'static str {
    match code {
        Some(0x35) => {
            " The key the system computed does not match the one the ECU expects; the key service, the variant's secret and the seed it was given are not told apart (a zero seed shows a level that is already unlocked)."
        }
        Some(0x36 | 0x37) => {
            " The attempt counter or its delay may come from an earlier job or another tester."
        }
        _ => "",
    }
}

/// The reading a window the agent's deadline ended leaves open (ADR-434).
const DEADLINE_OPEN: &str = "the agent stopped waiting before the VCI ended the window, so whether the VCI would have ended it at its limit is not shown";

/// Why the end of an exchange's wait, with no response, does not show that the limit in
/// force ran out (ADR-376): `None` only for a receive timeout judged at the limit on one clock
/// on a link that forwards pendings. The log presumes neither the VCI's timer nor, on J2534,
/// the worker's right, nor that a pending the VCI absorbed restarted its wait.
fn silence_open(evidence: &Evidence) -> Option<String> {
    let mut open = Vec::new();
    if matches!(evidence.end, End::VciDidNotEnd) {
        // The agent's deadline ended the window (ADR-434), not the VCI.
        open.push(DEADLINE_OPEN);
    } else if evidence.timing.window != Window::TimeoutAtLimit {
        open.push(
            "the end of the wait is not judged against the limit in force on one clock, so it may have come early or late: the VCI's timer, or on a J2534 link the worker's, is not presumed right",
        );
    }
    if evidence.absorbing() {
        open.push(
            "the VCI absorbs pendings, so a pending it absorbed may have come, and how its wait went on after one is not seen (H4)",
        );
    }
    (!open.is_empty()).then(|| open.join("; and "))
}

/// The classification and reason of each answer of `evidence`, judged as the exchange it alone
/// would be ([`Evidence::as_answer`], ADR-400), against its own history. The one place that
/// classifies an answer: the entry's answer list and its collecting and several-answers rows all
/// read it. `with_owed` leaves the owed requests an answer could belong to in its verdict; a
/// request meant for one ECU weighs them against its extra answers instead.
fn answer_verdicts(
    evidence: &Evidence,
    history: &LinkHistory,
    with_owed: bool,
) -> Vec<(Classification, String)> {
    evidence
        .answers
        .iter()
        .map(|answer| {
            let mut own = answer.history.clone().unwrap_or_else(|| history.clone());
            if !with_owed {
                own.owed_matching = 0;
            }
            let verdict = classify(&evidence.as_answer(answer), &own);
            (verdict.classification, verdict.reason)
        })
        .collect()
}

/// The entry's `answers`: each answer with its own classification.
fn answer_items(evidence: &Evidence, history: &LinkHistory) -> Vec<AnswerItem> {
    evidence
        .answers
        .iter()
        .zip(answer_verdicts(evidence, history, evidence.collecting))
        .map(|(answer, (classification, reason))| AnswerItem::of(answer, classification, reason))
        .collect()
}

/// "1 answer", "2 answers".
fn counted(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// The worst of the answers' verdicts, the first of equals: its place from 0 and the verdict.
fn worst_answer(
    verdicts: &[(Classification, String)],
) -> Option<(usize, &(Classification, String))> {
    verdicts
        .iter()
        .enumerate()
        .rev()
        .max_by_key(|(_, (classification, _))| *classification)
}

/// The answer that set an entry's classification, as its reason names it.
fn answer_that_set_it(evidence: &Evidence, verdicts: &[(Classification, String)]) -> String {
    match worst_answer(verdicts) {
        Some((at, (_, reason))) => format!(
            " The answer that set it: #{} on {:#x}: {reason}",
            at + 1,
            evidence.answers[at].rx_id
        ),
        None => String::new(),
    }
}

/// The row of a request that collects every answer within a window the VCI ends (design 8.11
/// "Exchange entry" `answers`, ADR-400): the most severe of its answers' classifications, each
/// answer judged from the end of the one before it, and `undetermined` as well when a reading
/// stays open that the answers cannot exclude: a link error on another link of the channel, a
/// pending the VCI absorbed, a window end not judged against its limit on one clock. A link
/// error on the exchange's own link is the link-error rows' (`classify`). With no answer the
/// silence is `none` only for a confirmed send, on a link that forwards pendings, with the end
/// judged at its limit and no link error. A window the agent's deadline ended (ADR-434) takes
/// this row when it has answers and its VCI limits are not past; it has the deadline among the
/// open readings.
fn collecting_row(
    evidence: &Evidence,
    history: &LinkHistory,
    sent: Sent,
    owed_handled: &mut bool,
) -> (Classification, String) {
    let verdicts = answer_verdicts(evidence, history, true);
    // Each answer carries the owed requests it could belong to in its own verdict.
    *owed_handled = !verdicts.is_empty();
    let mut open: Vec<String> = Vec::new();
    if let Some(reading) = silence_open(evidence) {
        open.push(reading);
    }
    if verdicts.is_empty() {
        if sent != Sent::Confirmed {
            open.push(READING_H1.to_owned());
        }
        if evidence.any_link_error() {
            open.push(READING_H3.to_owned());
        }
        return if open.is_empty() {
            (
                Classification::None,
                "No answer came before the window ended: the request was sent, the VCI ended the wait at the limit in force on one clock, the link forwards pendings, and no link error came.".to_owned(),
            )
        } else {
            (
                Classification::Undetermined,
                format!(
                    "No answer came before the window ended, but {}.",
                    open.join("; ")
                ),
            )
        };
    }
    if !evidence.channel_link_errors.is_empty() {
        open.push(
            "a link error on another link of the channel came during the window and may have lost an answer (H3)"
                .to_owned(),
        );
    }
    let worst = verdicts
        .iter()
        .map(|(classification, _)| *classification)
        .max()
        .unwrap_or(Classification::None);
    let identifiers = {
        let mut ids: Vec<u32> = evidence.answers.iter().map(|answer| answer.rx_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let mut reason = format!(
        "{} on {} ({}).{}",
        counted(verdicts.len(), "answer"),
        counted(identifiers.len(), "identifier"),
        identifiers
            .iter()
            .map(|id| format!("{id:#x}"))
            .collect::<Vec<_>>()
            .join(", "),
        answer_that_set_it(evidence, &verdicts)
    );
    if open.is_empty() {
        return (worst, reason);
    }
    reason.push_str(&format!(" Also open: {}.", open.join("; ")));
    (worst.max(Classification::Undetermined), reason)
}

/// Whether every answer can be given its own owner out of `candidates` (the owed requests each
/// could belong to, by `seq`), an owner taking at most one answer: a bipartite matching that
/// covers all answers.
fn owners_absorb(candidates: &[&[u64]]) -> bool {
    fn place(
        at: usize,
        candidates: &[&[u64]],
        taken: &mut Vec<(u64, usize)>,
        seen: &mut Vec<u64>,
    ) -> bool {
        for &owner in candidates[at] {
            if seen.contains(&owner) {
                continue;
            }
            seen.push(owner);
            match taken.iter().position(|(held, _)| *held == owner) {
                None => {
                    taken.push((owner, at));
                    return true;
                }
                Some(slot) => {
                    let other = taken[slot].1;
                    if place(other, candidates, taken, seen) {
                        taken[slot].1 = at;
                        return true;
                    }
                }
            }
        }
        false
    }
    let mut taken = Vec::new();
    (0..candidates.len()).all(|at| place(at, candidates, &mut taken, &mut Vec::new()))
}

/// The row of a request meant for one ECU that got more than one answer (design 8.11 "Exchange
/// entry" `answers`, ADR-400): the extra answers may belong to requests still owed, each
/// weighed by its own history and an owed request taking one at most, which leaves it
/// `undetermined`; otherwise a second responder is a fault of the vehicle, unless an answer is
/// worse.
fn several_answers_row(evidence: &Evidence, history: &LinkHistory) -> (Classification, String) {
    let verdicts = answer_verdicts(evidence, history, false);
    let worst = verdicts
        .iter()
        .map(|(classification, _)| *classification)
        .max()
        .unwrap_or(Classification::None);
    let counted_answers = counted(evidence.answers.len(), "answer");
    // Each extra answer (every one but the first, the request's own) could belong to the owed
    // requests its own history gives; an owed request takes at most one of them (ADR-400
    // decision 4C).
    let candidates: Vec<&[u64]> = evidence
        .answers
        .iter()
        .skip(1)
        .map(|answer| answer.history.as_ref().unwrap_or(history).owners.as_slice())
        .collect();
    if owners_absorb(&candidates) {
        let mut owners: Vec<u64> = candidates.concat();
        owners.sort_unstable();
        owners.dedup();
        let own = LinkHistory {
            this_exchange: true,
            owed_matching: owners.len() as u32,
            owners,
            ..history.clone()
        };
        return owed_tail(
            evidence,
            &own,
            worst,
            format!(
                "The request got {counted_answers}.{}",
                answer_that_set_it(evidence, &verdicts)
            ),
        );
    }
    if worst >= Classification::Undetermined {
        return (
            worst,
            format!(
                "The request got {counted_answers}.{}",
                answer_that_set_it(evidence, &verdicts)
            ),
        );
    }
    (
        Classification::Vehicle,
        format!(
            "The request was meant for one ECU and got {counted_answers}: a second responder answered it, which no earlier request explains."
        ),
    )
}

/// The rows of the table, in its order, for the exchange's own outcome, with no outcome that
/// a time not judged leaves open (ADR-376): a final response whose time is not judged may also
/// have come after the limit in force with the VCI past its own limit, a `system` reading, so
/// it is never `none` or `vehicle` on its own.
fn row(
    evidence: &Evidence,
    history: &LinkHistory,
    owed_handled: &mut bool,
) -> (Classification, String) {
    let (classification, mut reason) = rows(evidence, history, owed_handled);
    let final_response = matches!(
        evidence.end,
        End::Positive(_) | End::PositiveUnusable(_) | End::Negative(_)
    );
    // A collecting entry's own timing judges the window's end, and its answers carry their own
    // (ADR-400).
    if final_response
        && !evidence.collecting
        && evidence.timing.window == Window::NotJudged
        && classification < Classification::Undetermined
    {
        reason.push_str(&format!(
            " Its time is not judged, so it may also have come after the limit in force with the VCI past its own limit ({}); the log does not presume the VCI's timer right, so both readings stay.",
            severity_word(Classification::System)
        ));
        return (Classification::Undetermined, reason);
    }
    (classification, reason)
}

/// [`row`] before a time that is not judged is taken into account.
fn rows(
    evidence: &Evidence,
    history: &LinkHistory,
    owed_handled: &mut bool,
) -> (Classification, String) {
    use Classification::{None as NoFault, System, Undetermined, Vehicle};

    let (sent, _) = sent_of(evidence, history);
    let absorbing = evidence.absorbing();
    let pending_seen = evidence.pending_seen();
    let this_exchange = history.this_exchange;
    // The rows of a pending seen need an answer the worker bound; a response start alone, which
    // is still an answer of the ECU for every other row, leaves them (ADR-354).
    let pending_answer = this_exchange && evidence.bound_answer();
    let window = evidence.timing.window;
    let late = window == Window::Late;
    let timed_out_unjudged = matches!(window, Window::TimeoutEarly | Window::TimeoutLate);
    let answers_listed = !evidence.answers.is_empty();
    // On an absorbing link a time that is not judged, or past P2 on a link that cannot show the
    // pendings, may hide one (design 8.11 "Possible pending").
    let possible_pending =
        absorbing && matches!(window, Window::NotJudged | Window::PossiblePending);
    let any_link_error = evidence.any_link_error();
    let suppressed = evidence.suppressed;
    let to_owned = |text: &str| text.to_owned();

    match &evidence.end {
        // Refused by the agent because of an earlier response, or because the worker reported
        // the session lost: the vehicle may have dropped it.
        End::NotSent(cause @ (RefusalCause::EarlierResponse | RefusalCause::SessionLost)) => (
            Undetermined,
            format!("Not sent: {}.", cause.phrase(evidence.caused_by)),
        ),
        // Refused by the agent, or by the worker for its own reasons.
        End::NotSent(cause) => (
            System,
            format!(
                "Not sent: the diagnostic system refused the request: {}.",
                cause.phrase(evidence.caused_by)
            ),
        ),
        // Any outcome after a forwarded pending past the limit in force before it.
        _ if evidence.timing.late_pending.is_some() => (
            System,
            format!(
                "The ECU sent response pending {} (0x78) {} ms after the limit in force before it, and the VCI did not end its wait at that limit; then {}.",
                evidence.timing.late_pending.unwrap_or_default() + 1,
                by_ms(&evidence.timing),
                outcome_phrase(evidence)
            ),
        ),
        End::VciLost { refused_after_loss } => {
            let mut reason = if *refused_after_loss {
                to_owned(
                    "The VCI had been lost before the request, so it was not sent; until the VCI reconnects, a drop of the vehicle's supply is not excluded.",
                )
            } else {
                to_owned(
                    "The VCI was lost while waiting for the response; until it reconnects, a drop of the vehicle's supply is not excluded.",
                )
            };
            if let Some(voltage) = evidence.supply_voltage {
                reason.push_str(&format!(
                    " The last supply reading was {} mV, {} ms old.",
                    voltage.millivolts, voltage.age_ms
                ));
            }
            (Undetermined, reason)
        }
        End::Unknown(cause) => (
            System,
            format!(
                "A part of the diagnostic system failed ({}); whether the request reached the bus is as the evidence shows.",
                cause.phrase()
            ),
        ),
        // The VCI's own limits past after a confirmed send, by its device clock at the agent's
        // deadline, with no response under way.
        End::VciDidNotEnd
            if sent == Sent::Confirmed && evidence.vci_limits_past_by_us.is_some() =>
        {
            (
                System,
                format!(
                    "The VCI did not end the exchange within its own timing: its clock, read when the agent stopped waiting, was {:.1} ms past the latest limit in force, the margin and the longest single-frame transfer, and no final response had started.",
                    evidence.vci_limits_past_by_us.unwrap_or_default() as f64 / 1000.0
                ),
            )
        }
        // A collecting request with answers: the most severe of them, at least `undetermined`
        // with the deadline open (ADR-434, ADR-400 decision 4(B)). Without answers the generic
        // row below stands.
        End::VciDidNotEnd if evidence.collecting && answers_listed => {
            collecting_row(evidence, history, sent, owed_handled)
        }
        End::VciDidNotEnd => (
            Undetermined,
            to_owned(
                "The agent stopped waiting before the VCI ended the exchange; a long transfer can outlast the agent's deadline.",
            ),
        ),
        End::LinkError(kind) => kind.judge(),
        // A response start alone does not exclude H2 (ADR-354); a pending bound to the
        // exchange or a matched answer does (`open_readings`).
        End::ResponseIncomplete if !evidence.bound_answer() && history.answered_same_match == 0 => {
            (
                Undetermined,
                to_owned(
                    "The ECU began a response that did not complete: a consecutive frame lost in the vehicle network or in the VCI's receive path, the ECU stopping, or a response the worker dropped because it matched no expected response (H2), cannot be told apart (the transport is not logged).",
                ),
            )
        }
        End::ResponseIncomplete => (
            Undetermined,
            to_owned(
                "The ECU began a response that did not complete: a consecutive frame lost in the vehicle network or in the VCI's receive path, or the ECU stopping, cannot be told apart (the transport is not logged).",
            ),
        ),
        // A receive timeout judged early or late (a timeout after a response began is the
        // transport's, and is not judged).
        End::NoResponse if window == Window::TimeoutEarly => (
            System,
            format!(
                "The VCI gave up waiting before the limit in force, by {} ms.",
                by_ms(&evidence.timing)
            ),
        ),
        End::NoResponse if window == Window::TimeoutLate => (
            System,
            format!(
                "The VCI ended its wait after the limit in force, by {} ms: its own timer did not fire in time.",
                by_ms(&evidence.timing)
            ),
        ),
        // A missing response has no row for a window `late`, which only a response gives.
        End::NoResponse if late => last_row(),
        End::NoResponse if suppressed && !this_exchange && !answers_listed => {
            let mut hidden = open_readings(evidence, history, sent, true);
            // A timeout at P2 does not show that no pending came: the VCI's handling of one it
            // absorbed is not seen (ADR-376).
            if absorbing {
                hidden.push("a pending the VCI absorbed may have obliged an answer (H4)");
            }
            // The VCI's own timeout judged at P2 on one clock.
            let timed_out_at_p2 = window == Window::TimeoutAtLimit;
            if hidden.is_empty() && sent == Sent::Confirmed && timed_out_at_p2 {
                // Sent, matched as earlier answers were, no link error, no pending hidden.
                (
                    NoFault,
                    to_owned(
                        "No response was expected, and none came: the request was sent, the ECU has answered requests matched this way, no link error came and no pending was hidden.",
                    ),
                )
            } else if hidden.is_empty() && timed_out_at_p2 {
                // Only a partial send leaves this: any other send but a confirmed one is H1.
                (
                    Undetermined,
                    format!(
                        "No response was expected and none came, but {}, so the ECU may not have received the whole request.",
                        send_phrase(sent).to_lowercase()
                    ),
                )
            } else if hidden.is_empty() {
                (
                    Undetermined,
                    to_owned(
                        "No response was expected and none came, and no hidden cause (H1 to H4) is open, but no receive timeout is judged at P2 on one clock, so nothing shows the wait ran its course: the VCI's timer, or on a J2534 link the worker's, is not presumed right.",
                    ),
                )
            } else {
                (
                    Undetermined,
                    format!("No response was expected, but {}.", hidden.join("; ")),
                )
            }
        }
        // The VCI ended a collecting window before or after the limit in force from the last
        // answer: its own timer, as for a request with no answer. A window whose only results
        // are rejected ones counts (the VCI took them as responses, ADR-440).
        _ if evidence.collecting && evidence.vci_ended && timed_out_unjudged => (
            System,
            format!(
                "The VCI ended the window {} the limit in force by {} ms, counted from the last answer or the transmit: its own timer did not fire in time.",
                if window == Window::TimeoutEarly {
                    "before"
                } else {
                    "after"
                },
                by_ms(&evidence.timing)
            ),
        ),
        // The rows of the answers (ADR-400): a request that collects every answer, with or
        // without one; and a request meant for one ECU that got more than one.
        _ if evidence.collecting && (answers_listed || matches!(evidence.end, End::NoResponse)) => {
            collecting_row(evidence, history, sent, owed_handled)
        }
        _ if !evidence.collecting && evidence.answers.len() > 1 => {
            *owed_handled = true;
            several_answers_row(evidence, history)
        }
        // A window `timeout_early` or `timeout_late` on a response has no other row.
        _ if timed_out_unjudged => last_row(),
        End::Positive(_) if suppressed && !pending_seen && !late => {
            if absorbing {
                (
                    Undetermined,
                    to_owned(
                        "The ECU answered on a link whose VCI absorbs pendings, where even within P2 a pending may have come first; whether a pending the VCI absorbed obliged it to answer cannot be seen (H4).",
                    ),
                )
            } else {
                (
                    Vehicle,
                    to_owned(
                        "The ECU sent a positive response although the request asked it not to.",
                    ),
                )
            }
        }
        // A placed result may be an earlier request's answer alone, so its arrival proves no
        // timing fault of the ECU for this request: only the VCI's wait past the limit is
        // shown (ADR-437 decision 6, ADR-447 decision 4).
        End::PositiveUnusable(_) if late && evidence.end_placed().is_some() => (
            System,
            format!(
                "The VCI did not end its wait at the limit in force but took a result {} ms after it; it was {}.",
                by_ms(&evidence.timing),
                placed_phrase(evidence)
            ),
        ),
        End::Positive(_) | End::PositiveUnusable(_) | End::Negative(_) if late => {
            let outcome = match &evidence.end {
                End::Positive(_) => "it responded positively".to_owned(),
                End::Negative(response) => format!(
                    "it refused the request: {}",
                    named_code(negative_code(response))
                ),
                _ => "its response does not match the definition".to_owned(),
            };
            (
                System,
                format!(
                    "Two faults seen together: the ECU answered after the limit in force, by {} ms, and the VCI did not end its wait at that limit but still took the answer; {outcome}.",
                    by_ms(&evidence.timing)
                ),
            )
        }
        // The result that ended the wait was placed with the earlier exchanges that could own
        // it, so it says nothing certain about this request's own answer: the rows below, which
        // take it as the ECU's answer to this request, do not apply (ADR-447). Below the rows
        // of the VCI's wait against the limit, so a fault of the VCI's timing stays its own
        // (ADR-437 decision 6).
        _ if evidence.end_placed().is_some() => (
            Undetermined,
            format!(
                "The VCI ended the exchange's wait on {}.",
                placed_phrase(evidence)
            ),
        ),
        End::Positive(_) | End::PositiveUnusable(_) | End::Negative(_)
            if possible_pending && !pending_seen =>
        {
            (
                Undetermined,
                to_owned(
                    "The ECU answered after P2, or at a time that is not judged, with no pending seen: on time after a pending the VCI absorbed, or late without one with the VCI past its own limit; the two give the same times (H4).",
                ),
            )
        }
        // Not implemented: a negative response with a code the definition declares definitive
        // for an identification or guard read (`none`). Needs the definition's declared codes.
        End::Positive(_) => (NoFault, to_owned("The ECU responded positively.")),
        End::PositiveUnusable(_) => (
            Undetermined,
            to_owned("The ECU's response does not match the definition."),
        ),
        End::Negative(response) => {
            let code = negative_code(response);
            let kind = code.map_or(NrcKind::UnknownCode, nrc_kind);
            let named = named_code(code);
            let bytes = clear_bytes(Direction::Response, response);
            if kind == NrcKind::EcuState {
                (
                    Vehicle,
                    format!(
                        "The ECU answered and refused the request: {named}; response bytes {bytes}."
                    ),
                )
            } else {
                (
                    Undetermined,
                    format!(
                        "The ECU does not accept the request as the definition built it, in the active session or access state, or at this point of the sequence (a repeat after a resume can cause it): {named}; response bytes {bytes}.{}",
                        negative_note(code)
                    ),
                )
            }
        }
        End::NoResponse if possible_pending && !pending_seen => {
            let mut reason = to_owned(
                "The VCI ended the wait later than P2, or at a time that is not judged: either the ECU answered with a pending the VCI absorbed and then fell silent, or the VCI waited past P2; the two give the same times (H4).",
            );
            let open = open_readings(evidence, history, sent, true);
            if !open.is_empty() {
                reason.push_str(&format!(
                    " Other readings stay open as well: {}.",
                    open.join("; ")
                ));
            }
            (Undetermined, reason)
        }
        // A primitive the worker ended after a pending without a receive timeout (finished or
        // cancelled with no response): no limit is shown to have run out, so the silence is not
        // the ECU's. The agent's own deadline is `vci_did_not_end` (`host::deadline_end`).
        End::NoResponse if pending_answer && !evidence.vci_ended => (
            Undetermined,
            to_owned(
                "The ECU answered with a pending, and the primitive ended with no response and no receive timeout from the VCI, so it is not shown that the limit in force ran out.",
            ),
        ),
        End::NoResponse if pending_answer && !any_link_error => {
            if let Some(open) = silence_open(evidence) {
                (
                    Undetermined,
                    format!(
                        "The ECU answered with a pending and then sent no response before the wait ended, but {open}."
                    ),
                )
            } else if history.owed_matching == 0 {
                (
                    Vehicle,
                    to_owned(
                        "The ECU answered with a pending and then sent no response within the limit in force; the pending shows that the address, the filter and the response patterns match this ECU.",
                    ),
                )
            } else {
                (
                    Vehicle,
                    to_owned(
                        "The ECU answered with a pending and then sent no response within the limit in force, but the pending could belong to an earlier request, so it is not known whether this request reached the bus, nor whether the address, the filter and the response patterns match this ECU.",
                    ),
                )
            }
        }
        End::NoResponse if pending_answer => (
            Undetermined,
            format!(
                "{}, and the ECU answered with a pending, but a link error came on this link or another link of its channel and may have lost a frame (H3).",
                send_phrase(sent)
            ),
        ),
        End::NoResponse
            if sent == Sent::Confirmed && history.answered_same_match > 0 && !any_link_error =>
        {
            match silence_open(evidence) {
                Some(open) => (
                    Undetermined,
                    format!(
                        "Sent on the bus; no response before the wait ended, from an ECU that has answered requests matched this way, but {open}."
                    ),
                ),
                None => (
                    Vehicle,
                    to_owned(
                        "Sent on the bus; no response within the limit in force, from an ECU that has answered requests matched this way.",
                    ),
                ),
            }
        }
        End::NoResponse if sent == Sent::Confirmed => (
            Undetermined,
            format!(
                "Sent on the bus, but {}.",
                open_readings(evidence, history, sent, false).join("; ")
            ),
        ),
        End::NoResponse if sent == Sent::Unknown => {
            let mut reason =
                to_owned("There is no evidence that the request reached the bus (H1).");
            let open = open_readings(evidence, history, sent, false);
            if !open.is_empty() {
                reason.push_str(&format!(
                    " Other readings stay open as well: {}.",
                    open.join("; ")
                ));
            }
            (Undetermined, reason)
        }
        _ => last_row(),
    }
}

/// How a reason words the send: from `sent` alone, so that "sent on the bus" is said only for a
/// confirmed send.
fn send_phrase(sent: Sent) -> &'static str {
    match sent {
        Sent::Confirmed => "Sent on the bus",
        Sent::Partial => "Only the first frame of the request went out",
        Sent::NotSent => "Not sent",
        Sent::Unknown => "It is not known whether the request reached the bus",
    }
}

/// The last row of the table: anything else.
/// By how much a window was early or late, in milliseconds, as a reason gives it.
fn by_ms(timing: &Timing) -> String {
    format!("{:.1}", timing.window_by_ms.unwrap_or_default())
}

fn last_row() -> (Classification, String) {
    (
        Classification::Undetermined,
        "The log could not decide: the evidence is not one the classification covers yet."
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const RX: u32 = 0x7E8;

    fn evidence(request: &[u8], end: End) -> Evidence {
        Evidence::new(
            Origin::Procedure,
            Target::Endpoint {
                link: 1,
                tx_id: 0x7E0,
                rx_id: RX,
            },
            1,
            RX,
            request.to_vec(),
            end,
        )
    }

    fn forwarding(mut evidence: Evidence) -> Evidence {
        evidence.pending_visibility = PendingVisibility::Forwarded;
        evidence
    }

    /// `evidence` with its time judged on one clock, as on a link with transmit times: a
    /// missing response by a receive timeout at the limit in force, a response within P2. A
    /// time not judged leaves a late reading open (ADR-376), which the tests of other rows set
    /// aside this way.
    fn judged(mut evidence: Evidence) -> Evidence {
        evidence.timing.window = match evidence.end {
            End::NoResponse => Window::TimeoutAtLimit,
            _ => Window::WithinP2,
        };
        evidence.timing.not_judged_reason = None;
        evidence
    }

    fn pending() -> Pending {
        Pending {
            vci_us: None,
            clock: None,
            epoch: None,
            agent_us: 10,
            since_previous_ms: None,
        }
    }

    /// The classification and reason of `evidence` against a link with no history.
    fn verdict_of(evidence: &Evidence) -> (Classification, String) {
        let verdict = classify(evidence, &LinkState::default().history_for(evidence));
        (verdict.classification, verdict.reason)
    }

    /// Classifies `evidence` against an empty link.
    fn verdict(evidence: &Evidence) -> Verdict {
        classify(evidence, &LinkHistory::empty())
    }

    /// Logs `evidence` as line `seq` on `state`, which takes it in.
    fn log(state: &mut LinkState, seq: u64, evidence: Evidence) -> ExchangeEntry {
        let history = state.history_for(&evidence);
        let entry = ExchangeEntry::build(seq, evidence, history);
        state.record(&entry);
        entry
    }

    const READ_F186: [u8; 3] = [0x22, 0xF1, 0x86];

    /// [`log_kept`] with each answer's history set from the owed requests first, as the log
    /// does for a collecting exchange.
    fn log_kept_owned(
        state: &mut LinkState,
        seq: u64,
        mut evidence: Evidence,
    ) -> (ExchangeEntry, Kept) {
        let history = state.history_for(&evidence);
        evidence.set_answer_histories(state.answer_histories(&evidence, &history, seq));
        log_kept(state, seq, evidence)
    }

    /// Logs `evidence` as line `seq` on `state`, as the log does, into its window too; what the
    /// log keeps of it, as a settling entry finds it.
    fn log_kept(state: &mut LinkState, seq: u64, evidence: Evidence) -> (ExchangeEntry, Kept) {
        let clears = state.clears();
        let entry = log(state, seq, evidence.clone());
        let mut kept = Kept {
            origin: evidence.origin,
            request: Request::of(&evidence),
            evidence: evidence.clone(),
            classification: entry.classification,
            sent: entry.sent,
            transmit: None,
            pendings: Vec::new(),
            pending_order: Vec::new(),
            answer_order: Vec::new(),
            answer_ends: Vec::new(),
            limits: None,
            start_unattached: false,
            late_start: None,
            late_start_id: None,
            answer_at: AnswerTime::None,
            late_pendings: Vec::new(),
            late_places: Vec::new(),
            floor: None,
            partial: None,
            pending_at_end: false,
            clears: state.clears(),
            matched: entry.matched_answer(),
        };
        // The time of its answer, as the log stores it at the entry (ADR-385).
        if entry.link_history.this_exchange {
            let vci = evidence.times.vci;
            let final_response = (evidence.end.response().is_some() && !evidence.end_rejected)
                .then(|| vci.and_then(|vci| vci.rx_end?.placed()));
            let start = evidence
                .response_started
                .then(|| vci.and_then(|vci| vci.rx_start?.placed()));
            // The entry's pendings, placed on their clock when they name one, as the log places
            // them.
            let pendings: Vec<super::super::timing::At> = evidence
                .pending
                .iter()
                .map(|pending| {
                    pending.clock.zip(pending.epoch.zip(pending.vci_us)).map(
                        |(clock, (epoch, us))| {
                            (
                                clock,
                                super::super::clocks::Placed {
                                    epoch,
                                    unwrapped: i64::try_from(us).unwrap_or(i64::MAX),
                                },
                            )
                        },
                    )
                })
                .collect();
            kept.answer_at = kept.answer_time(final_response, &pendings, start);
        }
        state.log(&entry, evidence, clears, kept.answer_at);
        (entry, kept)
    }

    /// The answer a settling entry adds when exchange `seq`, kept as `kept`, gets `response`.
    fn final_answer(state: &LinkState, seq: u64, kept: &Kept, response: &[u8]) -> SettledAnswer {
        let answered = kept.answered(response, &LateAnswer::default());
        let history = state.history_settling(&answered, seq);
        ExchangeEntry::build(seq, answered, history).settled_answer(kept, true)
    }

    /// The settling entry's view of exchange `seq`, kept as `kept`, that gets `response` late,
    /// applied as the log applies it: settled, then the end of the evidence before it, then the
    /// answer itself.
    fn settle_late(
        state: &mut LinkState,
        seq: u64,
        kept: &Kept,
        response: &[u8],
    ) -> (ExchangeEntry, Vec<Reclassified>, Option<u64>) {
        state.settle_owed(seq);
        let answered = kept.answered(response, &LateAnswer::default());
        let history = state.history_settling(&answered, seq);
        let entry = ExchangeEntry::build(seq, answered.clone(), history);
        state.settled(seq, entry.classification, &entry.reason, answered, true);
        let answer = entry.settled_answer(kept, true);
        let (changed, through) =
            super::super::settle_later(state, seq, entry.late_restart(), &answer);
        (entry, changed, through)
    }

    /// Whether exchange `seq` has left the link's window.
    fn logged_beyond(state: &LinkState, seq: u64) -> bool {
        state.window.iter().all(|logged| logged.seq != seq)
    }

    fn window_history(state: &LinkState, seq: u64) -> &LinkHistory {
        &state
            .window
            .iter()
            .find(|logged| logged.seq == seq)
            .expect("in the window")
            .history
    }

    /// ADR-369: a session change accepted by a response read after its entry ends the evidence
    /// from before it as it would have in time. The exchanges logged after it lose the matched
    /// answers and the owners from before it, and the window keeps it and those exchanges.
    #[test]
    fn a_late_acceptance_ends_the_evidence_from_before_it() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = judged(forwarding(evidence(request, end)));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let f186 = || End::Positive(vec![0x62, 0xF1, 0x86, 0x41]);
        let mut state = LinkState::default();
        // A matched answer for F186, a silent read of F189 and a session change, both owed.
        log_kept(&mut state, 1, confirmed(&READ_F186, f186()));
        log_kept(
            &mut state,
            2,
            confirmed(&[0x22, 0xF1, 0x89], End::NoResponse),
        );
        let (_, kept) = log_kept(&mut state, 3, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(3, kept.clone());
        // After it: a silent F186 read, `vehicle` on that matched answer; an F189 read whose
        // answer the owed F189 read could own; an F186 read whose answer the silent one after
        // the reset could own, and a silent F186 read.
        let (fourth, _) = log_kept(&mut state, 4, confirmed(&READ_F186, End::NoResponse));
        assert_eq!(fourth.classification, Classification::Vehicle);
        let (fifth, _) = log_kept(
            &mut state,
            5,
            confirmed(
                &[0x22, 0xF1, 0x89],
                End::Positive(vec![0x62, 0xF1, 0x89, 0x41]),
            ),
        );
        assert_eq!(fifth.link_history.owners, vec![2]);
        log_kept(&mut state, 6, confirmed(&READ_F186, f186()));
        let (seventh, _) = log_kept(&mut state, 7, confirmed(&READ_F186, End::NoResponse));
        assert_eq!(seventh.link_history.answered_same_match, 1);

        let (entry, changed, through) =
            settle_late(&mut state, 3, &kept, &[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]);
        assert_eq!(entry.late_restart(), Some((true, 3)));
        assert_eq!(through, None);
        // The silent F186 read after it has no matched answer any more.
        let fourth = changed.iter().find(|later| later.seq == 4);
        assert_eq!(
            fourth.map(|later| later.classification),
            Some(Classification::Undetermined),
            "{changed:?}"
        );
        // The F189 read before it is owed no more: the answer after it is that exchange's
        // alone, a matched answer for the exchanges after it.
        assert!(window_history(&state, 5).owners.is_empty());
        assert!(
            !state.owed.iter().any(|owed| owed.seq < 3),
            "{:?}",
            state.owed
        );
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x89], End::NoResponse))
                .answered_same_match,
            1
        );
        // F186's matched answer before it counts no more, for the exchanges logged after it
        // and for those to come; the answer after it is still one an owed request after the
        // reset could own.
        assert_eq!(window_history(&state, 7).answered_same_match, 0);
        assert_eq!(window_history(&state, 6).owners, vec![4]);
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            0
        );
        assert_eq!(state.last_reset(), Some(3));
        let seqs: Vec<u64> = state.window.iter().map(|logged| logged.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5, 6, 7]);
        assert_eq!(state.left_window, Some(2));
        // The answered counts stand, and the late answer adds one: the answers were given.
        assert_eq!(
            window_history(&state, 7).answered,
            seventh.link_history.answered + 1
        );
    }

    /// ADR-369: what the reset shifts and frees keeps the evidence after it. A matched answer
    /// logged after it still counts; an owed exchange after it whose answer comes later still
    /// counts for the exchanges after that one; and an exchange the reset places alone is no
    /// owner of a later answer.
    #[test]
    fn a_late_acceptance_keeps_the_evidence_after_it() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = forwarding(evidence(request, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let f186 = || End::Positive(vec![0x62, 0xF1, 0x86, 0x41]);
        let accept: &[u8] = &[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4];

        // A matched answer after the reset.
        let mut state = LinkState::default();
        let (_, reset) = log_kept(&mut state, 1, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(1, reset.clone());
        log_kept(&mut state, 2, confirmed(&READ_F186, f186()));
        log_kept(&mut state, 3, confirmed(&READ_F186, End::NoResponse));
        assert_eq!(window_history(&state, 3).answered_same_match, 1);
        settle_late(&mut state, 1, &reset, accept);
        assert_eq!(window_history(&state, 3).answered_same_match, 1);
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            1
        );

        // An owed exchange after the reset, answered after the late acceptance: a matched
        // answer for the exchanges to come.
        let mut state = LinkState::default();
        let (_, reset) = log_kept(&mut state, 1, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(1, reset.clone());
        let (_, owed) = log_kept(&mut state, 2, confirmed(&READ_F186, End::NoResponse));
        state.keep(2, owed);
        settle_late(&mut state, 1, &reset, accept);
        let owed = state.kept(2).cloned().expect("still owed");
        settle_late(&mut state, 2, &owed, &[0x62, 0xF1, 0x86, 0x41]);
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            1
        );

        // An exchange the reset places alone is no owner of the answer after it.
        let mut state = LinkState::default();
        log_kept(
            &mut state,
            1,
            confirmed(&[0x22, 0xF1, 0x89], End::NoResponse),
        );
        let (_, reset) = log_kept(&mut state, 2, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(2, reset.clone());
        let f189 = || End::Positive(vec![0x62, 0xF1, 0x89, 0x41]);
        log_kept(&mut state, 3, confirmed(&[0x22, 0xF1, 0x89], f189()));
        log_kept(&mut state, 4, confirmed(&[0x22, 0xF1, 0x89], f189()));
        assert_eq!(window_history(&state, 4).owners, vec![1, 3]);
        settle_late(&mut state, 2, &reset, accept);
        assert!(window_history(&state, 3).owners.is_empty());
        assert!(window_history(&state, 4).owners.is_empty());
        assert!(!state.owed.iter().any(|owed| owed.seq == 3 || owed.seq == 4));
    }

    /// ADR-369: an unusable positive response to an ECUReset, read late, ends the matched
    /// answers from before it only, as in time: the owed requests and the window stay.
    #[test]
    fn a_late_unusable_answer_to_a_reset_ends_the_matched_answers_only() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = judged(forwarding(evidence(request, end)));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let mut state = LinkState::default();
        log_kept(
            &mut state,
            1,
            confirmed(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86, 0x41])),
        );
        log_kept(
            &mut state,
            2,
            confirmed(&[0x22, 0xF1, 0x89], End::NoResponse),
        );
        log_kept(&mut state, 3, confirmed(&[0x11, 0x01], End::NoResponse));
        let (fourth, _) = log_kept(&mut state, 4, confirmed(&READ_F186, End::NoResponse));
        assert_eq!(fourth.classification, Classification::Vehicle);
        let (changed, through) = state.reset_after(3, 3, false);
        assert_eq!(through, None);
        assert_eq!(
            changed
                .iter()
                .find(|later| later.seq == 4)
                .map(|later| later.classification),
            Some(Classification::Undetermined),
            "{changed:?}"
        );
        assert!(state.owed.iter().any(|owed| owed.seq == 2));
        assert_eq!(state.window.front().map(|logged| logged.seq), Some(1));
        assert_eq!(state.last_reset(), None);
        // An earlier owed request's answer, read later, is no matched answer for the
        // exchanges after the reset.
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            0
        );
    }

    /// Which settling entries end the evidence before their exchange (ADR-369).
    #[test]
    fn only_a_positive_answer_to_a_reset_or_session_change_ends_the_evidence_late() {
        let late = |request: &[u8], end| {
            ExchangeEntry::build(7, evidence(request, end), LinkHistory::empty()).late_restart()
        };
        assert_eq!(
            late(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            Some((true, 7))
        );
        assert_eq!(
            late(&[0x11, 0x01], End::Positive(vec![0x51, 0x01])),
            Some((true, 7))
        );
        assert_eq!(
            late(&[0x11, 0x01], End::PositiveUnusable(vec![0x51])),
            Some((false, 7))
        );
        assert_eq!(
            late(&[0x11, 0x01], End::Negative(vec![0x7F, 0x11, 0x22])),
            None
        );
        assert_eq!(late(&[0x11, 0x01], End::NoResponse), None);
        assert_eq!(
            late(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86])),
            None
        );
        // The earliest request its answer could belong to bounds the owed requests it ends.
        let history = LinkHistory {
            owners: vec![4, 2],
            owed_matching: 2,
            this_exchange: true,
            ..LinkHistory::empty()
        };
        let entry = ExchangeEntry::build(
            7,
            evidence(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            history,
        );
        assert_eq!(entry.late_restart(), Some((true, 2)));
    }

    /// ADR-369: when the reset has left the window, the exchanges after it still in the window
    /// count their matched answers from the window alone, and the ones that left are named.
    #[test]
    fn a_late_acceptance_past_the_window_recounts_from_the_window() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = forwarding(evidence(request, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(1, kept.clone());
        log_kept(
            &mut state,
            2,
            confirmed(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86, 0x41])),
        );
        let last = 2 + WINDOW as u64;
        for seq in 3..=last {
            log_kept(&mut state, seq, confirmed(&READ_F186, End::NoResponse));
        }
        assert_eq!(state.left_window, Some(2));
        assert_eq!(window_history(&state, last).answered_same_match, 1);
        let (_, changed, through) = settle_late(&mut state, 1, &kept, &[0x50, 0x03]);
        assert_eq!(through, Some(2));
        assert_eq!(window_history(&state, last).answered_same_match, 0);
        assert!(
            changed
                .iter()
                .all(|later| later.classification == Classification::Undetermined),
            "{changed:?}"
        );
        assert!(!changed.is_empty());
    }

    /// An owed first exchange `1`, kept, and an exchange `2` after it whose answer only `1` could
    /// own, kept and owed through it, then enough exchanges for `2` to leave the window.
    fn owner_and_answered_beyond_window(
        second: impl Fn(&mut Evidence),
        sent_unknown: bool,
    ) -> (LinkState, Kept) {
        owners_and_answered_beyond_window(1, |_, evidence| second(evidence), sent_unknown)
    }

    /// [`owner_and_answered_beyond_window`] with `count` exchanges `2..` after the first, each
    /// made by `second` (given its place), all beyond the window.
    fn owners_and_answered_beyond_window(
        count: usize,
        mut second: impl FnMut(usize, &mut Evidence),
        sent_unknown: bool,
    ) -> (LinkState, Kept) {
        let confirmed = |end| {
            let mut evidence = forwarding(evidence(&READ_F186, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let mut state = LinkState::default();
        let (_, first) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        state.keep(1, first.clone());
        for at in 0..count {
            let seq = 2 + at as u64;
            let mut answered = confirmed(End::Positive(vec![0x62, 0xF1, 0x86, 0x41]));
            second(at, &mut answered);
            let (_, mut kept) = log_kept_owned(&mut state, seq, answered);
            assert!(!kept.matched);
            if sent_unknown {
                kept.sent = Sent::Unknown;
                state.unknown_sends += 1;
            }
            state.keep(seq, kept);
            assert!(state.owed.iter().any(|owed| owed.seq == seq));
        }
        for seq in 2 + count as u64..2 + (count + WINDOW) as u64 {
            log_kept(&mut state, seq, confirmed(End::NoResponse));
        }
        assert!(
            state
                .window
                .iter()
                .all(|logged| logged.seq > 1 + count as u64)
        );
        (state, first)
    }

    /// ADR-439: a kept exchange beyond the window that only an owner could own the answer of is
    /// placed alone when that owner settles, as in the window: released, its send confirmed, its
    /// answer freed as final and counted once as a matched answer.
    #[test]
    fn an_exchange_beyond_the_window_is_released_when_its_owner_settles() {
        let (mut state, _) = owner_and_answered_beyond_window(|_| {}, true);
        assert_eq!(state.unknown_sends, 1);
        state.settle_owed(1);
        assert!(state.kept(2).is_some());
        let freed = state.refresh_kept_beyond_window(2..u64::MAX, Place::Beyond);
        let [freed] = &freed[..] else {
            panic!("{freed:?}");
        };
        assert_eq!(freed.seq, 2);
        assert!(freed.final_response && freed.newly_matched, "{freed:?}");
        assert!(state.kept(2).is_none());
        assert!(state.owed.iter().all(|owed| owed.seq != 2));
        assert_eq!(state.unknown_sends, 0);
        // Counted once: a second pass finds nothing to free.
        assert!(
            state
                .refresh_kept_beyond_window(2..u64::MAX, Place::Beyond)
                .is_empty()
        );
    }

    /// The count of matched answers with the key of the exchanges `owners_and_answered_beyond_window`
    /// logs, for the link's next exchange.
    fn link_matched(state: &LinkState) -> u32 {
        let key = MatchKey::of(&state.window.back().unwrap().evidence);
        state.matched.get(&key).copied().unwrap_or(0)
    }

    fn same_match(state: &LinkState, seq: u64) -> u32 {
        state.logged_history(seq).unwrap().answered_same_match
    }

    /// ADR-441 decision 7: the matched answer of a kept exchange after the restart's exchange,
    /// released by the restart's visit, is counted once for the window's later exchange and the
    /// link, as a settlement counts it; whether it counted at its record or not.
    #[test]
    fn a_restart_counts_the_matched_answer_of_a_released_exchange_once() {
        for matched_at_record in [false, true] {
            let (mut state, _) = owners_and_answered_beyond_window(1, |_, _| {}, false);
            state.kept.get_mut(&2).unwrap().matched = matched_at_record;
            let window_exchange = state.window.back().unwrap().seq;
            // The restart is exchange 1; the earliest request its answer could belong to is 2,
            // so no owner is left for exchange 2, which is released.
            state.reset_after(1, 2, true);
            assert!(state.kept(2).is_none(), "{matched_at_record}");
            assert_eq!(
                same_match(&state, window_exchange),
                1,
                "{matched_at_record}"
            );
            assert_eq!(link_matched(&state), 1, "{matched_at_record}");
        }
    }

    /// ADR-441 decision 7: a kept exchange after the restart's that stays kept and counted at its
    /// record counts in the new epoch too, as a released one does, and once.
    #[test]
    fn a_restart_counts_the_matched_answer_of_a_kept_exchange_that_stays_kept() {
        let (mut state, _) = owners_and_answered_beyond_window(1, |_, _| {}, false);
        state.kept.get_mut(&2).unwrap().matched = true;
        let window_exchange = state.window.back().unwrap().seq;
        // Not accepted: nothing is released, only the matched answers end.
        state.reset_after(1, 2, false);
        assert!(state.kept(2).is_some());
        assert_eq!(same_match(&state, window_exchange), 1);
        assert_eq!(link_matched(&state), 1);
    }

    /// ADR-441: a kept exchange after the restart's that stays kept and did not count is counted
    /// by the settlement after the restart, once.
    #[test]
    fn a_kept_exchange_that_stays_kept_after_a_restart_is_counted_by_the_settlement_once() {
        let (mut state, owner) = owners_and_answered_beyond_window(1, |_, _| {}, false);
        let window_exchange = state.window.back().unwrap().seq;
        state.reset_after(1, 1, true);
        assert!(state.kept(2).is_some());
        assert_eq!(same_match(&state, window_exchange), 0);
        assert_eq!(link_matched(&state), 0);
        state.settle_owed(1);
        let answer = final_answer(&state, 1, &owner, &[0x62, 0xF1, 0x86, 0x41]);
        state.reclassify_after(&answer);
        assert!(state.kept(2).is_none());
        assert_eq!(same_match(&state, window_exchange), 1);
        assert_eq!(link_matched(&state), 1);
    }

    /// ADR-441 decision 3: a survivor between the earliest request and the restart's exchange is
    /// released without counting its matched answer, which is from before the restart.
    #[test]
    fn a_released_survivor_does_not_count_its_matched_answer() {
        let (mut state, _) = owners_and_answered_beyond_window(1, |_, _| {}, false);
        // The restart is a window exchange; exchange 2 survives it (earliest 2), loses its
        // owner 1 and is released.
        let restart = state.window.front().unwrap().seq;
        state.reset_after(restart, 2, true);
        assert!(state.kept(2).is_none());
        let after = state.window.back().unwrap().seq;
        assert!(after > restart);
        assert_eq!(same_match(&state, after), 0);
        assert_eq!(link_matched(&state), 0);
    }

    /// ADR-439: an answer a link error followed ended its matched answer, so the freed answer of
    /// an exchange beyond the window is no matched answer either.
    #[test]
    fn an_answer_freed_beyond_the_window_after_a_link_error_is_no_matched_answer() {
        let (mut state, _) = owner_and_answered_beyond_window(
            |evidence| evidence.link_errors = vec![LinkErrorKind::Receive],
            false,
        );
        state.settle_owed(1);
        let freed = state.refresh_kept_beyond_window(2..u64::MAX, Place::Beyond);
        let [freed] = &freed[..] else {
            panic!("{freed:?}");
        };
        assert!(freed.final_response && !freed.newly_matched, "{freed:?}");
        assert!(state.kept(2).is_none());
    }

    /// A pending only, which the first exchange could own: it stays owed after its owner
    /// settles, and its pending turns into a matched answer.
    fn pending_only(evidence: &mut Evidence) {
        evidence.end = End::NoResponse;
        evidence.pending = vec![pending()];
    }

    /// ADR-439: an exchange beyond the window that stays owed and whose pending turns into a
    /// matched answer is counted once, and has its send confirmed by the answer even when its
    /// own transmit indication did not (the exchange stays owed).
    #[test]
    fn a_pending_turned_matched_beyond_the_window_is_counted_once_and_confirms_the_send() {
        let (mut state, _) = owner_and_answered_beyond_window(
            |evidence| {
                pending_only(evidence);
                evidence.transmit_evidence = None;
            },
            false,
        );
        assert_eq!(state.unknown_sends, 1);
        assert_eq!(state.kept(2).map(|kept| kept.sent), Some(Sent::Unknown));
        state.settle_owed(1);
        let freed = state.refresh_kept_beyond_window(2..u64::MAX, Place::Beyond);
        let [freed] = &freed[..] else {
            panic!("{freed:?}");
        };
        assert!(freed.newly_matched && !freed.final_response, "{freed:?}");
        let kept = state.kept(2).expect("a pending leaves it owed");
        assert!(kept.matched);
        assert_eq!(kept.sent, Sent::Confirmed);
        assert_eq!(state.unknown_sends, 0);
        assert!(
            state
                .refresh_kept_beyond_window(2..u64::MAX, Place::Beyond)
                .is_empty()
        );
    }

    /// ADR-439: a matched answer that does not count beyond the window (a late link error whose
    /// place is unknown may have come after it) changes nothing for the later exchanges, so it
    /// is not offered at any visit, and a chain of such exchanges stays linear.
    #[test]
    fn a_matched_answer_that_cannot_count_beyond_the_window_is_not_offered() {
        let (mut state, kept) = owners_and_answered_beyond_window(6, |_, e| pending_only(e), false);
        state.settle_owed(1);
        state.late_error_unplaced = Some(1_000);
        assert!(
            state
                .refresh_kept_beyond_window(2..u64::MAX, Place::Beyond)
                .is_empty()
        );
        assert!(
            state
                .refresh_kept_beyond_window(2..u64::MAX, Place::Beyond)
                .is_empty()
        );
        assert!((2..8).all(|seq| state.kept(seq).is_some_and(|kept| !kept.matched)));
        // The settling of the first reaches all of them in one visit each.
        state.reclassify_after(&final_answer(&state, 1, &kept, &[0x62, 0xF1, 0x86, 0x41]));
        assert!((2..8).all(|seq| state.kept(seq).is_some()));
    }

    /// ADR-441 decision 7: a kept exchange beyond the window whose own link error came after its
    /// answer has no matched answer to count in the new epoch, though its record says it
    /// counted: `record` ended that answer, as the freed answer of a released one is not counted
    /// either. Accepting or not.
    #[test]
    fn a_restart_does_not_count_a_kept_answer_a_link_error_followed() {
        for (earliest, accepting) in [(2, false), (1, true)] {
            let (mut state, _) = owner_and_answered_beyond_window(
                |evidence| evidence.link_errors = vec![LinkErrorKind::Receive],
                false,
            );
            state.kept.get_mut(&2).unwrap().matched = true;
            let window_exchange = state.window.back().unwrap().seq;
            state.reset_after(1, earliest, accepting);
            assert!(state.kept(2).is_some(), "{accepting}");
            assert_eq!(same_match(&state, window_exchange), 0, "{accepting}");
            assert_eq!(link_matched(&state), 0, "{accepting}");
        }
    }

    /// ADR-441 decision 7: nor does one whose answer a late link error ended, as it ends a window
    /// exchange's: `error_after` clears the record's matched answer when the record is after the
    /// error's place and its answer time is not shown after the error (ADR-385). An error read
    /// after the window that could not be placed leaves a record before it counted for the
    /// window's exchanges, which come before the error (not for the link's next exchange).
    #[test]
    fn a_restart_does_not_count_a_kept_answer_a_late_link_error_may_have_followed() {
        let at = |us| {
            let (clock, placed) = placed_at(Clock::Device, 0, us);
            AnswerTime::At(clock, placed)
        };
        /// Where the late error is read: after the owner (before the kept record) or after the
        /// record itself, placed at 2 000 us, after the window and unplaced, or not at all.
        #[derive(Clone, Copy, Debug)]
        enum Error {
            PlacedBeforeRecord,
            PlacedAfterRecord,
            UnplacedAfterWindow,
            None,
        }
        let cases = [
            (Error::PlacedBeforeRecord, at(1_000), 0),
            (Error::PlacedBeforeRecord, at(3_000), 1),
            (Error::PlacedAfterRecord, at(1_000), 0),
            (Error::PlacedAfterRecord, at(3_000), 1),
            (Error::UnplacedAfterWindow, AnswerTime::Unknown, 1),
            (Error::None, AnswerTime::Unknown, 1),
        ];
        for (error, answer_at, counted) in cases {
            for (earliest, accepting) in [(2, false), (1, true)] {
                let (mut state, _) = owner_and_answered_beyond_window(|_| {}, false);
                {
                    let kept = state.kept.get_mut(&2).unwrap();
                    kept.matched = true;
                    kept.answer_at = answer_at;
                }
                let window_exchange = state.window.back().unwrap().seq;
                match error {
                    Error::PlacedBeforeRecord => {
                        let time = Some(placed_at(Clock::Device, 0, 2_000));
                        state.error_after(Some(1), 100, LinkErrorKind::Receive, time);
                    }
                    // The error falls after the record itself, which the `>= from` bound of
                    // the shift and of the ending both take in.
                    Error::PlacedAfterRecord => {
                        let time = Some(placed_at(Clock::Device, 0, 2_000));
                        state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
                    }
                    Error::UnplacedAfterWindow => {
                        state.error_after(Some(window_exchange), 100, LinkErrorKind::Receive, None);
                        assert!(state.late_error_unplaced > Some(2));
                    }
                    Error::None => {}
                }
                assert_eq!(
                    state.kept(2).unwrap().matched,
                    counted == 1,
                    "{error:?} {answer_at:?}"
                );
                state.reset_after(1, earliest, accepting);
                assert_eq!(
                    same_match(&state, window_exchange),
                    counted,
                    "{error:?} {answer_at:?} {accepting}"
                );
                // The error read after the window is before the link's next exchange, which
                // takes no matched answer from before it.
                let next = if matches!(error, Error::UnplacedAfterWindow) {
                    0
                } else {
                    counted
                };
                assert_eq!(
                    link_matched(&state),
                    next,
                    "{error:?} {answer_at:?} {accepting}"
                );
            }
        }
    }

    /// An owed exchange `1` and an exchange `2` after it, kept and owed through it, whose
    /// answer only `1` could own. Exchange `1` then settles with its own answer, so the answer
    /// of `2` is freed and counts as a matched answer, as the log counts it; its answer time is
    /// then set to `answer_us` on the device clock, in the window and in the record alike.
    /// Gives `1`'s record as it stood before it settled, which settling removed.
    fn answered_and_kept(state: &mut LinkState, answer_us: i64) -> Kept {
        let confirmed = |end| {
            let mut evidence = forwarding(evidence(&READ_F186, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let (_, first) = log_kept(state, 1, confirmed(End::NoResponse));
        state.keep(1, first.clone());
        let mut second = confirmed(End::NoResponse);
        pending_only(&mut second);
        let (_, kept) = log_kept_owned(state, 2, second);
        assert!(!kept.matched);
        state.keep(2, kept);
        settle_late(state, 1, &first, &[0x62, 0xF1, 0x86, 0x41]);
        assert!(state.kept(2).unwrap().matched);
        let (clock, placed) = placed_at(Clock::Device, 0, answer_us);
        let at = AnswerTime::At(clock, placed);
        state.kept.get_mut(&2).unwrap().answer_at = at;
        state
            .window
            .iter_mut()
            .find(|logged| logged.seq == 2)
            .unwrap()
            .answer_at = at;
        first
    }

    /// An exchange `seq` without an answer, logged on `state` (it counts no matched answer).
    fn log_unanswered(state: &mut LinkState, seq: u64) {
        let mut evidence = forwarding(evidence(&READ_F186, End::NoResponse));
        evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept(state, seq, evidence);
    }

    /// ADR-441 decision 7, ADR-444: a late link error placed after an owed exchange ends that
    /// exchange's matched answer when its stored time is not shown after the error, and keeps
    /// it when it is, in the window and beyond it. Once the exchange leaves the window, a
    /// restart counts it in the new epoch only when it was kept.
    #[test]
    fn a_restart_counts_the_answer_of_the_exchange_a_late_error_was_placed_after_by_its_stored_time()
     {
        for answer_us in [1_000, 3_000] {
            let mut state = LinkState::default();
            answered_and_kept(&mut state, answer_us);
            // The error is read after exchange 2 and placed at 2 000 us.
            let time = Some(placed_at(Clock::Device, 0, 2_000));
            state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
            assert_eq!(state.kept(2).unwrap().matched, answer_us > 2_000);
            for seq in 3..3 + WINDOW as u64 {
                log_unanswered(&mut state, seq);
            }
            assert!(state.window.iter().all(|logged| logged.seq > 2));
            assert!(state.kept(2).is_some());
            let window_exchange = state.window.back().unwrap().seq;
            state.reset_after(1, 1, true);
            let counted = u32::from(answer_us > 2_000);
            assert_eq!(same_match(&state, window_exchange), counted, "{answer_us}");
            assert_eq!(link_matched(&state), counted, "{answer_us}");
        }
    }

    /// The same exchange gives the same count in the window and once beyond it: after a late link
    /// error placed after exchange `2`, a restart counts `2`'s answer for the later exchanges
    /// when its stored time is shown after the error and for none when it is not, whether `2`
    /// is still in the window or has left it, and for either restart (ADR-444).
    #[test]
    fn the_answer_a_late_error_was_placed_after_counts_alike_in_and_beyond_the_window() {
        for (answer_us, expected) in [(1_000, (0, 0)), (3_000, (1, 1))] {
            for (earliest, accepting) in [(1, true), (2, false)] {
                let mut counts = Vec::new();
                for extra in [1, WINDOW as u64] {
                    let mut state = LinkState::default();
                    answered_and_kept(&mut state, answer_us);
                    let time = Some(placed_at(Clock::Device, 0, 2_000));
                    state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
                    for seq in 3..3 + extra {
                        log_unanswered(&mut state, seq);
                    }
                    assert_eq!(
                        state.window.iter().any(|logged| logged.seq == 2),
                        extra == 1
                    );
                    let last = state.window.back().unwrap().seq;
                    state.reset_after(1, earliest, accepting);
                    counts.push((same_match(&state, last), link_matched(&state)));
                }
                assert_eq!(
                    counts,
                    [expected, expected],
                    "{answer_us} {earliest} {accepting}"
                );
            }
        }
    }

    /// Exchange `1`, kept and owed, which transmits at 1 000 us on the device clock, and a
    /// silent exchange `2` of its match that transmits at 6 000 us. Gives the state and `1`'s
    /// evidence.
    fn owed_and_later_silence() -> (LinkState, Evidence) {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, timed(silent.clone(), Some(1_000), None));
        state.keep(1, kept);
        log_kept(&mut state, 2, timed(silent.clone(), Some(6_000), None));
        (state, silent)
    }

    /// What exchange `1`'s answer and a late link error leave of the counts: the later exchange
    /// `2`'s counts and classification, the link's, and the answer's own record and window entry.
    #[derive(Debug, PartialEq)]
    struct Settlement {
        same_match: u32,
        since_error: u32,
        vehicle: bool,
        link_matched: u32,
        link_since_error: u32,
        matched_at: Option<u64>,
        after: Clears,
        kept_clears: Clears,
        kept_matched: bool,
    }

    fn settlement(state: &LinkState) -> Settlement {
        let first = state.window.iter().find(|logged| logged.seq == 1).unwrap();
        let later = state.window.iter().find(|logged| logged.seq == 2).unwrap();
        let kept = state.kept(1).unwrap();
        Settlement {
            same_match: later.history.answered_same_match,
            since_error: later.history.answered_since_error,
            vehicle: later.classification == Classification::Vehicle,
            link_matched: link_matched(state),
            link_since_error: state.answered_since_error,
            matched_at: first.matched_at,
            after: first.after,
            kept_clears: kept.clears,
            kept_matched: kept.matched,
        }
    }

    /// Exchange `1`'s pending is offered at `answer` and a late error at 5 000 us is read, in
    /// the order given (`error_first` reads the error first). Gives the state and where the
    /// error was placed.
    fn answer_and_late_error(
        silent: &Evidence,
        mut state: LinkState,
        answer: AnswerTime,
        error_first: bool,
    ) -> (LinkState, ErrorPoint) {
        let error = placed_at(Clock::Device, 0, 5_000);
        let point;
        if error_first {
            point = read_late_error(&mut state, 3, error);
            offer_answer(&mut state, 1, silent, true, answer);
        } else {
            offer_answer(&mut state, 1, silent, true, answer);
            point = read_late_error(&mut state, 3, error);
        }
        (state, point)
    }

    /// ADR-444: the exchange a late error falls after counts its answer for the exchanges after
    /// it by its stored time, the one test every answer gets (ADR-385): when it is shown after
    /// the error, the later exchange has an answered match and an answer since the error, and
    /// the link has the matched answer; when it is not (before, at the error's time, another
    /// clock, another epoch, no time, an error that could not be placed) nothing counts.
    #[test]
    fn the_exchange_a_late_error_falls_after_counts_its_answer_by_its_stored_time() {
        let (state, silent) = owed_and_later_silence();
        let counted = answer_and_late_error(&silent, state, device_time(0, 5_500), false);
        assert_eq!(counted.1, ErrorPoint::After(1));
        let counted = settlement(&counted.0);
        assert_eq!((counted.same_match, counted.since_error), (1, 1));
        assert!(counted.vehicle && counted.kept_matched, "{counted:?}");
        assert_eq!((counted.link_matched, counted.link_since_error), (1, 1));
        let (clock, placed) = placed_at(Clock::Worker, 0, 5_500);
        let times = [
            device_time(0, 4_000),
            device_time(0, 5_000),
            AnswerTime::At(clock, placed),
            device_time(1, 5_500),
            AnswerTime::Unknown,
        ];
        for time in times {
            let (state, silent) = owed_and_later_silence();
            let (state, point) = answer_and_late_error(&silent, state, time, false);
            assert_eq!(point, ErrorPoint::After(1));
            let not = settlement(&state);
            assert_eq!((not.same_match, not.since_error), (0, 0), "{time:?}");
            assert!(!not.vehicle && !not.kept_matched, "{time:?} {not:?}");
            assert_eq!((not.link_matched, not.link_since_error), (0, 0), "{time:?}");
        }
        // An error that could not be placed.
        let (mut state, silent) = owed_and_later_silence();
        offer_answer(&mut state, 1, &silent, true, device_time(0, 5_500));
        state.error_after(Some(1), 3, LinkErrorKind::Receive, None);
        let not = settlement(&state);
        assert_eq!(
            (not.same_match, not.since_error, not.link_matched),
            (0, 0, 0)
        );
        assert!(!not.vehicle && !not.kept_matched, "{not:?}");
    }

    /// ADR-444: the order the answer and the late error are read in does not change what is
    /// counted, nor the answer's `matched_at`, `after`, `Kept::clears` and `Kept::matched`.
    #[test]
    fn the_order_a_late_answer_and_a_late_error_are_read_in_does_not_change_the_counts() {
        for time in [device_time(0, 5_500), device_time(0, 4_000)] {
            let mut results = Vec::new();
            for error_first in [false, true] {
                let (state, silent) = owed_and_later_silence();
                let (state, _) = answer_and_late_error(&silent, state, time, error_first);
                results.push(settlement(&state));
            }
            if time == device_time(0, 5_500) {
                assert_eq!(results[0], results[1], "{time:?}");
            } else {
                // The `matched_at` of an answer that does not count is left as the order it
                // was read in gave it, and is inert here: nothing in this test reads it, since
                // the answer is neither counted nor offered again. The counts, the window
                // entry's `after` and the record's `Kept::clears` are compared.
                let counts = |result: &Settlement| {
                    (
                        result.same_match,
                        result.since_error,
                        result.vehicle,
                        result.link_matched,
                        result.link_since_error,
                        result.kept_matched,
                        result.after,
                        result.kept_clears,
                    )
                };
                assert_eq!(counts(&results[0]), counts(&results[1]), "{time:?}");
            }
        }
    }

    /// ADR-444: a window entry and its record hold one epoch: `Logged::after` is the record's
    /// `Kept::clears` after a late error placed, unplaced, with the answer before or after it, and
    /// after a restart.
    #[test]
    fn a_late_error_leaves_the_window_entry_and_its_record_on_one_epoch() {
        let one_epoch = |state: &LinkState, what: &str| {
            let first = state.window.iter().find(|logged| logged.seq == 1).unwrap();
            assert_eq!(first.after, state.kept(1).unwrap().clears, "{what}");
        };
        for time in [device_time(0, 5_500), device_time(0, 4_000)] {
            let (state, silent) = owed_and_later_silence();
            let (mut state, _) = answer_and_late_error(&silent, state, time, false);
            one_epoch(&state, &format!("placed {time:?}"));
            state.reset_after(1, 1, true);
            one_epoch(&state, &format!("restart {time:?}"));
        }
        let (mut state, silent) = owed_and_later_silence();
        offer_answer(&mut state, 1, &silent, true, device_time(0, 5_500));
        state.error_after(Some(1), 3, LinkErrorKind::Receive, None);
        one_epoch(&state, "unplaced");
    }

    /// ADR-444: an answer counted after a late error is counted once: a late final response
    /// offered again, a refresh beyond the window and the settlement of an earlier owner
    /// leave the link's matched answers and answers since the error, and the later exchange's
    /// counts, as they were. Each offer is built as the log builds it
    /// ([`ExchangeEntry::settled_answer`]).
    #[test]
    fn an_answer_counted_after_a_late_error_is_not_counted_again_when_offered_again() {
        let reoffer = |state: &mut LinkState, seq: u64| {
            let kept = state.kept(seq).unwrap().clone();
            let mut answer = final_answer(state, seq, &kept, &[0x62, 0xF1, 0x86, 0x41]);
            assert!(!answer.newly_answered && !answer.newly_matched, "{seq}");
            state.judge_answer(&mut answer, kept.answer_at);
            state.reclassify_after(&answer);
        };
        let counts = |state: &LinkState| {
            (
                link_matched(state),
                state.answered_since_error,
                same_match(state, 3),
                window_history(state, 3).answered_since_error,
            )
        };
        let mut state = LinkState::default();
        let owner = answered_and_kept(&mut state, 3_000);
        log_unanswered(&mut state, 3);
        let time = Some(placed_at(Clock::Device, 0, 2_000));
        state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
        let counted = counts(&state);
        assert_eq!((counted.0, counted.2), (1, 1));
        assert!(counted.1 >= 1 && counted.3 >= 1, "{counted:?}");
        // The late final response of the exchange, twice.
        for _ in 0..2 {
            reoffer(&mut state, 2);
            assert_eq!(counts(&state), counted);
        }
        // The earlier owner, its record in place as it stood when it was owed, settles with its
        // own answer: it is before the error, so it adds nothing after it.
        state.keep(1, owner.clone());
        let answer = final_answer(&state, 1, &owner, &[0x62, 0xF1, 0x86, 0x41]);
        state.reclassify_after(&answer);
        assert_eq!(counts(&state), counted);
        // Beyond the window.
        for seq in 4..4 + WINDOW as u64 {
            log_unanswered(&mut state, seq);
        }
        assert!(state.window.iter().all(|logged| logged.seq > 2));
        let before = (link_matched(&state), state.answered_since_error);
        assert!(
            state
                .refresh_kept_beyond_window(2..u64::MAX, Place::Beyond)
                .is_empty()
        );
        assert_eq!((link_matched(&state), state.answered_since_error), before);
        assert!(state.kept(2).unwrap().matched);
        reoffer(&mut state, 2);
        assert_eq!((link_matched(&state), state.answered_since_error), before);
    }

    /// ADR-444: the answer of an exchange whose own entry saw a link error that came after the
    /// answer, or in no known order with it, is not counted after a late error, whatever its stored
    /// time: in the window and beyond it. An error placed within it is such an error.
    #[test]
    fn an_error_of_its_own_entry_ends_the_answer_before_the_stored_time_is_tested() {
        for within in [false, true] {
            for extra in [1, WINDOW as u64] {
                let mut state = LinkState::default();
                answered_and_kept(&mut state, 3_000);
                if within {
                    state.error_in(2, LinkErrorKind::Receive, 1, true, false);
                } else {
                    let logged = state.window.iter_mut().find(|logged| logged.seq == 2);
                    logged.unwrap().evidence.link_errors = vec![LinkErrorKind::Receive];
                    state.kept.get_mut(&2).unwrap().evidence.link_errors =
                        vec![LinkErrorKind::Receive];
                }
                let time = Some(placed_at(Clock::Device, 0, 2_000));
                state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
                for seq in 3..3 + extra {
                    log_unanswered(&mut state, seq);
                }
                let last = state.window.back().unwrap().seq;
                state.reset_after(1, 1, true);
                assert_eq!(
                    (same_match(&state, last), link_matched(&state)),
                    (0, 0),
                    "{within} {extra}"
                );
            }
        }
    }

    /// ADR-444: with two late errors placed after one exchange, its answer counts only when it is
    /// shown after both; one it is not shown after ends it for good.
    #[test]
    fn two_late_errors_after_one_exchange_count_its_answer_only_when_shown_after_both() {
        for (time, counted) in [(5_500, 1), (5_100, 0)] {
            let (state, silent) = owed_and_later_silence();
            let (mut state, point) =
                answer_and_late_error(&silent, state, device_time(0, time), false);
            assert_eq!(point, ErrorPoint::After(1));
            let second = placed_at(Clock::Device, 0, 5_200);
            assert_eq!(read_late_error(&mut state, 4, second), ErrorPoint::After(1));
            let result = settlement(&state);
            assert_eq!(
                (result.same_match, result.link_matched, result.kept_matched),
                (counted, counted, counted == 1),
                "{time}"
            );
        }
    }

    /// A later refresh of the records beyond the window, once exchange `2` has left it, counts
    /// nothing of an answer shown before the late link error placed after it, and leaves the
    /// record's `matched` ended.
    #[test]
    fn a_refresh_beyond_the_window_does_not_count_the_answer_a_late_error_was_placed_after() {
        let mut state = LinkState::default();
        answered_and_kept(&mut state, 1_000);
        let time = Some(placed_at(Clock::Device, 0, 2_000));
        state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
        for seq in 3..3 + WINDOW as u64 {
            log_unanswered(&mut state, seq);
        }
        assert!(state.window.iter().all(|logged| logged.seq > 2));
        state.refresh_kept_beyond_window(2..u64::MAX, Place::Beyond);
        assert!(!state.kept(2).unwrap().matched);
        assert_eq!(link_matched(&state), 0);
    }

    /// An answer of exchange `2` that settles after a late link error placed after it, with the
    /// answer shown before the error: a later item of its own (a final response, at `final_us`)
    /// is a new matched answer, counted once for the link and for a later window exchange,
    /// since the error ended the one the exchange had.
    #[test]
    fn a_later_answer_of_the_exchange_a_late_error_was_placed_after_counts_once() {
        for final_us in [2_500, 3_000] {
            let mut state = LinkState::default();
            answered_and_kept(&mut state, 1_000);
            let time = Some(placed_at(Clock::Device, 0, 2_000));
            state.error_after(Some(2), 100, LinkErrorKind::Receive, time);
            let kept = state.kept(2).unwrap().clone();
            assert!(!kept.matched);
            log_unanswered(&mut state, 3);
            // Exchange 3 is sent after exchange 2's answer item, so it owns nothing of it.
            // `settle_owed(3)` stands for exchange 3 having been released (cancelled, say,
            // with no pending open).
            state.settle_owed(3);
            assert_eq!(same_match(&state, 3), 0);
            let before = link_matched(&state);
            // The final response settles the exchange; its time is after the error.
            state.settle_owed(2);
            let answered = kept.answered(&[0x62, 0xF1, 0x86, 0x41], &LateAnswer::default());
            let history = state.history_settling(&answered, 2);
            let entry = ExchangeEntry::build(2, answered.clone(), history);
            state.settled(2, entry.classification, &entry.reason, answered, true);
            let mut answer = entry.settled_answer(&kept, true);
            let (clock, placed) = placed_at(Clock::Device, 0, final_us);
            state.judge_answer(&mut answer, AnswerTime::At(clock, placed));
            assert!(answer.newly_matched, "{final_us}");
            super::super::settle_later(&mut state, 2, entry.late_restart(), &answer);
            assert_eq!(link_matched(&state), before + 1, "{final_us}");
            assert_eq!(same_match(&state, 3), 1, "{final_us}");
        }
    }

    /// ADR-439: an owed exchange the refresh leaves no row (its answers have no owner left on any
    /// identifier it was owed on) and that is not placed alone, since its exchange-level history
    /// still names an owner, is released with its send confirmed once: the count of unknown sends
    /// goes down by one for it, not two. The entry is made so by hand, as no log of events gives
    /// the two reads different owners here.
    #[test]
    fn an_exchange_emptied_of_rows_lowers_the_unknown_sends_once() {
        let table = vec![RX, RX2];
        let mut state = LinkState::default();
        let mut owner = collecting(vec![lone(RX2)]);
        owner.end = End::NoResponse;
        owner.pending = vec![pending()];
        owner.pending_ids = vec![Some(RX2)];
        owner.rx_id = 0;
        owner.receive_ids = table.clone();
        let (_, kept_owner) = log_kept_owned(&mut state, 1, owner);
        state.keep(1, kept_owner);
        // Final answers on both identifiers, which the owner could own: owed on both through it.
        let mut resend = collecting(vec![
            positive(RX, Window::WithinP2),
            positive(RX2, Window::WithinP2),
        ]);
        resend.rx_id = 0;
        resend.receive_ids = table;
        let (_, mut kept_resend) = log_kept_owned(&mut state, 2, resend);
        kept_resend.sent = Sent::Unknown;
        state.keep(2, kept_resend);
        assert_eq!(state.owed_row_ids(2).len(), 2);
        // The owner's and the resend's sends are unknown, and a third send elsewhere.
        state.kept.get_mut(&1).unwrap().sent = Sent::Unknown;
        state.unknown_sends = 3;
        state.settle_owed(1);
        assert_eq!(state.unknown_sends, 2);
        let kept = state.kept(2).unwrap().clone();
        let mut entry = state.kept_entry(2, &kept.evidence, kept.classification);
        assert!(entry.placed_alone());
        entry.effect.owners = vec![1];
        assert!(!entry.placed_alone());
        let freed = state.free_owner_answer(
            (Sent::Unknown, false),
            &entry,
            false,
            &kept.evidence,
            kept.answer_at,
            (kept.clears, kept.clears),
            Place::Beyond,
        );
        assert!(freed.is_some_and(|freed| freed.final_response));
        assert!(state.kept(2).is_none());
        assert!(state.owed_row_ids(2).is_empty());
        assert_eq!(state.unknown_sends, 1);
    }

    /// ADR-442: an entry is owed through its owners only when it settles and an owner stands:
    /// not when it is placed alone, has no final response, or is open on a pending or silent.
    #[test]
    fn an_entry_is_owed_through_owners_only_when_it_settles_with_an_owner() {
        let mut state = LinkState::default();
        // Alone: no owner stands.
        let alone = log(
            &mut state,
            1,
            collecting(vec![positive(RX, Window::WithinP2)]),
        );
        assert!(alone.placed_alone() && !alone.owed_through_owners());
        // An owner that has no answer.
        let mut state = LinkState::default();
        log(&mut state, 1, collecting(Vec::new()));
        // A final response with the owner standing.
        let settling = log(
            &mut state,
            2,
            collecting(vec![positive(RX, Window::WithinP2)]),
        );
        assert!(settling.owed_through_owners() && !settling.placed_alone());
        // Only owners sent after it could own its answer (ADR-445): it ends the owed request.
        let mut later_only = settling.clone();
        later_only.effect.owners = vec![3];
        assert!(!later_only.owed_through_owners());
        // An owner sent before it and one after it: the earlier one keeps it owed.
        let mut both = settling.clone();
        both.effect.owners = vec![1, 3];
        assert!(both.owed_through_owners());
        // No final response.
        let silent = log(&mut state, 3, collecting(Vec::new()));
        assert!(!silent.owed_through_owners());
        // Open on a pending.
        let mut open = collecting(vec![lone(RX)]);
        open.pending = vec![pending()];
        open.pending_ids = vec![Some(RX)];
        let open = log(&mut state, 4, open);
        assert!(!open.owed_through_owners());
        // A silent identifier beside an answer an owner could own: it does not settle.
        let table = vec![RX, RX2];
        let mut state = LinkState::default();
        let mut owner = collecting(Vec::new());
        owner.rx_id = 0;
        owner.receive_ids = table.clone();
        log(&mut state, 1, owner);
        let mut beside = collecting(vec![positive(RX, Window::WithinP2)]);
        beside.rx_id = 0;
        beside.receive_ids = table;
        let beside = log(&mut state, 2, beside);
        assert!(!beside.effect.owners.is_empty());
        assert!(!beside.settles() && !beside.owed_through_owners());
    }

    /// ADR-445 decision 2: an entry that settles is owed through its owners only when an
    /// identifier is left for them to own. Here an unnamed pending that stands in the answers
    /// (the evidence is not folded) is the only thing the owner could own, and the final answers
    /// on both identifiers are not: the owners are not empty and the entry is owed on no
    /// identifier, so it ends its owed request instead of holding a record with no rows.
    #[test]
    fn an_entry_owed_on_no_identifier_is_not_owed_through_owners() {
        let table = vec![RX, RX2];
        let mut state = LinkState::default();
        let mut owner = collecting(Vec::new());
        owner.rx_id = 0;
        owner.receive_ids = table.clone();
        // It could own a pending or a negative response of the presence request, no positive one.
        owner.expected.positive_prefix = vec![0x62];
        log(&mut state, 1, owner);
        let mut dependent = collecting(vec![
            lone(0),
            positive(RX, Window::WithinP2),
            positive(RX2, Window::WithinP2),
        ]);
        dependent.end = End::Positive(POSITIVE.to_vec());
        dependent.rx_id = 0;
        dependent.receive_ids = table;
        let mut histories = state.history_for(&dependent);
        dependent.set_answer_histories(state.answer_histories(&dependent, &histories, 2));
        histories = state.history_for(&dependent);
        let dependent = ExchangeEntry::build(2, dependent, histories);
        assert!(dependent.settles());
        assert_eq!(dependent.effect.owners, [1]);
        assert!(dependent.effect.owed_ids.is_empty());
        assert!(!dependent.owed_through_owners());
    }

    /// ADR-439: the refresh after an accepted restart changes rows and records only; it does not
    /// count a matched answer or note it in the record (the restart recounts them from the
    /// window), so the exchange's later final response still counts as one.
    #[test]
    fn an_accepted_restart_leaves_the_matched_answers_of_kept_exchanges_uncounted() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = forwarding(evidence(request, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let mut state = LinkState::default();
        let (_, first) = log_kept(&mut state, 1, confirmed(&READ_F186, End::NoResponse));
        state.keep(1, first);
        let (_, reset) = log_kept(&mut state, 2, confirmed(&[0x10, 0x03], End::NoResponse));
        state.keep(2, reset);
        let mut pending_only_exchange = confirmed(&READ_F186, End::NoResponse);
        pending_only_exchange.pending = vec![pending()];
        let (_, third) = log_kept_owned(&mut state, 3, pending_only_exchange);
        state.keep(3, third);
        for seq in 4..4 + WINDOW as u64 {
            log_kept(
                &mut state,
                seq,
                confirmed(
                    &[0x22, 0xF1, 0x88],
                    End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                ),
            );
        }
        assert!(state.window.iter().all(|logged| logged.seq > 3));
        state.reset_after(2, 2, true);
        let kept = state.kept(3).cloned().expect("a pending leaves it owed");
        assert!(!kept.matched);
        let own = final_answer(&state, 3, &kept, &[0x62, 0xF1, 0x86, 0x41]);
        assert!(own.newly_matched);
    }

    /// ADR-439: an exchange beyond the window is not classified again: what its entry reads of
    /// its classification (a suppressed ECUReset accepted when classified `none`) is the kept
    /// classification, floor included, not one derived from the live counts.
    #[test]
    fn an_exchange_beyond_the_window_keeps_its_classification_for_the_refresh() {
        let confirmed = |request: &[u8], end| {
            let mut evidence = forwarding(evidence(request, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let mut state = LinkState::default();
        let mut reset = confirmed(&[0x11, 0x81], End::NoResponse);
        reset.suppressed = true;
        let (entry, mut kept) = log_kept(&mut state, 1, reset);
        assert_ne!(entry.classification, Classification::None);
        assert!(state.owed.iter().any(|owed| owed.seq == 1));
        // The classification the exchange was kept with (set directly, as the `none` row needs a
        // judged timeout): it was accepted as a reset, so it is placed alone.
        kept.classification = Classification::None;
        state.keep(1, kept);
        for seq in 2..2 + WINDOW as u64 {
            log_kept(
                &mut state,
                seq,
                confirmed(
                    &[0x22, 0xF1, 0x88],
                    End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                ),
            );
        }
        assert!(state.window.iter().all(|logged| logged.seq > 1));
        let freed = state.refresh_kept_beyond_window(1..u64::MAX, Place::Beyond);
        assert!(
            freed.iter().all(|answer| answer.final_response),
            "{freed:?}"
        );
        assert_eq!(freed.len(), 1);
        assert!(state.kept(1).is_none());
    }

    /// ADR-439: the kept exchanges are visited in `seq` order, so a chain of answered exchanges
    /// beyond the window, each one owed through the ones before it, is released in one pass when
    /// an accepted restart ends the first one's owner (visited out of order, each would still
    /// see the rows of those not yet visited).
    #[test]
    fn a_chain_of_kept_exchanges_is_released_in_one_pass_after_an_accepted_restart() {
        let count = 14;
        let (mut state, _) = owners_and_answered_beyond_window(count, |_, _| {}, false);
        assert!((2..2 + count as u64).all(|seq| state.kept(seq).is_some()));
        state.reset_after(1, 2, true);
        assert!(
            (2..2 + count as u64).all(|seq| state.kept(seq).is_none()),
            "{:?}",
            (2..2 + count as u64)
                .filter(|seq| state.kept(*seq).is_some())
                .collect::<Vec<_>>()
        );
    }

    /// Design 8.11 "Settlement": a pending of a later exchange that the earlier owed request
    /// could own becomes a matched answer once that request has its response, for the exchanges
    /// after it, though its exchange stays owed; and a floor an ambiguous response set stands.
    #[test]
    fn a_settled_answer_reaches_on_through_the_answers_it_makes_unambiguous() {
        let confirmed = |end| {
            let mut evidence = forwarding(evidence(&READ_F186, end));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let answer: &[u8] = &[0x62, 0xF1, 0x86, 0x41];
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        // A read of another identifier that got a pending, which the first could own, then
        // none; and a silent read of that identifier.
        let mut pending_only = forwarding(read([0xF1, 0x87], End::NoResponse));
        pending_only.transmit_evidence = Some(TransmitEvidence::TxIndication);
        pending_only.pending = vec![pending()];
        let (second, _) = log_kept(&mut state, 2, pending_only);
        assert_eq!(second.link_history.owed_matching, 1);
        let mut silent = judged(forwarding(read([0xF1, 0x87], End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let (third, _) = log_kept(&mut state, 3, silent);
        assert_eq!(third.classification, Classification::Undetermined);
        let (changed, _) = state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        let third = changed.iter().find(|later| later.seq == 3);
        assert_eq!(
            third.map(|later| later.classification),
            Some(Classification::Vehicle),
            "{changed:?}"
        );
        assert!(
            state.owed.iter().any(|owed| owed.seq == 2),
            "a pending places nothing alone"
        );

        // That exchange's own final response, unique to it, is a matched answer at last: its
        // pending never counted, so a silent read logged after it is `vehicle`.
        let mut state = LinkState::default();
        log_kept(&mut state, 1, confirmed(End::NoResponse));
        let mut pending_only = forwarding(read([0xF1, 0x87], End::NoResponse));
        pending_only.transmit_evidence = Some(TransmitEvidence::TxIndication);
        pending_only.pending = vec![pending()];
        let (_, kept_second) = log_kept(&mut state, 2, pending_only);
        assert!(!kept_second.matched);
        let own = final_answer(&state, 2, &kept_second, &[0x62, 0xF1, 0x87, 0x41]);
        assert!(own.newly_matched);
        state.reclassify_after(&own);
        let mut silent = judged(forwarding(read([0xF1, 0x87], End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let (after, _) = log_kept(&mut state, 3, silent);
        assert_eq!(after.classification, Classification::Vehicle);

        // A freed answer its own exchange saw a link error after keeps that order: the error
        // ended it, so it is no matched answer for the silent read after it.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        let mut errored = confirmed(End::Positive(answer.to_vec()));
        errored.link_errors = vec![LinkErrorKind::Receive];
        log_kept(&mut state, 2, errored);
        log_kept(&mut state, 3, confirmed(End::NoResponse));
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert!(
            state
                .window
                .iter()
                .all(|logged| logged.seq != 3 || logged.classification != Classification::Vehicle),
            "{:?}",
            state.window
        );

        // A pending made a matched answer this way counts once: its exchange's final response
        // later is no new matched answer.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        state.keep(1, kept.clone());
        let mut pending_only = forwarding(read([0xF1, 0x87], End::NoResponse));
        pending_only.transmit_evidence = Some(TransmitEvidence::TxIndication);
        pending_only.pending = vec![pending()];
        let (_, kept_second) = log_kept(&mut state, 2, pending_only);
        state.keep(2, kept_second);
        state.settle_owed(1);
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        let kept_second = state.kept(2).cloned().expect("a pending leaves it owed");
        assert!(kept_second.matched);
        let own = final_answer(&state, 2, &kept_second, &[0x62, 0xF1, 0x87, 0x41]);
        assert!(!own.newly_matched);

        // An owed exchange an ambiguous response raised is still freed by its owner's answer:
        // owed no more, and still at least `undetermined`.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        let mut negative = forwarding(read([0xF1, 0x87], End::Negative(vec![0x7F, 0x22, 0x31])));
        negative.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept(&mut state, 2, negative);
        assert!(state.owed.iter().any(|owed| owed.seq == 2));
        state.raise(2, Classification::Undetermined, "raised", " raised");
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert!(
            !state.owed.iter().any(|owed| owed.seq == 2),
            "{:?}",
            state.owed
        );
        assert!(
            state
                .window
                .iter()
                .all(|logged| logged.seq != 2
                    || logged.classification >= Classification::Undetermined)
        );

        // A pending a non-final settlement added, which the earlier owed request could own, is
        // in the window's history: once that request has its response, it is a matched answer.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        let mut waited = forwarding(read([0xF1, 0x87], End::VciDidNotEnd));
        waited.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept(&mut state, 2, waited.clone());
        let mut ended = waited;
        ended.end = End::NoResponse;
        ended.pending = vec![pending()];
        state.settled(2, Classification::Undetermined, "ended", ended, false);
        assert_eq!(
            state
                .window
                .iter()
                .find(|logged| logged.seq == 2)
                .map(|logged| logged.history.owners.clone()),
            Some(vec![1])
        );
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        let mut silent = judged(forwarding(read([0xF1, 0x87], End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let (after, _) = log_kept(&mut state, 3, silent);
        assert_eq!(after.classification, Classification::Vehicle);

        // A freed answer confirms its exchange's send: that exchange can own a late transmit
        // indication no more.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        let unconfirmed = forwarding(read([0xF1, 0x87], End::Negative(vec![0x7F, 0x22, 0x31])));
        let (second, _) = log_kept(&mut state, 2, unconfirmed);
        assert_eq!(second.sent, Sent::Unknown);
        assert_eq!(state.unknown_sends, 1);
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert_eq!(state.unknown_sends, 0);

        // An exchange a final response settled keeps its classification, but its answer is
        // freed once its owner has its response.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        let mut waited = forwarding(read([0xF1, 0x87], End::VciDidNotEnd));
        waited.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept(&mut state, 2, waited.clone());
        let mut ended = waited;
        ended.end = End::Negative(vec![0x7F, 0x22, 0x31]);
        state.settle_owed(2);
        state.settled(2, Classification::System, "ended", ended, true);
        state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert!(
            state
                .window
                .iter()
                .all(|logged| logged.seq != 2 || logged.classification == Classification::System)
        );
        let mut silent = judged(forwarding(read([0xF1, 0x87], End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let (after, _) = log_kept(&mut state, 3, silent);
        assert_eq!(after.classification, Classification::Vehicle);

        // A later answer an ambiguous response may have been is not cleared by its history.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, confirmed(End::NoResponse));
        log_kept(&mut state, 2, confirmed(End::Positive(answer.to_vec())));
        state.floor(2, Classification::Undetermined, " floor");
        let (changed, _) = state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert!(
            changed
                .iter()
                .all(|later| later.classification == Classification::Undetermined),
            "{changed:?}"
        );
        assert!(
            state
                .window
                .iter()
                .all(|logged| logged.classification >= Classification::Undetermined)
        );
    }

    /// A time on `clock` at `unwrapped` microseconds in epoch `epoch`.
    fn placed_at(
        clock: Clock,
        epoch: u32,
        unwrapped: i64,
    ) -> (Clock, super::super::clocks::Placed) {
        (clock, super::super::clocks::Placed { epoch, unwrapped })
    }

    /// `evidence` with its VCI times placed on the device clock, epoch 0: the transmit done and
    /// the response's end, each when given.
    fn timed(mut evidence: Evidence, tx_done: Option<i64>, rx_end: Option<i64>) -> Evidence {
        let time = |us: i64| {
            VciTime::new(us as u64, Some(Clock::Device)).at(Some(super::super::clocks::Placed {
                epoch: 0,
                unwrapped: us,
            }))
        };
        evidence.times.vci = Some(VciTimes {
            tx_done: tx_done.map(time),
            rx_end: rx_end.map(time),
            ..VciTimes::default()
        });
        evidence
    }

    /// ADR-384: a late link error falls within an exchange from its earliest placed transmit done
    /// or response start; an exchange with only end-side times (a single-frame response with no
    /// transmit indication) has no start, so an error after its latest time is after it, one
    /// after the exchange before and not after that time may have come during it, and an exchange
    /// with no time on the error's clock and epoch is taken as after the error, as before.
    #[test]
    fn an_exchange_with_only_end_side_times_may_hold_a_late_link_error() {
        let answer: &[u8] = &[0x62, 0xF1, 0x86, 0x41];
        let answered = || forwarding(evidence(&READ_F186, End::Positive(answer.to_vec())));
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(answered(), Some(1_000), Some(1_200)));
        log_kept(&mut state, 2, timed(answered(), None, Some(2_300)));
        let at = |unwrapped| placed_at(Clock::Device, 0, unwrapped);
        // Within the first exchange's span: the exchange before the one with end-side times
        // only holds it.
        assert_eq!(state.place_error(at(1_100)), ErrorPoint::Within(1));
        // After the first exchange's latest time, not after the second's: it may have come
        // during the second, or between them.
        assert_eq!(state.place_error(at(1_500)), ErrorPoint::MaybeWithin(2));
        assert_eq!(state.place_error(at(2_300)), ErrorPoint::MaybeWithin(2));
        // After the second's latest time: after it, not after the exchange before.
        assert_eq!(state.place_error(at(2_301)), ErrorPoint::After(2));
        // Before every start: before the window.
        assert_eq!(
            state.place_error(at(500)),
            ErrorPoint::BeforeWindow { within: None }
        );
        // The second exchange's times are on another epoch of the clock: it has no time on the
        // error's clock and epoch, so the error is placed against the first, and the second is
        // taken as after it.
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(answered(), Some(1_000), Some(1_200)));
        let mut elsewhere = timed(answered(), None, Some(2_300));
        if let Some(vci) = &mut elsewhere.times.vci {
            vci.rx_end = vci.rx_end.map(|time| {
                time.at(Some(super::super::clocks::Placed {
                    epoch: 1,
                    unwrapped: 2_300,
                }))
            });
        }
        log_kept(&mut state, 2, elsewhere);
        assert_eq!(state.place_error(at(1_500)), ErrorPoint::After(1));
        // No exchange with a time on the clock at all: not placed.
        assert_eq!(
            state.place_error(placed_at(Clock::Worker, 0, 1_500)),
            ErrorPoint::NotPlaced
        );
        // Only end-side times in the whole window: they place the error too.
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(answered(), None, Some(2_300)));
        assert_eq!(state.place_error(at(2_000)), ErrorPoint::MaybeWithin(1));
        assert_eq!(state.place_error(at(2_400)), ErrorPoint::After(1));
    }

    /// ADR-384: an answer a settling entry adds after a late link error counts as one since the
    /// error, and as a matched answer for the exchanges after it, only when its time is shown
    /// after the error's on the same clock and epoch, or no such error applies to its exchange;
    /// otherwise it counts as answered only. A pending that counts as a matched answer follows
    /// the same rule.
    #[test]
    fn a_late_answer_counts_since_a_late_error_only_when_shown_after_it() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let device = |epoch, unwrapped| Some(placed_at(Clock::Device, epoch, unwrapped));
        // The first exchange is owed, a silent one follows it, and a late link error falls after
        // the first (placed at 2 000 us on the device clock, epoch 0, or not placed). Then its
        // answer is read, at `answered_at`: what the later exchange gets.
        let run = |error: Option<(Clock, super::super::clocks::Placed)>,
                   answered_at: Option<(Clock, super::super::clocks::Placed)>,
                   pending: bool| {
            let mut state = LinkState::default();
            let (_, kept) = log_kept(&mut state, 1, silent.clone());
            state.keep(1, kept);
            let (later, _) = log_kept(&mut state, 2, silent.clone());
            assert_eq!(later.classification, Classification::Undetermined);
            state.error_after(Some(1), 3, LinkErrorKind::Receive, error);
            let kept = state.kept(1).unwrap().clone();
            // The answer is a matched answer, as the settling entry's own classification of the
            // first exchange gives it; a pending is no final response.
            let mut settled = SettledAnswer {
                seq: 1,
                key: MatchKey::of(&silent),
                newly_answered: true,
                newly_matched: true,
                final_response: !pending,
                clears: kept.clears,
                since_error: true,
                settled_ids: Vec::new(),
            };
            settled.set_since_error(state.since_late_error(1, answered_at.into()));
            let (changed, _) = state.reclassify_after(&settled);
            let later = changed
                .iter()
                .find(|later| later.seq == 2)
                .map(|later| later.classification);
            (later, state.answered, state.answered_since_error)
        };
        let error = Some(placed_at(Clock::Device, 0, 2_000));
        for pending in [false, true] {
            // Shown after the error: it counts, and the later exchange has a matched answer.
            assert_eq!(
                run(error, device(0, 3_000), pending),
                (Some(Classification::Vehicle), 1, 1),
                "pending {pending}"
            );
            // Shown before it, on another clock, on another epoch, with no time, or the error
            // not placed: answered only, and no matched answer.
            for (error, answered_at) in [
                (error, device(0, 1_500)),
                (error, device(0, 2_000)),
                (error, Some(placed_at(Clock::Worker, 0, 3_000))),
                (error, device(1, 3_000)),
                (error, None),
                (None, device(0, 3_000)),
            ] {
                assert_eq!(
                    run(error, answered_at, pending),
                    (None, 1, 0),
                    "pending {pending} {error:?} {answered_at:?}"
                );
            }
        }
        // An error read when exchanges 1 and 2 are known may come after either one's answer, so
        // neither counts one with no time since it, though the error falls after exchange 1
        // (ADR-384); an exchange logged after the error was read does. An error not placed does
        // not apply to an exchange logged after it either.
        let mut state = LinkState::default();
        log_kept(&mut state, 1, silent.clone());
        log_kept(&mut state, 2, silent.clone());
        state.error_after(Some(1), 3, LinkErrorKind::Receive, error);
        assert!(!state.since_late_error(2, AnswerTime::None));
        assert!(!state.since_late_error(1, AnswerTime::None));
        assert!(state.since_late_error(3, AnswerTime::None));
        assert!(state.since_late_error(2, device(0, 9_000).into()));
        let mut state = LinkState::default();
        state.error_after(Some(1), 3, LinkErrorKind::Receive, None);
        assert!(!state.since_late_error(2, device(0, 9_000).into()));
        assert!(state.since_late_error(3, AnswerTime::None));
    }

    /// ADR-384: a late link error that falls before the window's exchanges ends the matched
    /// answers from before it: every window exchange is after it, counts again from the window
    /// alone and is reclassified, so one that rested on an answer of an exchange that has left
    /// the window loses it. The window's answers are timed after the error: an answer logged
    /// before the error was read counts since it only when its time is shown after it (ADR-385).
    #[test]
    fn a_link_error_before_the_window_reclassifies_the_windows_exchanges() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut answered = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 0x41]),
        )));
        answered.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut other = judged(forwarding(evidence(
            &[0x22, 0xF1, 0x87],
            End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
        )));
        other.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(answered, Some(1_000), Some(1_200)));
        let (rests, _) = log_kept(&mut state, 2, timed(silent, Some(2_000), None));
        assert_eq!(rests.classification, Classification::Vehicle);
        // Fill the window so that the first exchange leaves it.
        for seq in 3..=u64::try_from(WINDOW).unwrap() + 1 {
            let us = 10_000 * i64::try_from(seq).unwrap();
            log_kept(
                &mut state,
                seq,
                timed(other.clone(), Some(us), Some(us + 200)),
            );
        }
        assert_eq!(state.left_window, Some(1));
        let error = placed_at(Clock::Device, 0, 500);
        assert_eq!(
            state.place_error(error),
            ErrorPoint::BeforeWindow { within: None }
        );
        let (changed, not_reclassified) =
            state.error_after(None, 99, LinkErrorKind::Receive, Some(error));
        let [reclassified] = &changed[..] else {
            panic!("{changed:?}");
        };
        assert_eq!(reclassified.seq, 2);
        assert_eq!(reclassified.classification, Classification::Undetermined);
        // What left the window is named: it cannot be told whether it is after the error.
        assert_eq!(not_reclassified, Some(1));
        // The counts start again from the window alone: the answers of its other exchanges.
        assert_eq!(state.answered_since_error, 15);
    }

    /// ADR-384: an error placed after an earlier exchange (one with no time on the clock at its
    /// entry is taken as after the error) does not make the late answer of that exchange count
    /// since the error when the answer is shown at or before the error on the error's clock and
    /// epoch; an answer that cannot be set against it keeps the rule by position.
    #[test]
    fn a_late_answer_shown_before_an_error_placed_before_its_exchange_is_answered_only() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut answered = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 0x41]),
        )));
        answered.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let device = |unwrapped| Some(placed_at(Clock::Device, 0, unwrapped));
        // Exchange 1 has times, exchange 2 is owed and has none, exchange 3 is silent with the
        // same match and transmits late. The error at 5 000 falls after exchange 1.
        let run = |error_at: i64, answered_at: Option<(Clock, super::super::clocks::Placed)>| {
            let mut state = LinkState::default();
            log_kept(
                &mut state,
                1,
                timed(answered.clone(), Some(1_000), Some(1_200)),
            );
            let (_, kept) = log_kept(&mut state, 2, silent.clone());
            state.keep(2, kept);
            log_kept(&mut state, 3, timed(silent.clone(), Some(6_000), None));
            let at = placed_at(Clock::Device, 0, error_at);
            let point = state.place_error(at);
            let after = match point {
                ErrorPoint::After(seq) | ErrorPoint::Within(seq) => Some(seq),
                ErrorPoint::BeforeWindow { .. } => None,
                other => panic!("{other:?}"),
            };
            state.error_after(after, 4, LinkErrorKind::Receive, Some(at));
            let kept = state.kept(2).unwrap().clone();
            let mut settled = SettledAnswer {
                seq: 2,
                key: MatchKey::of(&silent),
                newly_answered: true,
                newly_matched: true,
                final_response: true,
                clears: kept.clears,
                since_error: true,
                settled_ids: Vec::new(),
            };
            settled.set_since_error(state.since_late_error(2, answered_at.into()));
            let (changed, _) = state.reclassify_after(&settled);
            let later = changed
                .iter()
                .find(|later| later.seq == 3)
                .map(|later| later.classification);
            (point, later, state.answered_since_error)
        };
        // Placed after exchange 1: exchange 2's answer at 4 000 is before the error.
        let (point, later, since) = run(5_000, device(4_000));
        assert_eq!(point, ErrorPoint::After(1));
        assert_eq!((later, since), (None, 0));
        // After the error: it counts.
        let (_, later, since) = run(5_000, device(5_500));
        assert_eq!((later, since), (Some(Classification::Vehicle), 1));
        // Not set against it: its exchange was known when the error was read, and nothing shows
        // its answer came after (its position alone, taken as after, does not).
        let (_, later, since) = run(5_000, None);
        assert_eq!((later, since), (None, 0));
        // Before the window: the untimed exchange in the window after the error is the same.
        let (point, later, since) = run(500, device(400));
        assert_eq!(point, ErrorPoint::BeforeWindow { within: None });
        // (exchange 1's own answer counts since the error either way)
        assert_eq!((later, since), (None, 1));
        let (_, _, since) = run(500, device(600));
        assert_eq!(since, 2);
    }

    /// A late link error read as the log reads it (`Inner::late_link_error`): placed among the
    /// link's exchanges, taken by the exchange it falls within, and the counts after its point
    /// start again. Gives where it was placed.
    fn read_late_error(
        state: &mut LinkState,
        written: u64,
        error: (Clock, super::super::clocks::Placed),
    ) -> ErrorPoint {
        let point = state.place_error(error);
        let after = match point {
            ErrorPoint::After(seq) => Some(seq),
            ErrorPoint::Within(seq) | ErrorPoint::MaybeWithin(seq) => {
                let placed = matches!(point, ErrorPoint::Within(_));
                state.error_in(seq, LinkErrorKind::Receive, 1, placed, false);
                Some(seq)
            }
            ErrorPoint::BeforeWindow { within } => {
                if let Some(within) = within {
                    state.error_in(within, LinkErrorKind::Receive, 1, true, false);
                }
                None
            }
            ErrorPoint::NotPlaced => state.window_last(),
        };
        let time = (point != ErrorPoint::NotPlaced).then_some(error);
        state.error_after(after, written, LinkErrorKind::Receive, time);
        point
    }

    /// A settling entry offers the answer of owed exchange `seq` (a matched answer of `key`'s
    /// evidence, a final response or a pending) at `time`. Gives whether it counts since the
    /// last link error.
    fn offer_answer(
        state: &mut LinkState,
        seq: u64,
        key: &Evidence,
        pending: bool,
        time: AnswerTime,
    ) -> bool {
        let kept = state.kept(seq).unwrap().clone();
        let mut answer = SettledAnswer {
            seq,
            key: MatchKey::of(key),
            newly_answered: true,
            newly_matched: true,
            final_response: !pending,
            clears: kept.clears,
            since_error: true,
            settled_ids: Vec::new(),
        };
        state.judge_answer(&mut answer, time);
        let since = answer.since_error;
        // The settling entry's evidence gives the exchange an answer of its own.
        if let Some(logged) = state.window.iter_mut().find(|logged| logged.seq == seq) {
            logged.history.this_exchange = true;
        }
        state.reclassify_after(&answer);
        since
    }

    fn device_time(epoch: u32, us: i64) -> AnswerTime {
        let (clock, placed) = placed_at(Clock::Device, epoch, us);
        AnswerTime::At(clock, placed)
    }

    /// An answered exchange 1 with times (another match), an owed silent exchange 2 with none,
    /// and a silent exchange 3 of exchange 2's match, which transmits at 6 000 us on the device
    /// clock. Returns the link state and exchange 2's evidence.
    fn late_error_scenario() -> (LinkState, Evidence) {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut other = judged(forwarding(evidence(
            &[0x22, 0xF1, 0x87],
            End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
        )));
        other.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(other, Some(1_000), Some(1_200)));
        let (_, kept) = log_kept(&mut state, 2, silent.clone());
        state.keep(2, kept);
        log_kept(&mut state, 3, timed(silent.clone(), Some(6_000), None));
        (state, silent)
    }

    /// ADR-385: an owed exchange whose matched answer a late link error's recount leaves out has
    /// none counted any more, so its later final response, shown after the error, is a new
    /// matched answer and is tested by its own time rather than passed over.
    #[test]
    fn a_later_answer_after_a_recount_left_the_first_out_is_tested_by_its_own_time() {
        for (late_us, counts) in [(7_000, true), (4_500, false)] {
            // Exchange 2 alone can own its answers: no later exchange of its match is owed.
            let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
            silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let mut other = judged(forwarding(evidence(
                &[0x22, 0xF1, 0x87],
                End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
            )));
            other.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let mut state = LinkState::default();
            log_kept(&mut state, 1, timed(other, Some(1_000), Some(1_200)));
            let (_, kept) = log_kept(&mut state, 2, silent.clone());
            state.keep(2, kept);
            // A pending with no placed time counts as a matched answer before any error.
            assert!(offer_answer(
                &mut state,
                2,
                &silent,
                true,
                AnswerTime::Unknown
            ));
            assert!(state.kept(2).unwrap().matched);
            // The late error leaves it out: its time is unknown.
            read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
            assert!(!state.kept(2).unwrap().matched, "{late_us}");
            // Exchange 2's final response, settled through the real path.
            let kept = state.kept(2).unwrap().clone();
            let response = kept.answered(&[0x62, 0xF1, 0x86, 0x41], &LateAnswer::default());
            let history = state.history_settling(&response, 2);
            let entry = ExchangeEntry::build(2, response.clone(), history);
            let mut answer = entry.settled_answer(&kept, true);
            assert!(answer.newly_matched, "{late_us}");
            let time =
                kept.answer_time(Some(Some(placed_at(Clock::Device, 0, late_us))), &[], None);
            state.judge_answer(&mut answer, time);
            assert_eq!(answer.since_error, counts, "{late_us}");
            assert_eq!(answer.newly_matched, counts, "{late_us}");
            assert_eq!(state.kept(2).unwrap().answer_at, device_time(0, late_us));
        }
    }

    /// ADR-385: an owed exchange's final response, settled after its pending already counted as
    /// a matched answer, stores its time, so a late error read afterwards tests it by that time:
    /// the outcome does not depend on which of the two was read first.
    #[test]
    fn a_later_answer_settled_before_the_error_is_read_is_tested_by_its_own_time() {
        for (late_us, counts) in [(7_000, true), (4_500, false)] {
            let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
            silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let mut other = judged(forwarding(evidence(
                &[0x22, 0xF1, 0x87],
                End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
            )));
            other.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let mut state = LinkState::default();
            log_kept(&mut state, 1, timed(other, Some(1_000), Some(1_200)));
            // Its entry holds a forwarded pending with no clock: a matched answer at the entry,
            // whose time is unknown.
            let mut pended = silent.clone();
            pended.pending = vec![Pending {
                vci_us: None,
                clock: None,
                agent_us: 3_000,
                epoch: None,
                since_previous_ms: None,
            }];
            let (entry, kept) = log_kept(&mut state, 2, pended);
            assert!(entry.matched_answer() && kept.matched);
            assert_eq!(kept.answer_at, AnswerTime::Unknown);
            state.keep(2, kept);
            // The final response first: it counts as nothing new, but its time is stored.
            let kept = state.kept(2).unwrap().clone();
            let response = kept.answered(&[0x62, 0xF1, 0x86, 0x41], &LateAnswer::default());
            let history = state.history_settling(&response, 2);
            let entry = ExchangeEntry::build(2, response, history);
            let mut answer = entry.settled_answer(&kept, true);
            assert!(!answer.newly_answered && !answer.newly_matched);
            let time =
                kept.answer_time(Some(Some(placed_at(Clock::Device, 0, late_us))), &[], None);
            state.judge_answer(&mut answer, time);
            assert_eq!(state.kept(2).unwrap().answer_at, device_time(0, late_us));
            // Then the error, at 5 000: the recount tests the stored time.
            read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
            let logged = state.window.iter().find(|logged| logged.seq == 2).unwrap();
            assert_eq!(state.counts_since_error(logged), counts, "{late_us}");
        }
    }

    /// ADR-385: a late link error is placed by the entries' own times, not by an answer's stored
    /// time: an answer that came after a later exchange began does not stretch its exchange over
    /// that one, so an error the later exchange may have held is still its own possible one.
    #[test]
    fn a_stored_answer_time_does_not_stretch_its_exchange_over_a_later_one() {
        let mut owed = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        owed.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut single = judged(forwarding(evidence(
            &[0x22, 0xF1, 0x87],
            End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
        )));
        single.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 2, timed(owed.clone(), Some(1_000), None));
        state.keep(2, kept);
        // Exchange 2's late response at 7 000 is stored.
        assert!(offer_answer(
            &mut state,
            2,
            &owed,
            false,
            device_time(0, 7_000)
        ));
        // Exchange 3 has only the end of its single-frame response, at 6 500.
        log_kept(&mut state, 3, timed(single, None, Some(6_500)));
        assert_eq!(
            state.place_error(placed_at(Clock::Device, 0, 6_200)),
            ErrorPoint::MaybeWithin(3)
        );
    }

    /// ADR-385: the answer of an owed exchange, settled before a late link error was read, is
    /// tested against the error by its stored time when the error is read. Only one shown after
    /// the error counts, as a matched answer for the exchange after it; one at or before it, on
    /// another clock or epoch, with no time, or with the error not placed, does not. The stored
    /// time does not place the error.
    #[test]
    fn a_late_error_tests_the_stored_time_of_an_answer_settled_before_it_was_read() {
        let worker = |us| {
            let (clock, placed) = placed_at(Clock::Worker, 0, us);
            AnswerTime::At(clock, placed)
        };
        let at = |us| placed_at(Clock::Device, 0, us);
        // (error, answer, where the error is placed, whether the later silent exchange gets the
        // matched answer)
        // The error is placed by the entries' own times alone: exchange 2, with none, is taken
        // as after it, and only its stored answer time decides.
        let table = [
            // Rows 2 and 5: the answer is before the error, or at its time.
            (
                at(5_000),
                device_time(0, 4_000),
                ErrorPoint::After(1),
                false,
            ),
            (
                at(5_000),
                device_time(0, 5_000),
                ErrorPoint::After(1),
                false,
            ),
            // Shown after the error, it counts.
            (at(5_000), device_time(0, 5_500), ErrorPoint::After(1), true),
            // Row 3: no time that can be set against the error.
            (at(5_000), worker(4_000), ErrorPoint::After(1), false),
            (
                at(5_000),
                device_time(1, 4_000),
                ErrorPoint::After(1),
                false,
            ),
            (at(5_000), AnswerTime::Unknown, ErrorPoint::After(1), false),
            // Row 1: before every exchange with a start, the answer shown after the error counts;
            // one shown before it does not.
            (
                at(500),
                device_time(0, 4_000),
                ErrorPoint::BeforeWindow { within: None },
                true,
            ),
            (
                at(500),
                device_time(0, 400),
                ErrorPoint::BeforeWindow { within: None },
                false,
            ),
        ];
        for pending in [false, true] {
            for (error, answer, expected, matched) in &table {
                let (mut state, silent) = late_error_scenario();
                assert!(offer_answer(&mut state, 2, &silent, pending, *answer));
                assert_eq!(
                    window_history(&state, 3).answered_same_match,
                    1,
                    "{pending} {error:?} {answer:?}: the answer counted when it was read"
                );
                let point = read_late_error(&mut state, 4, *error);
                assert_eq!(point, *expected, "{pending} {error:?} {answer:?}");
                let y = state.window.iter().find(|logged| logged.seq == 3).unwrap();
                assert_eq!(
                    y.history.answered_same_match,
                    u32::from(*matched),
                    "{pending} {error:?} {answer:?}"
                );
                assert_eq!(
                    y.classification == Classification::Vehicle,
                    *matched,
                    "{pending} {error:?} {answer:?}"
                );
            }
            // Row 6: an error on a clock no exchange has is not placed. It is taken as after the
            // last exchange, which it does not reclassify, and no exchange known when it was read
            // counts an answer since it.
            let (mut state, silent) = late_error_scenario();
            offer_answer(&mut state, 2, &silent, pending, device_time(0, 4_000));
            let error = placed_at(Clock::Worker, 0, 5_000);
            assert_eq!(read_late_error(&mut state, 4, error), ErrorPoint::NotPlaced);
            assert!(!state.since_late_error(2, device_time(0, 9_000)));
            assert!(!state.since_late_error(3, AnswerTime::Unknown));
            assert_eq!(state.answered_since_error, 0);
        }
    }

    /// ADR-385: an answer settled after the error was read is judged by its own time (ADR-384
    /// item 2), and is judged again with the time stored with the exchange when it is offered
    /// again. Row 7.
    #[test]
    fn a_late_answer_settled_after_an_error_was_read_is_judged_by_its_time_alone() {
        for pending in [false, true] {
            for (answer, counts) in [
                (device_time(0, 4_000), false),
                (device_time(0, 5_000), false),
                (device_time(0, 5_500), true),
                (AnswerTime::Unknown, false),
            ] {
                let (mut state, silent) = late_error_scenario();
                let point = read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
                assert_eq!(point, ErrorPoint::After(1));
                assert_eq!(
                    offer_answer(&mut state, 2, &silent, pending, answer),
                    counts,
                    "{pending} {answer:?}"
                );
                assert_eq!(
                    window_history(&state, 3).answered_same_match,
                    u32::from(counts),
                    "{pending} {answer:?}"
                );
                // The same answer offered again with no item of its own keeps its result.
                let kept = state.kept(2).unwrap().clone();
                assert_eq!(kept.answer_time(None, &[], None), answer);
            }
        }
    }

    /// ADR-385: a start alone counts as an answer, and since the last link error only when shown
    /// after it; it is never a matched answer. Row 8.
    #[test]
    fn a_late_start_alone_counts_since_an_error_only_when_shown_after_it() {
        for (start, since) in [
            (device_time(0, 4_000), false),
            (device_time(0, 5_500), true),
        ] {
            let (mut state, silent) = late_error_scenario();
            read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
            let (answered, answered_since) = (state.answered, state.answered_since_error);
            let kept = state.kept(2).unwrap().clone();
            let mut answer = SettledAnswer {
                seq: 2,
                key: MatchKey::of(&silent),
                newly_answered: true,
                newly_matched: false,
                final_response: false,
                clears: kept.clears,
                since_error: true,
                settled_ids: Vec::new(),
            };
            let time = kept.answer_time(
                None,
                &[],
                Some(Some(match start {
                    AnswerTime::At(clock, placed) => (clock, placed),
                    _ => unreachable!(),
                })),
            );
            assert_eq!(time, start);
            state.judge_answer(&mut answer, time);
            assert_eq!(answer.since_error, since);
            state.reclassify_after(&answer);
            assert_eq!(state.answered, answered + 1);
            assert_eq!(
                state.answered_since_error,
                answered_since + u32::from(since)
            );
            assert_eq!(window_history(&state, 3).answered_same_match, 0);
        }
    }

    /// ADR-385: an answer freed through its owners after a late link error was read is a matched
    /// answer for the exchange after the error only when its stored time is shown after the
    /// error. Row 9.
    #[test]
    fn a_freed_answer_is_a_matched_answer_after_a_late_error_only_when_shown_after_it() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let response = vec![0x62, 0xF1, 0x86, 0x41];
        let answered = judged(forwarding(evidence(&READ_F186, End::Positive(response))));
        for (second, since) in [
            (timed(answered.clone(), Some(6_000), Some(6_200)), true),
            // No time on the error's clock: the answer's order against the error is not shown.
            (answered.clone(), false),
        ] {
            let mut state = LinkState::default();
            let mut owed = silent.clone();
            owed.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let (_, kept) = log_kept(&mut state, 1, timed(owed, Some(1_000), None));
            state.keep(1, kept);
            // Exchange 2's answer could belong to exchange 1: no matched answer yet.
            let (second, _) = log_kept(&mut state, 2, second);
            assert!(!second.matched_answer());
            log_kept(&mut state, 3, timed(silent.clone(), Some(7_000), None));
            let point = read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
            assert_eq!(point, ErrorPoint::After(1));
            // Exchange 1's response, no matched answer itself, frees exchange 2's answer.
            let kept = state.kept(1).unwrap().clone();
            let owner = SettledAnswer {
                seq: 1,
                key: MatchKey::of(&silent),
                newly_answered: false,
                newly_matched: false,
                final_response: true,
                clears: kept.clears,
                since_error: true,
                settled_ids: Vec::new(),
            };
            state.reclassify_after(&owner);
            assert_eq!(
                window_history(&state, 3).answered_same_match,
                u32::from(since)
            );
        }
    }

    /// ADR-385: several late link errors merge: an answer counts since them only when it is later
    /// than the latest on a clock and epoch, and an error that is not placed ends the count of
    /// every exchange known when it was read. An answer with no time, logged after the first
    /// error and before the second, counts after the first only. Row 10.
    #[test]
    fn two_late_errors_merge_and_never_count_an_answer_back() {
        // T after both errors, the earlier one read second: it counts.
        let (mut state, silent) = late_error_scenario();
        read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 3_000));
        read_late_error(&mut state, 5, placed_at(Clock::Device, 0, 2_000));
        assert!(offer_answer(
            &mut state,
            2,
            &silent,
            false,
            device_time(0, 4_000)
        ));
        assert_eq!(window_history(&state, 3).answered_same_match, 1);
        // Between the two errors: not after the later one.
        let (mut state, silent) = late_error_scenario();
        read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 3_000));
        read_late_error(&mut state, 5, placed_at(Clock::Device, 0, 2_000));
        assert!(!offer_answer(
            &mut state,
            2,
            &silent,
            false,
            device_time(0, 2_500)
        ));
        // The second error is on a clock no exchange has: it is not placed, and the answer, which
        // was known when it was read, no longer counts though it is after the first.
        let (mut state, silent) = late_error_scenario();
        read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 3_000));
        read_late_error(&mut state, 5, placed_at(Clock::Worker, 0, 1_000));
        assert!(!offer_answer(
            &mut state,
            2,
            &silent,
            false,
            device_time(0, 4_000)
        ));
        assert_eq!(window_history(&state, 3).answered_same_match, 0);
        // An exchange with no time logged after the first error counts then; the second error,
        // read when it is known, ends its count.
        let (mut state, _) = late_error_scenario();
        read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 5_000));
        let mut other = judged(forwarding(evidence(
            &[0x22, 0xF1, 0x87],
            End::Positive(vec![0x62, 0xF1, 0x87, 0x41]),
        )));
        other.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept(&mut state, 6, other);
        assert!(state.since_late_error(6, AnswerTime::Unknown));
        assert_eq!(state.answered_since_error, 1);
        let point = read_late_error(&mut state, 7, placed_at(Clock::Device, 0, 5_500));
        assert_eq!(point, ErrorPoint::After(1));
        assert!(!state.since_late_error(6, AnswerTime::Unknown));
        assert_eq!(state.answered_since_error, 0);
    }

    /// ADR-385: an in-time link error and a late one: the epochs of the clears decide first,
    /// then the time of the answer. An answer that an in-time error followed does not count
    /// since the late error whatever its time; one with no time on the late error's clock, logged
    /// before the error was read, does not either. Row 11, and a final response in time with no
    /// comparable time.
    #[test]
    fn an_in_time_error_ends_an_answer_before_the_late_errors_time_is_tested() {
        let response = vec![0x62, 0xF1, 0x86, 0x41];
        let answered = || {
            judged(forwarding(evidence(
                &READ_F186,
                End::Positive(response.clone()),
            )))
        };
        let worker_end = |mut evidence: Evidence| {
            let time =
                VciTime::new(2_200, Some(Clock::Worker)).at(Some(super::super::clocks::Placed {
                    epoch: 0,
                    unwrapped: 2_200,
                }));
            evidence.times.vci = Some(VciTimes {
                rx_end: Some(time),
                ..VciTimes::default()
            });
            evidence
        };
        let mut after_error = timed(answered(), Some(2_000), Some(2_200));
        after_error.link_errors.push(LinkErrorKind::Receive);
        after_error.link_errors_before_answer = false;
        // (exchange 2, answered since the late error counted in exchange 3's history)
        let cases = [
            (
                "shown after the error",
                timed(answered(), Some(2_000), Some(2_200)),
                1,
            ),
            ("an in-time error followed it", after_error, 0),
            ("no time", answered(), 0),
            ("another clock", worker_end(answered()), 0),
        ];
        for (name, second, counted) in cases {
            let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
            silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
            let mut state = LinkState::default();
            log_kept(&mut state, 1, timed(answered(), Some(1_000), Some(1_200)));
            log_kept(&mut state, 2, second);
            log_kept(&mut state, 3, timed(silent, Some(6_000), None));
            let point = read_late_error(&mut state, 4, placed_at(Clock::Device, 0, 1_500));
            assert_eq!(point, ErrorPoint::After(1), "{name}");
            assert_eq!(
                window_history(&state, 3).answered_since_error,
                counted,
                "{name}"
            );
        }
    }

    /// ADR-384: an error is shown to fall within an exchange that has only end-side times only
    /// when every exchange before it is still known to the link state. With one gone from the
    /// window the error is placed as before: after an earlier exchange shown to start before it,
    /// else before the window. An end-side exchange kept beyond the window holds none.
    #[test]
    fn an_end_side_exchange_holds_an_error_only_while_the_exchanges_before_it_are_known() {
        let answered = || {
            forwarding(evidence(
                &READ_F186,
                End::Positive(vec![0x62, 0xF1, 0x86, 0x41]),
            ))
        };
        let at = |unwrapped| placed_at(Clock::Device, 0, unwrapped);
        // The window front has only an end, and the exchange before it has left the window.
        let mut state = LinkState::default();
        log_kept(&mut state, 1, timed(answered(), Some(1_000), Some(1_200)));
        log_kept(&mut state, 2, timed(answered(), None, Some(5_000)));
        for seq in 3..=u64::try_from(WINDOW).unwrap() + 1 {
            let us = 10_000 * i64::try_from(seq).unwrap();
            log_kept(&mut state, seq, timed(answered(), Some(us), Some(us + 200)));
        }
        assert_eq!(state.left_window, Some(1));
        assert_eq!(state.window.front().map(|logged| logged.seq), Some(2));
        assert_eq!(
            state.place_error(at(4_000)),
            ErrorPoint::BeforeWindow { within: None }
        );
        // After its latest time it is after it, as ever; at that time too, since its answer is
        // not shown before the error then either.
        assert_eq!(state.place_error(at(5_100)), ErrorPoint::After(2));
        assert_eq!(state.place_error(at(5_000)), ErrorPoint::After(2));
        // Owed and kept beyond the window, with only an end: no exchange holds the error.
        let mut owed = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        owed.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, timed(owed, None, Some(2_000)));
        state.keep(1, kept);
        for seq in 2..=u64::try_from(WINDOW).unwrap() + 1 {
            let us = 10_000 * i64::try_from(seq).unwrap();
            log_kept(&mut state, seq, timed(answered(), Some(us), Some(us + 200)));
        }
        assert_eq!(
            state.place_error(at(1_500)),
            ErrorPoint::BeforeWindow { within: None }
        );
    }

    /// ADR-384, ADR-385: an answer counted as an answer alone stays out of the matched answers
    /// when its exchange's owners are freed later: here an earlier owner's final response, also
    /// shown before the error, leaves the answer to exchange 2 alone. Its stored time is judged
    /// again and gives the same result.
    #[test]
    fn an_answer_judged_as_answered_only_freed_later_is_still_no_matched_answer() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let response: &[u8] = &[0x62, 0xF1, 0x86, 0x41];
        let mut state = LinkState::default();
        let (_, first) = log_kept(&mut state, 1, timed(silent.clone(), Some(1_000), None));
        state.keep(1, first);
        let (_, second) = log_kept(&mut state, 2, silent.clone());
        state.keep(2, second);
        let (later, _) = log_kept(&mut state, 3, timed(silent.clone(), Some(6_000), None));
        let error = placed_at(Clock::Device, 0, 5_000);
        assert_eq!(state.place_error(error), ErrorPoint::After(1));
        state.error_after(Some(1), 4, LinkErrorKind::Receive, Some(error));
        let settle = |state: &mut LinkState, seq: u64, at: i64| {
            let kept = state.kept(seq).unwrap().clone();
            state.settle_owed(seq);
            let answered = kept.answered(response, &LateAnswer::default());
            let history = state.history_settling(&answered, seq);
            let entry = ExchangeEntry::build(seq, answered.clone(), history);
            state.settled(seq, entry.classification, &entry.reason, answered, true);
            let mut answer = entry.settled_answer(&kept, true);
            let (clock, placed) = placed_at(Clock::Device, 0, at);
            state.judge_answer(&mut answer, AnswerTime::At(clock, placed));
            assert!(!answer.since_error, "{seq}");
            state.reclassify_after(&answer);
        };
        settle(&mut state, 2, 4_000);
        settle(&mut state, 1, 4_500);
        assert_eq!(state.window[2].history.answered_same_match, 0);
        assert_eq!(state.window[2].classification, later.classification);
        assert_ne!(state.window[2].classification, Classification::Vehicle);
    }

    /// ADR-385: a recount after a later error tests the stored time of an answer judged as
    /// answered only, and keeps it out.
    #[test]
    fn a_recount_after_an_error_keeps_an_answer_judged_as_answered_only_out() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, timed(silent.clone(), Some(1_000), None));
        state.keep(1, kept);
        log_kept(&mut state, 2, timed(silent.clone(), Some(5_000), None));
        let first = placed_at(Clock::Device, 0, 3_000);
        state.error_after(Some(1), 3, LinkErrorKind::Receive, Some(first));
        // The first exchange's answer at 2 500 is before the error: answered only.
        let kept = state.kept(1).unwrap().clone();
        let answered = kept.answered(&[0x62, 0xF1, 0x86, 0x41], &LateAnswer::default());
        let history = state.history_settling(&answered, 1);
        let entry = ExchangeEntry::build(1, answered.clone(), history);
        let mut settled = entry.settled_answer(&kept, true);
        let (clock, placed) = placed_at(Clock::Device, 0, 2_500);
        state.judge_answer(&mut settled, AnswerTime::At(clock, placed));
        assert!(!settled.since_error);
        state.settled(1, entry.classification, &entry.reason, answered, true);
        state.reclassify_after(&settled);
        assert_eq!(state.answered_since_error, 0);
        // An error before the window recounts from the window: the answer is still not one.
        state.error_after(
            None,
            4,
            LinkErrorKind::Receive,
            Some(placed_at(Clock::Device, 0, 500)),
        );
        assert_eq!(state.answered_since_error, 0);
        assert_eq!(state.window[1].history.answered_since_error, 0);
    }

    /// ADR-384, ADR-385: the item that answers an owed exchange gives its answer's time: the
    /// final response, else the latest pending the settling entry adds, and a start only when the
    /// exchange has no answer yet; with no new item, the time stored with the exchange. A pending
    /// with no placed time is unknown, and no other item's time stands in for it.
    #[test]
    fn the_answer_time_is_that_of_the_item_that_answers() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, mut kept) = log_kept(&mut state, 1, silent);
        let at = |us| Some(placed_at(Clock::Device, 0, us));
        let time = |us| {
            let (clock, placed) = placed_at(Clock::Device, 0, us);
            AnswerTime::At(clock, placed)
        };
        // Nothing answers.
        assert_eq!(kept.answer_time(None, &[], None), AnswerTime::None);
        // A start alone answers by its own time.
        assert_eq!(kept.answer_time(None, &[], Some(at(30))), time(30));
        assert_eq!(kept.answer_time(None, &[], Some(None)), AnswerTime::Unknown);
        // With an answer stored, a start stands in for nothing: the stored time is the answer's.
        kept.answer_at = time(10);
        assert_eq!(kept.answer_time(None, &[], Some(at(30))), time(10));
        assert_eq!(kept.answer_time(None, &[], None), time(10));
        // The settling entry's latest pending, then its final response, over the stored time.
        assert_eq!(
            kept.answer_time(None, &[at(25), at(26)], Some(at(30))),
            time(26)
        );
        assert_eq!(kept.answer_time(Some(at(40)), &[at(25)], None), time(40));
        // A pending with no placed time is unknown, whatever else the exchange has.
        assert_eq!(
            kept.answer_time(Some(None), &[at(25)], None),
            AnswerTime::Unknown
        );
        assert_eq!(kept.answer_time(None, &[None], None), AnswerTime::Unknown);
        kept.answer_at = AnswerTime::Unknown;
        assert_eq!(
            kept.answer_time(None, &[], Some(at(30))),
            AnswerTime::Unknown
        );
    }

    /// The exchange a late link error falls after answered at 2 200 us, before the error at
    /// 5 000 us: when its answer is freed later, by its owner's final response after the error,
    /// it counts for no exchange after the error, since its stored time is not after the error's
    /// (ADR-385 decision 2, ADR-444).
    #[test]
    fn an_answer_freed_after_an_error_placed_after_its_exchange_counts_before_it() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let response = vec![0x62, 0xF1, 0x86, 0x41];
        let mut answered = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(response.clone()),
        )));
        answered.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, timed(silent.clone(), Some(1_000), None));
        state.keep(1, kept);
        let (answer, _) = log_kept(&mut state, 2, timed(answered, Some(2_000), Some(2_200)));
        assert!(!answer.matched_answer());
        log_kept(&mut state, 3, timed(silent.clone(), Some(6_000), None));
        let at = placed_at(Clock::Device, 0, 5_000);
        assert_eq!(state.place_error(at), ErrorPoint::After(2));
        state.error_after(Some(2), 4, LinkErrorKind::Receive, Some(at));
        let kept = state.kept(1).unwrap().clone();
        state.settle_owed(1);
        let late = kept.answered(&response, &LateAnswer::default());
        let history = state.history_settling(&late, 1);
        let entry = ExchangeEntry::build(1, late.clone(), history);
        let mut settled = entry.settled_answer(&kept, true);
        let (clock, placed) = placed_at(Clock::Device, 0, 7_000);
        state.judge_answer(&mut settled, AnswerTime::At(clock, placed));
        state.settled(1, entry.classification, &entry.reason, late, true);
        state.reclassify_after(&settled);
        assert_eq!(window_history(&state, 3).answered_same_match, 0);
        let third = state.window.iter().find(|logged| logged.seq == 3).unwrap();
        assert_ne!(third.classification, Classification::Vehicle);
    }

    /// ADR-444: the answer of an owned exchange, freed after a late error placed after the
    /// exchange, counts for the later exchange when its stored time is shown after the error, and
    /// for none when it is not, whether the exchange is still in the window or has left it.
    #[test]
    fn an_answer_freed_after_an_error_placed_after_its_exchange_counts_by_its_stored_time() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let response = vec![0x62, 0xF1, 0x86, 0x41];
        for (answer_us, counted) in [(5_500, 1), (2_200, 0)] {
            for extra in [0, WINDOW as u64] {
                let mut owned = silent.clone();
                pending_only(&mut owned);
                let mut state = LinkState::default();
                let (_, owner) = log_kept(&mut state, 1, timed(silent.clone(), Some(1_000), None));
                state.keep(1, owner);
                let (_, kept) = log_kept_owned(&mut state, 2, timed(owned, Some(2_000), None));
                state.keep(2, kept);
                log_kept(&mut state, 3, timed(silent.clone(), Some(6_000), None));
                let at = device_time(0, answer_us);
                state.kept.get_mut(&2).unwrap().answer_at = at;
                state
                    .window
                    .iter_mut()
                    .find(|l| l.seq == 2)
                    .unwrap()
                    .answer_at = at;
                let error = placed_at(Clock::Device, 0, 5_000);
                assert_eq!(state.place_error(error), ErrorPoint::After(2));
                state.error_after(Some(2), 4, LinkErrorKind::Receive, Some(error));
                for seq in 4..4 + extra {
                    log_unanswered(&mut state, seq);
                }
                assert_eq!(logged_beyond(&state, 2), extra > 0);
                let kept = state.kept(1).unwrap().clone();
                settle_late(&mut state, 1, &kept, &response);
                let last = state.window.back().unwrap().seq;
                assert_eq!(
                    (same_match(&state, last), link_matched(&state)),
                    (counted, counted),
                    "{answer_us} {extra}"
                );
            }
        }
    }

    /// A settling entry that adds no answer stores no answer time (ADR-384, ADR-385).
    #[test]
    fn a_settlement_with_no_answer_stores_no_answer_time() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        state.keep(1, kept);
        log_kept(&mut state, 2, silent);
        let at = placed_at(Clock::Device, 0, 5_000);
        state.error_after(Some(1), 3, LinkErrorKind::Receive, Some(at));
        let kept = state.kept(1).unwrap().clone();
        let history = state.history_settling(&kept.evidence, 1);
        let entry = ExchangeEntry::build(1, kept.evidence.clone(), history);
        let mut settled = entry.settled_answer(&kept, true);
        let time = kept.answer_time(None, &[], None);
        assert_eq!(time, AnswerTime::None);
        state.judge_answer(&mut settled, time);
        state.reclassify_after(&settled);
        assert_eq!(state.kept(1).unwrap().answer_at, AnswerTime::None);
        assert_eq!(state.window[0].answer_at, AnswerTime::None);
    }

    /// ADR-384, ADR-385: once an owed exchange's answer was judged as answered only, a later
    /// settling path that offers it again, with no answering item of its own, judges the stored
    /// time and gives the same result; only an answering item the path adds, shown after the
    /// error, counts.

    #[test]
    fn an_answer_judged_as_answered_only_is_judged_the_same_by_a_later_settling_path() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let at = |us| Some(placed_at(Clock::Device, 0, us));
        // (path, the final response, the settling entry's pendings, a start, whether it counts)
        type Path = (
            &'static str,
            Option<Option<(Clock, super::super::clocks::Placed)>>,
            Vec<Option<(Clock, super::super::clocks::Placed)>>,
            Option<Option<(Clock, super::super::clocks::Placed)>>,
            bool,
        );
        let table: Vec<Path> = vec![
            (
                "a transmit indication re-offers the stored time",
                None,
                vec![],
                None,
                false,
            ),
            (
                "a start re-offers the stored time",
                None,
                vec![],
                Some(at(6_000)),
                false,
            ),
            ("job end with no new item", None, vec![], None, false),
            (
                "a new pending before the error",
                None,
                vec![at(4_500)],
                None,
                false,
            ),
            (
                "a new item with no placed time",
                None,
                vec![None],
                None,
                false,
            ),
            (
                "a new response after the error",
                Some(at(6_000)),
                vec![],
                None,
                true,
            ),
        ];
        for (path, response, pendings, start, counts) in table {
            let mut state = LinkState::default();
            let (_, kept) = log_kept(&mut state, 1, silent.clone());
            state.keep(1, kept);
            let (later, _) = log_kept(&mut state, 2, silent.clone());
            assert_eq!(later.classification, Classification::Undetermined);
            state.error_after(
                Some(1),
                3,
                LinkErrorKind::Receive,
                Some(placed_at(Clock::Device, 0, 5_000)),
            );
            let offer = |state: &mut LinkState, new: (Option<_>, &[_], Option<_>)| {
                let kept = state.kept(1).unwrap().clone();
                let mut answer = SettledAnswer {
                    seq: 1,
                    key: MatchKey::of(&silent),
                    newly_answered: true,
                    newly_matched: !kept.matched,
                    final_response: true,
                    clears: kept.clears,
                    since_error: true,
                    settled_ids: Vec::new(),
                };
                let time = kept.answer_time(new.0, new.1, new.2);
                state.judge_answer(&mut answer, time);
                let since = answer.since_error;
                let (changed, _) = state.reclassify_after(&answer);
                (since, changed.iter().any(|later| later.seq == 2))
            };
            // The first decision: a pending shown before the error is answered only.
            assert_eq!(
                offer(&mut state, (None, &[at(4_000)], None)),
                (false, false)
            );
            assert_eq!(state.kept(1).unwrap().answer_at, {
                let (clock, placed) = placed_at(Clock::Device, 0, 4_000);
                AnswerTime::At(clock, placed)
            });
            assert_eq!(state.answered_since_error, 0);
            // The path under test.
            let (since, reclassified) = offer(&mut state, (response, &pendings, start));
            assert_eq!((since, reclassified), (counts, counts), "{path}");
            assert_eq!(state.answered_since_error, u32::from(counts), "{path}");
        }
    }

    /// Design 8.11 "Settlement": a final response a settling entry adds to an earlier exchange
    /// ends it as an owed request for the later exchanges in the window, and a matched answer
    /// it adds counts for those with the same match unless a link error came between them; each
    /// is reclassified. The window ends at an accepted session change, and an exchange that left
    /// it is named, not reclassified.
    #[test]
    fn a_settled_answer_reclassifies_the_later_exchanges_in_the_window() {
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let answer: &[u8] = &[0x62, 0xF1, 0x86, 0x41];
        let mut answered = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(answer.to_vec()),
        )));
        answered.transmit_evidence = Some(TransmitEvidence::TxIndication);
        // A later answer the earlier owed request could own is `undetermined`; once that
        // request has its own response, the later one is its own rows' again.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        let (later, _) = log_kept(&mut state, 2, answered.clone());
        assert_eq!(later.classification, Classification::Undetermined);
        let (changed, past) = state.reclassify_after(&final_answer(&state, 1, &kept, answer));
        assert_eq!(past, None);
        let [reclassified] = &changed[..] else {
            panic!("{changed:?}");
        };
        assert_eq!(reclassified.seq, 2);
        assert_eq!(reclassified.classification, Classification::None);
        // The same answer again changes nothing: the window already has it.
        assert!(
            state
                .reclassify_after(&final_answer(&state, 1, &kept, answer))
                .0
                .is_empty()
        );

        // A matched answer the earlier exchange gets counts for a later silent exchange with
        // the same match: it is now `vehicle`.
        let matched = |kept: &Kept| SettledAnswer {
            seq: 1,
            key: MatchKey::of(&silent),
            newly_answered: true,
            newly_matched: true,
            final_response: true,
            clears: kept.clears,
            since_error: true,
            settled_ids: Vec::new(),
        };
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        let (later, _) = log_kept(&mut state, 2, silent.clone());
        assert_eq!(later.classification, Classification::Undetermined);
        let (changed, _) = state.reclassify_after(&matched(&kept));
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0].classification, Classification::Vehicle);

        // A link error between them ends the matched answers: the later exchange keeps its row.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        state.channel_link_error(2, LinkErrorKind::Receive);
        log_kept(&mut state, 3, silent.clone());
        let (changed, _) = state.reclassify_after(&matched(&kept));
        assert!(changed.is_empty(), "{changed:?}");

        // An accepted session change empties the window; one more than the window's size
        // pushes the oldest out. Either way the exchanges that left it are named.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        log_kept(&mut state, 2, silent.clone());
        log_kept(
            &mut state,
            3,
            evidence(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
        );
        assert!(state.window.iter().all(|logged| logged.seq == 3));
        assert_eq!(
            state.reclassify_after(&final_answer(&state, 1, &kept, answer)),
            (Vec::new(), Some(2))
        );
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, silent.clone());
        for seq in 2..=u64::try_from(WINDOW).unwrap() + 2 {
            log_kept(&mut state, seq, silent.clone());
        }
        assert_eq!(state.window.len(), WINDOW);
        let (changed, past) = state.reclassify_after(&matched(&kept));
        assert_eq!(past, Some(2));
        assert_eq!(changed.len(), WINDOW, "{changed:?}");
        assert_eq!(changed[0].seq, 3);
    }

    fn read(did: [u8; 2], end: End) -> Evidence {
        evidence(&[0x22, did[0], did[1]], end)
    }

    #[test]
    fn nrc_kind_table_lists_every_code_as_design_8_11_does() {
        let expect = |codes: &[u8], kind: NrcKind| {
            for &code in codes {
                assert_eq!(nrc_kind(code), kind, "{code:#04x}");
            }
        };
        expect(&[0x35], NrcKind::SecurityKey);
        expect(&[0x11, 0x12, 0x13], NrcKind::DefinitionMismatch);
        expect(&[0x14, 0x31], NrcKind::Ambiguous);
        expect(&[0x7E, 0x7F], NrcKind::Session);
        expect(&[0x33, 0x34], NrcKind::Access);
        expect(&[0x24, 0x73], NrcKind::Sequence);
        expect(&[0x36, 0x37], NrcKind::Attempts);
        expect(&[0x38, 0x39, 0x3A], NrcKind::SecuredData);
        expect(&(0x50..=0x5D).collect::<Vec<u8>>(), NrcKind::Authentication);
        let mut ecu_state = vec![0x21, 0x22, 0x25, 0x26, 0x70, 0x71, 0x72];
        ecu_state.extend(0x81..=0x8D);
        ecu_state.extend(0x8F..=0x94);
        ecu_state.extend(0x95..=0xEF);
        ecu_state.extend(0xF0..=0xFE);
        expect(&ecu_state, NrcKind::EcuState);
    }

    #[test]
    fn nrc_kind_range_edges_and_unlisted_codes() {
        for (code, kind) in [
            (0x8D, NrcKind::EcuState),
            (0x8E, NrcKind::UnknownCode),
            (0x8F, NrcKind::EcuState),
            (0x94, NrcKind::EcuState),
            (0x95, NrcKind::EcuState),
            (0xEF, NrcKind::EcuState),
            (0xF0, NrcKind::EcuState),
            (0xFE, NrcKind::EcuState),
            (0xFF, NrcKind::UnknownCode),
            (0x80, NrcKind::UnknownCode),
            (0x4F, NrcKind::UnknownCode),
            (0x5D, NrcKind::Authentication),
            (0x5E, NrcKind::UnknownCode),
            (0x6F, NrcKind::UnknownCode),
            (0x3A, NrcKind::SecuredData),
            (0x3B, NrcKind::UnknownCode),
            (0x00, NrcKind::UnknownCode),
            (0x10, NrcKind::UnknownCode),
            (0x78, NrcKind::UnknownCode),
        ] {
            assert_eq!(nrc_kind(code), kind, "{code:#04x}");
        }
        // Every code not listed is `unknown_code`.
        let listed: usize = (0..=0xFFu8)
            .filter(|&code| nrc_kind(code) != NrcKind::UnknownCode)
            .count();
        // 35; 11-13; 14, 31; 7E, 7F; 33, 34; 24, 73; 36, 37; 38-3A; 50-5D; 21, 22, 25, 26;
        // 70-72; 81-8D; 8F-FE.
        assert_eq!(
            listed,
            1 + 3 + 2 + 2 + 2 + 2 + 2 + 3 + 14 + 4 + 3 + 13 + 112
        );
    }

    #[test]
    fn nrc_names_are_given_for_listed_codes_only() {
        assert_eq!(nrc_name(0x22), Some("conditionsNotCorrect"));
        assert_eq!(nrc_name(0x35), Some("invalidKey"));
        assert_eq!(nrc_name(0x10), Some("generalReject"));
        assert_eq!(
            nrc_name(0x78),
            Some("requestCorrectlyReceivedResponsePending")
        );
        assert_eq!(nrc_name(0x7F), Some("serviceNotSupportedInActiveSession"));
        assert_eq!(
            nrc_name(0x95),
            Some("reservedForSpecificConditionsNotCorrect")
        );
        assert_eq!(
            nrc_name(0xFE),
            Some("vehicleManufacturerSpecificConditionsNotCorrect")
        );
        for code in [0x00, 0x23, 0x50, 0x5D, 0x8E, 0xFF] {
            assert_eq!(nrc_name(code), None, "{code:#04x}");
        }
        assert_eq!(service_name(0x31), Some("RoutineControl"));
        assert_eq!(service_name(0x11), Some("ECUReset"));
        assert_eq!(service_name(0x99), None);
    }

    #[test]
    fn an_ecu_negative_response_on_a_forwarding_link_is_vehicle_for_ecu_state_codes() {
        let evidence = judged(forwarding(evidence(
            &[0x31, 0x01, 0xFF, 0x00],
            End::Negative(vec![0x7F, 0x31, 0x22]),
        )));
        let verdict = verdict(&evidence);
        assert_eq!(verdict.classification, Classification::Vehicle);
        assert!(verdict.reason.contains("conditionsNotCorrect (0x22)"));
        assert!(verdict.text.contains("conditionsNotCorrect (0x22)"));
        let entry = ExchangeEntry::build(1, evidence, LinkHistory::empty());
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["outcome"]["result"], "negative");
        assert_eq!(json["outcome"]["nrc"], "0x22");
        assert_eq!(json["outcome"]["nrc_name"], "conditionsNotCorrect");
        assert_eq!(json["outcome"]["nrc_kind"], "ecu_state");
        assert_eq!(json["request"]["service"], "RoutineControl");
    }

    /// ADR-359: the entry lists the rejected results, redacted as unanswering responses, and
    /// the lost events; neither changes its classification.
    #[test]
    fn an_entry_lists_its_rejected_results_and_lost_events() {
        let plain = forwarding(evidence(&READ_F186, End::NoResponse));
        let mut with = plain.clone();
        with.rejected = vec![
            Rejected {
                rx_id: 0x7E8,
                identifier: Some(0x7E8),
                rx_flag: vec![0; 4],
                bytes: vec![0x62, 0xF1],
                vci_us: Some(5),
                clock: Some(Clock::Device),
                epoch: None,
                agent_us: 9,
                reason: RejectedReason::WrongEcho,
                late_response: None,
            },
            Rejected {
                rx_id: 0x7E8,
                identifier: Some(0x7E8),
                rx_flag: vec![0; 4],
                bytes: vec![0x67, 0x02, 0xAA, 0xBB],
                vci_us: None,
                clock: None,
                epoch: None,
                agent_us: 10,
                reason: RejectedReason::WrongEcho,
                late_response: None,
            },
        ];
        with.events_lost = Some(EventsLost {
            agent_us: 11,
            notifications: 1,
        });
        assert_eq!(
            verdict(&with).classification,
            verdict(&plain).classification
        );
        let json =
            serde_json::to_value(ExchangeEntry::build(1, with, LinkHistory::empty())).unwrap();
        assert_eq!(
            json["rejected"],
            serde_json::json!([
                {"rx_id": 0x7E8, "response": {"clear": "62f1"}, "vci_us": 5, "clock": "device",
                 "agent_us": 9, "reason": "wrong_echo"},
                {"rx_id": 0x7E8, "response": {"clear": "6702", "data": {"kind": "secret", "len": 2}},
                 "agent_us": 10, "reason": "wrong_echo"},
            ])
        );
        assert_eq!(
            json["events_lost"],
            serde_json::json!({"agent_us": 11, "notifications": 1})
        );
        let none =
            serde_json::to_value(ExchangeEntry::build(1, plain, LinkHistory::empty())).unwrap();
        assert!(
            none.get("rejected").is_none() && none.get("events_lost").is_none(),
            "{none}"
        );
    }

    /// ADR-359 item 3: an end the worker took from a rejected result is `positive_unusable`,
    /// redacted as a rejected result is, and it counts for no answer in the link's history: the
    /// request stays owed.
    #[test]
    fn an_end_taken_from_a_rejected_result_is_no_answer_of_the_link() {
        let mut state = LinkState::default();
        let mut cut = forwarding(evidence(
            &[0x27, 0x01],
            End::PositiveUnusable(vec![0x67, 0x02, 0xAA, 0xBB]),
        ));
        cut.end_rejected = true;
        let entry = log(&mut state, 1, cut);
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["outcome"]["result"], "positive_unusable");
        assert_eq!(
            json["outcome"]["response"],
            serde_json::json!({"clear": "6702", "data": {"kind": "secret", "len": 2}})
        );
        assert_eq!(json["link_history"]["this_exchange"], false);
        let next = state.history_for(&forwarding(evidence(
            &[0x27, 0x01],
            End::Positive(vec![0x67, 0x01, 0x11]),
        )));
        assert_eq!(next.answered, 0);
        assert_eq!(next.owed_matching, 1);
    }

    /// ADR-359: a pending before a rejected end is still an answer of the ECU.
    #[test]
    fn a_pending_before_a_rejected_end_is_still_an_answer() {
        let mut cut = forwarding(evidence(
            &READ_F186,
            End::PositiveUnusable(vec![0x62, 0xF1]),
        ));
        cut.end_rejected = true;
        cut.pending = vec![Pending {
            vci_us: Some(1),
            clock: Some(Clock::Device),
            epoch: None,
            agent_us: 2,
            since_previous_ms: None,
        }];
        assert_eq!(cut.answer_head(), Some(vec![0x7F, 0x22, 0x78]));
        assert!(cut.bound_answer());
    }

    #[test]
    fn an_ecu_negative_response_is_undetermined_for_every_other_kind() {
        for code in [
            0x10, 0x11, 0x14, 0x24, 0x31, 0x33, 0x35, 0x36, 0x38, 0x50, 0x73, 0x7F, 0x8E, 0xFF,
        ] {
            let evidence = forwarding(evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, code])));
            let verdict = verdict(&evidence);
            assert_eq!(
                verdict.classification,
                Classification::Undetermined,
                "{code:#04x}"
            );
        }
        for code in [0x21, 0x25, 0x26, 0x70, 0x81, 0x94, 0xF0] {
            let evidence = judged(forwarding(evidence(
                &READ_F186,
                End::Negative(vec![0x7F, 0x22, code]),
            )));
            assert_eq!(
                verdict(&evidence).classification,
                Classification::Vehicle,
                "{code:#04x}"
            );
        }
    }

    #[test]
    fn a_negative_response_on_an_absorbing_link_not_judged_is_undetermined_for_h4() {
        // The possible-pending row stands before the negative rows in the table.
        let evidence = evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, 0x22]));
        let verdict = verdict(&evidence);
        assert_eq!(verdict.classification, Classification::Undetermined);
        assert!(verdict.reason.contains("H4"));
    }

    #[test]
    fn a_positive_response_on_an_absorbing_link_not_judged_is_undetermined() {
        let evidence = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86, 1]));
        let verdict = verdict(&evidence);
        assert_eq!(verdict.classification, Classification::Undetermined);
        assert!(verdict.reason.contains("H4"));
        // A judged window within P2 leaves no possible pending.
        let mut judged = evidence.clone();
        judged.timing.window = Window::WithinP2;
        judged.timing.not_judged_reason = None;
        assert_eq!(verdict_of(&judged).0, Classification::None);
    }

    #[test]
    fn a_silence_is_vehicle_only_after_a_timeout_judged_at_the_limit_on_a_forwarding_link() {
        // ADR-376: a pending then silence, and a silence after a matched answer, are `vehicle`
        // only when the receive timeout is judged at the limit on one clock and the link
        // forwards pendings; the VCI's timer, or the worker's, is not presumed right, and a
        // timeout at P2 does not show that a VCI that absorbs pendings absorbed none.
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            forwarding(read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1]))),
        );
        let mut silent = forwarding(read([0xF1, 0x86], End::NoResponse));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        silent.vci_ended = true;
        let mut pending_then_silence = forwarding(evidence(&READ_F186, End::NoResponse));
        pending_then_silence.pending = vec![pending()];
        pending_then_silence.vci_ended = true;
        for (silent, history) in [
            (&silent, state.history_for(&silent)),
            (
                &pending_then_silence,
                LinkState::default().history_for(&pending_then_silence),
            ),
        ] {
            let of = |evidence: &Evidence| classify(evidence, &history);
            let unjudged = of(silent);
            assert_eq!(
                unjudged.classification,
                Classification::Undetermined,
                "{}",
                unjudged.reason
            );
            assert!(
                unjudged.reason.contains("not presumed right"),
                "{}",
                unjudged.reason
            );
            assert_eq!(
                of(&judged(silent.clone())).classification,
                Classification::Vehicle
            );
            let mut absorbing = judged(silent.clone());
            absorbing.pending_visibility = PendingVisibility::AbsorbedByVci;
            let absorbed = of(&absorbing);
            assert_eq!(
                absorbed.classification,
                Classification::Undetermined,
                "{}",
                absorbed.reason
            );
            assert!(absorbed.reason.contains("(H4)"), "{}", absorbed.reason);
            assert!(
                !absorbed.reason.contains("not presumed right"),
                "{}",
                absorbed.reason
            );
            // Neither judged nor forwarding: the possible-pending row names both readings.
            let mut both = silent.clone();
            both.pending_visibility = PendingVisibility::AbsorbedByVci;
            let both = of(&both);
            assert_eq!(both.classification, Classification::Undetermined);
            assert!(both.reason.contains("not judged"), "{}", both.reason);
            assert!(both.reason.contains("(H4)"), "{}", both.reason);
        }
        // A suppressed request's silence, timed out at P2 on a forwarding link, when only its
        // first frame went out: the reason says so, not that no timeout was judged.
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            forwarding(evidence(&[0x3E, 0x00], End::Positive(vec![0x7E, 0x00]))),
        );
        let mut partial = judged(forwarding(evidence(&[0x3E, 0x80], End::NoResponse)));
        partial.suppressed = true;
        partial.flow_control = true;
        let entry = log(&mut state, 2, partial);
        assert_eq!(entry.sent, Sent::Partial, "{}", entry.reason);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(
            entry.reason.contains("only the first frame"),
            "{}",
            entry.reason
        );
        assert!(
            !entry.reason.contains("no receive timeout"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn a_final_response_whose_time_is_not_judged_is_undetermined() {
        // ADR-376: on time it would be `none` or `vehicle`; late, with the VCI past its own
        // limit, `system`; nothing presumes the VCI's timer right.
        for end in [
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
            End::Negative(vec![0x7F, 0x22, 0x22]),
        ] {
            let evidence = forwarding(evidence(&READ_F186, end));
            assert_eq!(evidence.timing.window, Window::NotJudged);
            let verdict = verdict(&evidence);
            assert_eq!(verdict.classification, Classification::Undetermined);
            assert!(
                verdict.reason.contains("Its time is not judged"),
                "{}",
                verdict.reason
            );
            assert!(verdict.reason.contains("system"), "{}", verdict.reason);
            assert!(
                !verdict.reason.contains("presumption"),
                "{}",
                verdict.reason
            );
            // Judged within P2, its own row decides.
            let judged = verdict_of(&judged(evidence)).0;
            assert!(judged < Classification::Undetermined, "{judged:?}");
        }
    }

    #[test]
    fn a_missing_response_with_sent_unknown_names_h1() {
        let mut evidence = forwarding(evidence(&READ_F186, End::NoResponse));
        let verdict = verdict(&evidence);
        assert_eq!(verdict.classification, Classification::Undetermined);
        assert!(verdict.reason.contains("H1"));
        let entry = ExchangeEntry::build(2, evidence.clone(), LinkHistory::empty());
        assert_eq!(entry.sent, Sent::Unknown);
        assert_eq!(entry.sent_evidence, SentEvidence::None);
        assert_eq!(entry.timing.window, Window::NotJudged);
        assert_eq!(
            entry.timing.not_judged_reason,
            Some(NotJudgedReason::NoTransmitTime)
        );
        assert_eq!(entry.timing.measured_by, MeasuredBy::Agent);
        // On an absorbing link the possible-pending row comes first and names H4.
        evidence.pending_visibility = PendingVisibility::AbsorbedByVci;
        let verdict = verdict_of(&evidence);
        assert_eq!(verdict.0, Classification::Undetermined);
        assert!(verdict.1.contains("H4"));
        // The reason lists every reading the evidence leaves open: the send is not known (H1)
        // and no matched answer counts (H2), as well as the two readings of H4.
        assert!(verdict.1.contains("H1"), "{}", verdict.1);
        assert!(verdict.1.contains("H2"), "{}", verdict.1);
        assert!(!verdict.1.contains("H3"), "{}", verdict.1);
        // A confirmed send closes H1 and a link error opens H3.
        let mut confirmed = evidence.clone();
        confirmed.transmit_evidence = Some(TransmitEvidence::TxIndication);
        confirmed.channel_link_errors = vec![LinkErrorKind::Receive];
        let reason = verdict_of(&confirmed).1;
        assert!(!reason.contains("H1") && reason.contains("H3"), "{reason}");
        // With a timeout judged at the limit, H4 is out and H1 stands.
        evidence.timing.window = Window::TimeoutAtLimit;
        let verdict = verdict_of(&evidence);
        assert!(verdict.1.contains("H1"));
    }

    /// Design 8.11 `pending`, "On one clock": an interval from the transmit or the previous
    /// pending, between two unwrapped VCI times on one clock and epoch, forward only (ADR-361).
    #[test]
    fn a_pending_interval_is_taken_on_one_clock_and_epoch_and_forward_only() {
        let at = |vci_us, clock| Pending {
            vci_us: Some(vci_us),
            clock: Some(clock),
            epoch: None,
            agent_us: 0,
            since_previous_ms: None,
        };
        let on = |epoch, unwrapped| Some(Placed { epoch, unwrapped });
        let mut pending = vec![
            at(11_500, Clock::Device),
            at(41_500, Clock::Device),
            at(50_000, Clock::Worker),
            at(60_000, Clock::Worker),
            // Raw 10 after a wrap of the 32-bit time, then 10 again in a new epoch.
            at(10, Clock::Worker),
            at(10, Clock::Worker),
            Pending {
                vci_us: None,
                ..at(0, Clock::Worker)
            },
            at(70_000, Clock::Worker),
            Pending {
                clock: None,
                ..at(80_000, Clock::Worker)
            },
            at(90_000, Clock::Worker),
            at(95_000, Clock::Worker),
            at(94_000, Clock::Worker),
        ];
        let wrapped = (1_i64 << 32) + 10;
        let placed = [
            on(0, 11_500),
            on(0, 41_500),
            on(0, 50_000),
            on(0, (1_i64 << 32) - 5_000),
            on(0, wrapped),
            on(1, 10),
            None,
            on(1, 70_000),
            None,
            on(1, 90_000),
            None,
            on(1, 94_000),
        ];
        let transmit = Some((
            Clock::Device,
            Placed {
                epoch: 0,
                unwrapped: 10_000,
            },
        ));
        pending_intervals(&mut pending, &placed, transmit);
        let intervals: Vec<_> = pending.iter().map(|p| p.since_previous_ms).collect();
        assert_eq!(
            intervals,
            [
                Some(1.5),  // from the transmit
                Some(30.0), // from the previous pending
                None,       // another clock
                Some(4_294_912.296),
                Some(5.01), // across the wrap
                None,       // another epoch
                None,       // no time
                None,       // after one with no time
                None,       // a time but no clock
                None,       // after one with no clock
                None,       // not placed
                None,       // after one not placed
            ]
        );
        // Backward within one epoch gives none.
        let mut back = vec![at(2_000, Clock::Device), at(1_000, Clock::Device)];
        pending_intervals(&mut back, &[on(0, 2_000), on(0, 1_000)], None);
        assert_eq!(back[1].since_previous_ms, None);
        // Without a transmit time the first has no interval.
        let mut first = vec![at(5_000, Clock::Device)];
        pending_intervals(&mut first, &[on(0, 5_000)], None);
        assert_eq!(first[0].since_previous_ms, None);
    }

    #[test]
    fn a_pending_then_silence_is_vehicle_and_confirms_the_send() {
        let mut evidence = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        evidence.pending = vec![pending()];
        evidence.vci_ended = true;
        let entry = ExchangeEntry::build(3, evidence.clone(), {
            LinkState::default().history_for(&evidence)
        });
        assert_eq!(entry.classification, Classification::Vehicle);
        assert_eq!(entry.sent, Sent::Confirmed);
        assert_eq!(entry.sent_evidence, SentEvidence::Answer);
        assert!(entry.link_history.this_exchange);
        // A primitive that ended with no receive timeout shows no limit ran out, so the silence
        // is not the ECU's.
        let mut ended_without_timeout = evidence.clone();
        ended_without_timeout.vci_ended = false;
        let verdict = verdict_of(&ended_without_timeout);
        assert_eq!(verdict.0, Classification::Undetermined);
        assert!(verdict.1.contains("no receive timeout"), "{}", verdict.1);
        // A link error during the exchange makes it undetermined and names both.
        evidence.link_errors = vec![LinkErrorKind::Receive];
        let verdict = verdict_of(&evidence);
        assert_eq!(verdict.0, Classification::Undetermined);
        assert!(verdict.1.contains("H3"));
        assert!(verdict.1.contains("receive error"));
    }

    #[test]
    fn a_link_error_during_the_exchange_takes_the_most_severe() {
        let mut evidence = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86]),
        )));
        evidence.link_errors = vec![LinkErrorKind::Frame];
        assert_eq!(verdict_of(&evidence).0, Classification::Undetermined);
        evidence.link_errors = vec![LinkErrorKind::Frame, LinkErrorKind::VciHardwareFault];
        let (class, reason) = verdict_of(&evidence);
        assert_eq!(class, Classification::System);
        assert!(reason.contains("responded positively"));
        assert!(reason.contains("frame error"));
        assert!(reason.contains("fault of the VCI"));
        // A negative ecu_state is vehicle; a link error of the channel's other link is evidence
        // only.
        let mut evidence = judged(forwarding(evidence_neg(0x22)));
        evidence.channel_link_errors = vec![LinkErrorKind::Receive];
        assert_eq!(verdict_of(&evidence).0, Classification::Vehicle);
    }

    fn evidence_neg(code: u8) -> Evidence {
        evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, code]))
    }

    #[test]
    fn refusals_by_the_agent_are_system_and_one_caused_by_a_response_is_undetermined() {
        for cause in [
            RefusalCause::Policy,
            RefusalCause::BlockTooLong,
            RefusalCause::PreparationError,
            RefusalCause::NoTransferActive,
            RefusalCause::EmptyRequest,
            RefusalCause::UndeclaredService,
            RefusalCause::SystemOwnReasons,
            RefusalCause::LinkHeld,
            RefusalCause::KeepAliveFailed,
            RefusalCause::StreamFailed,
        ] {
            let evidence = evidence(&[0x36, 1], End::NotSent(cause));
            let entry = ExchangeEntry::build(4, evidence, LinkHistory::empty());
            assert_eq!(entry.classification, Classification::System);
            assert_eq!(entry.sent, Sent::NotSent);
            assert_eq!(entry.sent_evidence, SentEvidence::Refused);
        }
        let mut evidence = evidence(&[0x36, 1], End::NotSent(RefusalCause::EarlierResponse));
        evidence.caused_by = Some(9);
        let entry = ExchangeEntry::build(10, evidence, LinkHistory::empty());
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("entry 9"));
        let json = serde_json::to_value(&entry).unwrap();
        // The refused exchange's cause is a name; the exchange that caused it is the entry's own
        // `caused_by`.
        assert_eq!(
            json["outcome"],
            json!({ "result": "not_sent", "cause": "earlier_response" })
        );
        assert_eq!(json["caused_by"], 9);
    }

    /// A refusal because the worker reported the session lost is undetermined (the vehicle may
    /// have dropped the session), not the system's, and names the `keep_alive` entry of the
    /// error in `caused_by` (design 8.11).
    #[test]
    fn a_refusal_for_a_lost_session_is_undetermined_and_names_the_error_entry() {
        let mut evidence = evidence(&[0x36, 1], End::NotSent(RefusalCause::SessionLost));
        evidence.caused_by = Some(7);
        let entry = ExchangeEntry::build(8, evidence, LinkHistory::empty());
        assert_eq!(entry.classification, Classification::Undetermined);
        assert_eq!(entry.sent, Sent::NotSent);
        assert_eq!(entry.sent_evidence, SentEvidence::Refused);
        assert!(entry.reason.contains("entry 7"), "{}", entry.reason);
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            json["outcome"],
            json!({ "result": "not_sent", "cause": "session_lost" })
        );
        assert_eq!(json["caused_by"], 7);
    }

    #[test]
    fn a_lost_vci_keeps_the_send_the_evidence_gave_and_a_refusal_after_it_is_not_sent() {
        let pending_before = {
            let mut evidence = forwarding(evidence(
                &READ_F186,
                End::VciLost {
                    refused_after_loss: false,
                },
            ));
            evidence.pending = vec![pending()];
            evidence
        };
        let entry = ExchangeEntry::build(
            5,
            pending_before.clone(),
            LinkState::default().history_for(&pending_before),
        );
        assert_eq!(entry.classification, Classification::Undetermined);
        assert_eq!(entry.sent, Sent::Confirmed);
        let mut silent = evidence(
            &READ_F186,
            End::VciLost {
                refused_after_loss: false,
            },
        );
        silent.supply_voltage = Some(SupplyVoltage {
            millivolts: 11_800,
            age_ms: 250,
        });
        let entry = ExchangeEntry::build(6, silent, LinkHistory::empty());
        assert_eq!(entry.sent, Sent::Unknown);
        assert!(entry.reason.contains("11800 mV"));
        let refused = evidence(
            &READ_F186,
            End::VciLost {
                refused_after_loss: true,
            },
        );
        let entry = ExchangeEntry::build(7, refused, LinkHistory::empty());
        assert_eq!(entry.sent, Sent::NotSent);
        assert_eq!(entry.classification, Classification::Undetermined);
    }

    #[test]
    fn failures_of_the_system_are_system() {
        for cause in [
            UnknownCause::RpcFailure,
            UnknownCause::WorkerNotAnswering,
            UnknownCause::EventsEnded,
            UnknownCause::EventsLost,
            UnknownCause::PrimitiveFailedToStart,
        ] {
            let entry = ExchangeEntry::build(
                1,
                evidence(&READ_F186, End::Unknown(cause)),
                LinkHistory::empty(),
            );
            assert_eq!(entry.classification, Classification::System);
            assert_eq!(entry.sent, Sent::Unknown);
        }
    }

    #[test]
    fn the_deadline_and_link_errors_by_kind() {
        let verdict = verdict(&evidence(&READ_F186, End::VciDidNotEnd));
        assert_eq!(verdict.classification, Classification::Undetermined);
        for (kind, class) in [
            (LinkErrorKind::VciHardwareFault, Classification::System),
            (LinkErrorKind::ResourceLocked, Classification::System),
            (LinkErrorKind::Initialisation, Classification::Undetermined),
            (LinkErrorKind::Receive, Classification::Undetermined),
            (LinkErrorKind::Protocol, Classification::Undetermined),
            (LinkErrorKind::Frame, Classification::Undetermined),
            (LinkErrorKind::TesterPresent, Classification::Undetermined),
            (LinkErrorKind::Other, Classification::Undetermined),
        ] {
            assert_eq!(
                verdict_of(&evidence(&READ_F186, End::LinkError(kind))).0,
                class,
                "{kind:?}"
            );
        }
        let entry = ExchangeEntry::build(
            1,
            evidence(&READ_F186, End::LinkError(LinkErrorKind::Transmit)),
            LinkHistory::empty(),
        );
        assert_eq!(entry.classification, Classification::Undetermined);
        assert_eq!(entry.sent, Sent::Unknown);
        assert!(entry.reason.contains("detail is not available"));
    }

    #[test]
    fn a_late_response_settles_by_its_window() {
        use super::super::timing::LateWindow;
        let mut silent = forwarding(evidence(&READ_F186, End::NoResponse));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        silent.vci_ended = true;
        silent.timing.window = Window::TimeoutAtLimit;
        let kept = |classification| Kept {
            evidence: silent.clone(),
            origin: silent.origin,
            request: Request::of(&silent),
            classification,
            sent: Sent::Confirmed,
            transmit: None,
            pendings: Vec::new(),
            pending_order: Vec::new(),
            answer_order: Vec::new(),
            answer_ends: Vec::new(),
            limits: None,
            start_unattached: false,
            late_start: None,
            late_start_id: None,
            answer_at: AnswerTime::None,
            late_pendings: Vec::new(),
            late_places: Vec::new(),
            floor: None,
            partial: None,
            pending_at_end: false,
            clears: Clears::default(),
            matched: false,
        };
        let settle = |kept: &Kept, response: &[u8], window| {
            let answered = kept.answered(response, &LateAnswer::default());
            settle_late_response(
                kept,
                &answered,
                window,
                &LinkHistory::empty(),
                None,
                response,
            )
        };
        let positive = [0x62, 0xF1, 0x86, 0x01];
        let vehicle = kept(Classification::Vehicle);
        assert_eq!(
            settle(&vehicle, &positive, LateWindow::InTime { at_us: 40_000 }).0,
            Classification::System
        );
        let (class, reason) = settle(&vehicle, &positive, LateWindow::Late { by_us: 12_500 });
        assert_eq!(class, Classification::Vehicle);
        assert!(reason.contains("12.5 ms after the limit"), "{reason}");
        assert_eq!(
            settle(&vehicle, &positive, LateWindow::Open).0,
            Classification::Undetermined
        );
        // Late after a timeout that was not judged at the limit: the VCI may have ended its
        // wait early.
        let mut unjudged = vehicle.clone();
        unjudged.evidence.timing.window = Window::NotJudged;
        assert_eq!(
            settle(&unjudged, &positive, LateWindow::Late { by_us: 1 }).0,
            Classification::Undetermined
        );
        // Late after a timeout at P2 on a link whose VCI absorbs pendings: had the ECU sent one
        // the VCI absorbed, it should have waited longer (H4, ADR-376).
        let mut absorbing = vehicle.clone();
        absorbing.evidence.pending_visibility = PendingVisibility::AbsorbedByVci;
        let (class, reason) = settle(&absorbing, &positive, LateWindow::Late { by_us: 1 });
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("(H4)"), "{reason}");
        // Late, but its outcome's own row is not `vehicle`: an invalid key stays undetermined.
        assert_eq!(
            settle(&vehicle, &[0x7F, 0x22, 0x35], LateWindow::Late { by_us: 1 }).0,
            Classification::Undetermined
        );
        // A fault of the system the entry observed stands.
        let (class, reason) = settle(
            &kept(Classification::System),
            &positive,
            LateWindow::Late { by_us: 1 },
        );
        assert_eq!(class, Classification::System);
        assert!(reason.contains("stands"), "{reason}");
    }

    /// ADR-429: a late start bears on a late pending of the identifier it was judged on only.
    #[test]
    fn a_late_start_bears_on_a_late_pending_of_its_identifier_only() {
        use super::super::timing::LateWindow;
        let mut silent = forwarding(evidence(&READ_F186, End::NoResponse));
        silent.vci_ended = true;
        silent.collecting = true;
        silent.timing.window = Window::NotJudged;
        let kept = Kept {
            evidence: silent.clone(),
            origin: silent.origin,
            request: Request::of(&silent),
            classification: Classification::None,
            sent: Sent::Confirmed,
            transmit: None,
            pendings: Vec::new(),
            pending_order: Vec::new(),
            answer_order: Vec::new(),
            answer_ends: Vec::new(),
            limits: None,
            start_unattached: false,
            late_start: Some(LateStart::Late {
                by_us: 1,
                at_limit: true,
            }),
            late_start_id: Some(0x7E8),
            answer_at: AnswerTime::None,
            late_pendings: Vec::new(),
            late_places: Vec::new(),
            floor: None,
            partial: None,
            pending_at_end: false,
            clears: Clears::default(),
            matched: false,
        };
        let judged = |id| {
            settle_late_pending(
                &kept,
                LateWindow::Late { by_us: 1 },
                Classification::None,
                "",
                id,
            )
            .0
        };
        assert_eq!(judged(Some(0x7E8)), Classification::Vehicle);
        assert_eq!(judged(Some(0x7E9)), Classification::Undetermined);
    }

    #[test]
    fn a_primitive_end_keeps_an_observed_fault() {
        // A link error after the VCI's clock showed its own limits past: the end does not
        // refute that, so `system` stands.
        let entry = ExchangeEntry::build(
            1,
            evidence(&READ_F186, End::LinkError(LinkErrorKind::Receive)),
            LinkHistory::empty(),
        );
        assert_eq!(entry.classification, Classification::Undetermined);
        assert_eq!(
            settle_primitive_ended(&entry, false, false).0,
            Classification::Undetermined
        );
        let (class, reason) = settle_primitive_ended(&entry, true, false);
        assert_eq!(class, Classification::System);
        assert!(reason.contains("does not refute"), "{reason}");
        // The system fault of a start the worker did not answer stands under the same end
        // (ADR-416).
        let (class, reason) = settle_primitive_ended(&entry, false, true);
        assert_eq!(class, Classification::System);
        assert!(reason.contains("had not answered"), "{reason}");
        // A final response is `system` whatever its time.
        let positive = ExchangeEntry::build(
            1,
            forwarding(evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86]))),
            LinkHistory::empty(),
        );
        assert_eq!(
            settle_primitive_ended(&positive, false, false).0,
            Classification::System
        );
    }

    #[test]
    fn a_settled_send_owns_no_late_transmit_indication() {
        // A request with no transmit evidence and no answer: `sent` `unknown`, owed.
        let mut state = LinkState::default();
        let silent = evidence(&READ_F186, End::NoResponse);
        let entry = log(&mut state, 1, silent.clone());
        assert_eq!(entry.sent, Sent::Unknown);
        assert!(state.may_own_tx_indication());
        state.keep(
            1,
            Kept {
                evidence: silent.clone(),
                origin: silent.origin,
                request: Request::of(&silent),
                classification: entry.classification,
                sent: entry.sent,
                transmit: None,
                pendings: Vec::new(),
                pending_order: Vec::new(),
                answer_order: Vec::new(),
                answer_ends: Vec::new(),
                limits: None,
                start_unattached: false,
                late_start: None,
                late_start_id: None,
                answer_at: AnswerTime::None,
                late_pendings: Vec::new(),
                late_places: Vec::new(),
                floor: None,
                partial: None,
                pending_at_end: false,
                clears: Clears::default(),
                matched: false,
            },
        );
        assert!(state.settle_owed(1).is_some());
        assert!(!state.may_own_tx_indication());
        assert!(
            state
                .late_response_owners(None, &[0x62, 0xF1, 0x86])
                .is_empty()
        );
    }

    #[test]
    fn the_vci_s_own_limits_past_make_the_deadline_system() {
        let mut past = evidence(&READ_F186, End::VciDidNotEnd);
        past.transmit_evidence = Some(TransmitEvidence::TxIndication);
        past.vci_limits_past_by_us = Some(12_500);
        let verdict = verdict(&past);
        assert_eq!(verdict.classification, Classification::System);
        assert!(
            verdict.reason.contains("12.5 ms past the latest limit"),
            "{}",
            verdict.reason
        );
        // Not shown past: undetermined, as before.
        let mut not_shown = past.clone();
        not_shown.vci_limits_past_by_us = None;
        assert_eq!(verdict_of(&not_shown).0, Classification::Undetermined);
        // A send not confirmed: the VCI may not have started its timing.
        let mut unconfirmed = past.clone();
        unconfirmed.transmit_evidence = None;
        assert_eq!(verdict_of(&unconfirmed).0, Classification::Undetermined);
        // A collecting entry with answers past its limits stays the system's, and one without
        // answers takes the generic row when they are not (ADR-434).
        let mut collecting_past = deadline_ended(vec![positive(RX, Window::WithinP2)]);
        collecting_past.vci_limits_past_by_us = Some(12_500);
        let (class, reason) = verdict_of(&collecting_past);
        assert_eq!(class, Classification::System, "{reason}");
        assert!(reason.contains("12.5 ms past the latest limit"), "{reason}");
        let silent = deadline_ended(Vec::new());
        let (class, reason) = verdict_of(&silent);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.starts_with("The agent stopped waiting"), "{reason}");
    }

    #[test]
    fn a_suppressed_response_rows() {
        // No response: undetermined, naming the hidden causes that apply.
        let mut silent = evidence(&[0x3E, 0x80], End::NoResponse);
        silent.suppressed = true;
        let (class, reason) = verdict_of(&silent);
        assert_eq!(class, Classification::Undetermined);
        for hidden in ["H1", "H2", "H4"] {
            assert!(reason.contains(hidden), "{hidden}: {reason}");
        }
        assert!(!reason.contains("H3"));
        silent.link_errors = vec![LinkErrorKind::Receive];
        assert!(verdict_of(&silent).1.contains("H3"));
        // A positive response nobody asked for: vehicle on a forwarding link when its time is
        // judged within P2, undetermined when it is not (it may have come late, with the VCI
        // past its limit, ADR-376) and on an absorbing link.
        let mut positive = evidence(&[0x3E, 0x80], End::Positive(vec![0x7E, 0x00]));
        positive.suppressed = true;
        assert_eq!(verdict_of(&positive).0, Classification::Undetermined);
        let positive = forwarding(positive);
        assert_eq!(verdict_of(&positive).0, Classification::Undetermined);
        assert_eq!(verdict_of(&judged(positive)).0, Classification::Vehicle);
    }

    #[test]
    fn an_unusable_response_is_undetermined() {
        let evidence = forwarding(evidence(
            &READ_F186,
            End::PositiveUnusable(vec![0x62, 0xF1]),
        ));
        assert_eq!(verdict_of(&evidence).0, Classification::Undetermined);
    }

    #[test]
    fn any_outcome_after_a_late_forwarded_pending_is_system() {
        for end in [
            End::Positive(vec![0x62, 0xF1, 0x86]),
            End::NoResponse,
            End::VciLost {
                refused_after_loss: false,
            },
        ] {
            let mut late = forwarding(evidence(&READ_F186, end.clone()));
            late.pending = vec![pending()];
            late.timing.window = Window::Late;
            late.timing.window_by_ms = Some(3.0);
            late.timing.late_pending = Some(0);
            let verdict = verdict(&late);
            let (class, reason, text) = (verdict.classification, verdict.reason, verdict.text);
            assert_eq!(class, Classification::System, "{end:?}: {reason}");
            assert!(
                reason.contains("response pending 1 (0x78) 3.0 ms after the limit in force"),
                "{reason}"
            );
            assert!(reason.contains("did not end its wait"), "{reason}");
            // The outcome that followed is named, and the final response is not called late.
            assert!(text.contains("pending 1 of them 3.0 ms after"), "{text}");
            if matches!(end, End::Positive(_)) {
                assert!(reason.ends_with("responded positively."), "{reason}");
                assert!(!text.contains("positively 3.0 ms"), "{text}");
            }
        }
    }

    #[test]
    fn a_final_response_or_a_timeout_past_the_limit_is_system() {
        // A final response after the limit in force: two faults, the outcome named.
        let mut late = forwarding(evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86])));
        late.timing.window = Window::Late;
        late.timing.window_by_ms = Some(12.5);
        let (class, reason) = verdict_of(&late);
        assert_eq!(class, Classification::System);
        assert!(reason.contains("by 12.5 ms"), "{reason}");
        assert!(reason.contains("responded positively"), "{reason}");
        let mut refused = evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, 0x22]));
        refused.timing.window = Window::Late;
        let (class, reason) = verdict_of(&refused);
        assert_eq!(class, Classification::System);
        assert!(reason.contains("conditionsNotCorrect"), "{reason}");
        // A receive timeout early or late. Where a response had begun, the timeout is the
        // transport's and `timing` does not judge it, so only `no_response` has these rows.
        for end in [End::NoResponse] {
            for (window, phrase) in [
                (Window::TimeoutEarly, "gave up waiting before"),
                (Window::TimeoutLate, "after the limit in force"),
            ] {
                let mut timed_out = evidence(&READ_F186, end.clone());
                timed_out.timing.window = window;
                timed_out.timing.window_by_ms = Some(3.0);
                let (class, reason) = verdict_of(&timed_out);
                assert_eq!(class, Classification::System, "{reason}");
                assert!(
                    reason.contains(phrase) && reason.contains("by 3.0 ms"),
                    "{reason}"
                );
            }
        }
    }

    #[test]
    fn classification_is_ordered_by_severity() {
        use Classification::{None as NoFault, System, Undetermined, Vehicle};
        assert!(NoFault < Vehicle && Vehicle < Undetermined && Undetermined < System);
        assert_eq!(Vehicle.max(Undetermined), Undetermined);
    }

    #[test]
    fn an_answer_an_owed_request_could_own_is_undetermined_and_confirms_nothing() {
        let mut state = LinkState::default();
        for seq in 1..=3 {
            let entry = log(&mut state, seq, read([0xF1, 0x86], End::NoResponse));
            assert_eq!(entry.sent, Sent::Unknown);
        }
        let entry = log(
            &mut state,
            4,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(entry.link_history.owed_matching, 3);
        assert!(entry.link_history.this_exchange);
        assert_eq!(entry.sent, Sent::Unknown);
        assert_eq!(entry.sent_evidence, SentEvidence::None);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("3 earlier request(s)"));
        // The answer counts as an answer, not as a matched one; all four stay owed.
        let next = read([0xF1, 0x86], End::NoResponse);
        let history = state.history_for(&next);
        assert_eq!(history.answered, 1);
        assert_eq!(history.answered_same_match, 0);
        let next = log(&mut state, 5, next);
        assert_eq!(next.link_history.owed_matching, 0);
        // A negative answer to another read can belong to the owed read of the same service.
        let other = read([0xF1, 0x90], End::Negative(vec![0x7F, 0x22, 0x31]));
        assert_eq!(state.history_for(&other).owed_matching, 5);
    }

    #[test]
    fn an_answer_no_owed_request_could_own_confirms_the_send_and_is_matched() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        let entry = log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(entry.link_history.owed_matching, 0);
        assert_eq!(entry.sent, Sent::Confirmed);
        assert_eq!(entry.sent_evidence, SentEvidence::Answer);
        let again = read([0xF1, 0x86], End::NoResponse);
        let history = state.history_for(&again);
        assert_eq!((history.answered, history.answered_same_match), (1, 1));
        // Another read has no matched answer.
        let other = read([0xF1, 0x90], End::NoResponse);
        assert_eq!(state.history_for(&other).answered_same_match, 0);
        // The answered exchange is not owed; the silent one still is.
        let probe = read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89]));
        assert_eq!(state.history_for(&probe).owed_matching, 1);
        let probe = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86]));
        assert_eq!(state.history_for(&probe).owed_matching, 0);
    }

    #[test]
    fn a_pending_then_silence_stays_owed() {
        let mut state = LinkState::default();
        let mut stalled = forwarding(read([0xF1, 0x86], End::NoResponse));
        stalled.pending = vec![pending()];
        log(&mut state, 1, stalled);
        let later = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86]));
        assert_eq!(state.history_for(&later).owed_matching, 1);
    }

    #[test]
    fn an_accepted_reset_ends_the_evidence_and_the_answer_that_accepted_it_does_not_count() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            1
        );
        let reset = log(
            &mut state,
            3,
            evidence(&[0x11, 0x01], End::Positive(vec![0x51, 0x01])),
        );
        assert_eq!(reset.sent, Sent::Confirmed);
        assert_eq!(state.last_reset(), Some(3));
        // Matched answers and owed requests are gone, and the reset's own answer is not matched.
        let after = read([0xF1, 0x86], End::NoResponse);
        let history = state.history_for(&after);
        assert_eq!(history.answered_same_match, 0);
        assert_eq!(history.answered, 2);
        let repeat = evidence(&[0x11, 0x01], End::NoResponse);
        assert_eq!(state.history_for(&repeat).answered_same_match, 0);
        let owed = read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89]));
        assert_eq!(state.history_for(&owed).owed_matching, 0);
        // An answered read after the reset counts again.
        log(
            &mut state,
            4,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            1
        );
        // A session change ends them too.
        log(
            &mut state,
            5,
            evidence(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
        );
        assert_eq!(
            state
                .history_for(&read([0xF1, 0x86], End::NoResponse))
                .answered_same_match,
            0
        );
    }

    #[test]
    fn an_accepting_answer_several_requests_could_own_ends_only_those_before_the_first() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(&mut state, 2, evidence(&[0x11, 0x01], End::NoResponse));
        log(&mut state, 3, read([0xF1, 0x90], End::NoResponse));
        let reset = log(
            &mut state,
            4,
            evidence(&[0x11, 0x01], End::Positive(vec![0x51, 0x01])),
        );
        assert_eq!(reset.link_history.owed_matching, 1);
        assert_eq!(reset.classification, Classification::Undetermined);
        assert_eq!(reset.sent, Sent::Unknown);
        // The read before the earlier reset ended; the reset and what followed it stay owed, and
        // so does the later reset.
        let owners = |end| {
            let probe = read([0xF1, 0x89], end);
            state.history_for(&probe).owed_matching
        };
        assert_eq!(owners(End::Positive(vec![0x62, 0xF1, 0x89])), 0);
        let probe = read([0xF1, 0x90], End::Positive(vec![0x62, 0xF1, 0x90]));
        assert_eq!(state.history_for(&probe).owed_matching, 1);
        let probe = evidence(&[0x11, 0x01], End::Positive(vec![0x51, 0x01]));
        assert_eq!(state.history_for(&probe).owed_matching, 2);
    }

    #[test]
    fn a_suppressed_reset_classified_none_ends_the_owed_requests() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        let mut reset = evidence(&[0x11, 0x81], End::NoResponse);
        reset.suppressed = true;
        let history = state.history_for(&reset);
        let mut entry = ExchangeEntry::build(2, reset, history);
        // The `none` row needs a confirmed send, an earlier answer matched this way and a
        // timeout judged at P2; the classification is set directly to test what `none` does to
        // the link's history.
        entry.classification = Classification::None;
        state.record(&entry);
        assert_eq!(state.last_reset(), Some(2));
        let probe = read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89]));
        assert_eq!(state.history_for(&probe).owed_matching, 0);
        // As undetermined it may still have been carried out, which nothing shows (ADR-376):
        // it ends the matched answers, as an unusable answer would, but not the owed requests.
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        let matched = read([0xF1, 0x86], End::NoResponse);
        assert_eq!(state.history_for(&matched).answered_same_match, 1);
        let mut reset = evidence(&[0x11, 0x81], End::NoResponse);
        reset.suppressed = true;
        let entry = log(&mut state, 3, reset.clone());
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(!entry.matched_answer());
        assert_eq!(state.last_reset(), None);
        assert_eq!(state.history_for(&probe).owed_matching, 1);
        assert_eq!(state.history_for(&matched).answered_same_match, 0);
        // One the agent refused before sending it changes nothing.
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        reset.end = End::NotSent(RefusalCause::Policy);
        log(&mut state, 2, reset);
        assert_eq!(state.history_for(&matched).answered_same_match, 1);
    }

    #[test]
    fn a_link_error_ends_the_matched_answers_and_the_count_but_not_the_owed_requests() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        let failed = log(
            &mut state,
            3,
            read([0xF1, 0x90], End::LinkError(LinkErrorKind::Receive)),
        );
        assert_eq!(failed.sent, Sent::Unknown);
        assert_eq!(state.last_link_error(), Some((3, LinkErrorKind::Receive)));
        let probe = read([0xF1, 0x86], End::NoResponse);
        let history = state.history_for(&probe);
        assert_eq!(history.answered, 1);
        assert_eq!(history.answered_since_error, 0);
        assert_eq!(history.answered_same_match, 0);
        let owed = read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89]));
        assert_eq!(state.history_for(&owed).owed_matching, 1);
        // An error during an exchange does the same, and the answer in it does not survive it.
        let mut state = LinkState::default();
        let mut during = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86]));
        during.link_errors = vec![LinkErrorKind::Frame];
        log(&mut state, 1, during);
        let history = state.history_for(&probe);
        assert_eq!(
            (
                history.answered,
                history.answered_since_error,
                history.answered_same_match
            ),
            (1, 0, 0)
        );
    }

    #[test]
    fn an_answer_after_an_error_of_its_exchange_counts_since_that_error() {
        let probe = read([0xF1, 0x86], End::NoResponse);
        let counts_after = |before_answer: bool| {
            let mut state = LinkState::default();
            let mut exchange = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1]));
            exchange.link_errors = vec![LinkErrorKind::Frame];
            exchange.link_errors_before_answer = before_answer;
            log(&mut state, 1, exchange);
            assert_eq!(state.last_link_error(), Some((1, LinkErrorKind::Frame)));
            let history = state.history_for(&probe);
            (
                history.answered,
                history.answered_since_error,
                history.answered_same_match,
            )
        };
        // The answer came after the error: it is an answer since the error and a matched answer.
        assert_eq!(counts_after(true), (1, 1, 1));
        // The error came after the answer (or no order is known): it ends the answer.
        assert_eq!(counts_after(false), (1, 0, 0));
        // An error of another link of the channel has no order against the answer: it ends it.
        let mut state = LinkState::default();
        let mut exchange = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1]));
        exchange.link_errors = vec![LinkErrorKind::Frame];
        exchange.link_errors_before_answer = true;
        exchange.channel_link_errors = vec![LinkErrorKind::Receive];
        log(&mut state, 1, exchange);
        assert_eq!(state.history_for(&probe).answered_since_error, 0);
        // Earlier matched answers still end with the error.
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            read([0xF1, 0x90], End::Positive(vec![0x62, 0xF1, 0x90, 1])),
        );
        let mut exchange = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1]));
        exchange.link_errors = vec![LinkErrorKind::Frame];
        exchange.link_errors_before_answer = true;
        log(&mut state, 2, exchange);
        let history = state.history_for(&read([0xF1, 0x90], End::NoResponse));
        assert_eq!(
            (history.answered_since_error, history.answered_same_match),
            (1, 0)
        );
    }

    #[test]
    fn exchanges_that_cannot_have_arrived_owe_nothing() {
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            evidence(&READ_F186, End::NotSent(RefusalCause::Policy)),
        );
        log(
            &mut state,
            2,
            evidence(
                &READ_F186,
                End::VciLost {
                    refused_after_loss: true,
                },
            ),
        );
        let mut partial = evidence(&READ_F186, End::NoResponse);
        partial.flow_control = true;
        let entry = log(&mut state, 3, partial);
        assert_eq!(
            (entry.sent, entry.sent_evidence),
            (Sent::Partial, SentEvidence::FlowControl)
        );
        let probe = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86]));
        assert_eq!(state.history_for(&probe).owed_matching, 0);
    }

    #[test]
    fn a_transmit_confirmation_confirms_the_send() {
        for (transmit, expected) in [
            (TransmitEvidence::TxIndication, SentEvidence::TxIndication),
            (TransmitEvidence::Loopback, SentEvidence::Loopback),
            (TransmitEvidence::TxDoneTime, SentEvidence::TxDoneTime),
        ] {
            let mut sent = evidence(&READ_F186, End::NoResponse);
            sent.transmit_evidence = Some(transmit);
            assert_eq!(
                sent_of(&sent, &LinkHistory::empty()),
                (Sent::Confirmed, expected)
            );
            // With no matched answer, a confirmed send and no response stays undetermined (H2).
            assert_eq!(verdict_of(&sent).0, Classification::Undetermined);
        }
    }

    /// Design 8.11's two `no_response` rows that stand on a confirmed send: `vehicle` from an
    /// ECU that has answered requests matched this way and with no link error, `undetermined`
    /// otherwise (H2 without a matched answer, H3 with a link error). Both come after the
    /// possible-pending row, so a link that forwards pendings reaches them.
    #[test]
    fn a_confirmed_send_with_no_response_is_judged_by_its_matched_answers() {
        let mut state = LinkState::default();
        let answered = read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1]));
        log(&mut state, 1, forwarding(answered));
        let mut silent = judged(forwarding(read([0xF1, 0x86], End::NoResponse)));
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let judged = |evidence: &Evidence, state: &LinkState| {
            let verdict = classify(evidence, &state.history_for(evidence));
            (verdict.classification, verdict.reason)
        };
        let (classification, reason) = judged(&silent, &state);
        assert_eq!(classification, Classification::Vehicle, "{reason}");
        assert!(reason.starts_with("Sent on the bus"), "{reason}");
        // Another identifier has no matched answer: H2.
        let other = {
            let mut other = forwarding(read([0xF1, 0x90], End::NoResponse));
            other.transmit_evidence = Some(TransmitEvidence::TxIndication);
            other
        };
        let (classification, reason) = judged(&other, &state);
        assert_eq!(classification, Classification::Undetermined);
        assert!(
            reason.contains("(H2)") && !reason.contains("(H1)"),
            "{reason}"
        );
        // A link error during the exchange: H3.
        let mut erred = silent.clone();
        erred.channel_link_errors = vec![LinkErrorKind::Receive];
        let (classification, reason) = judged(&erred, &state);
        assert_eq!(classification, Classification::Undetermined);
        assert!(reason.contains("(H3)"), "{reason}");
        // A link error and no matched answer: both readings stay open.
        let mut both = other.clone();
        both.channel_link_errors = vec![LinkErrorKind::Receive];
        let (_, reason) = judged(&both, &state);
        assert!(
            reason.contains("(H2)") && reason.contains("(H3)"),
            "{reason}"
        );
        // On a link whose VCI absorbs pendings, with the time not judged, the possible-pending
        // row comes first (H4).
        let mut absorbed = silent.clone();
        absorbed.pending_visibility = PendingVisibility::AbsorbedByVci;
        let (classification, reason) = judged(&absorbed, &state);
        assert_eq!(classification, Classification::Undetermined);
        assert!(
            reason.contains("(H4)") && !reason.contains("(H1)"),
            "{reason}"
        );
    }

    /// An exchange that ended with `sent` `unknown` could still own a transmit indication that
    /// comes late, on the same opened link (design 8.11 `sent_evidence`); one confirmed or not
    /// sent owns none.
    #[test]
    fn an_unknown_send_may_own_a_late_transmit_indication_until_the_link_reopens() {
        let mut state = LinkState::default();
        let mut confirmed = read([0xF1, 0x86], End::NoResponse);
        confirmed.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log(&mut state, 1, confirmed);
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::NotSent(RefusalCause::Policy)),
        );
        assert!(!state.may_own_tx_indication());
        log(&mut state, 3, read([0xF1, 0x86], End::NoResponse));
        assert!(state.may_own_tx_indication());
        state.reopened();
        assert!(!state.may_own_tx_indication());
    }

    /// Every outcome and cause the classification has, for the checks over all of them.
    fn all_cases() -> Vec<Evidence> {
        let mut cases = Vec::new();
        let mut ends = vec![
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
            End::PositiveUnusable(vec![0x62]),
            End::NoResponse,
            End::VciDidNotEnd,
            End::VciLost {
                refused_after_loss: false,
            },
            End::VciLost {
                refused_after_loss: true,
            },
            End::NotSent(RefusalCause::Policy),
            End::NotSent(RefusalCause::BlockTooLong),
            End::NotSent(RefusalCause::PreparationError),
            End::NotSent(RefusalCause::NoTransferActive),
            End::NotSent(RefusalCause::EmptyRequest),
            End::NotSent(RefusalCause::UndeclaredService),
            End::NotSent(RefusalCause::SystemOwnReasons),
            End::NotSent(RefusalCause::KeepAliveFailed),
            End::NotSent(RefusalCause::EarlierResponse),
            End::NotSent(RefusalCause::SessionLost),
            End::NotSent(RefusalCause::StreamFailed),
            End::Unknown(UnknownCause::RpcFailure),
            End::Unknown(UnknownCause::WorkerNotAnswering),
            End::Unknown(UnknownCause::EventsEnded),
            End::Unknown(UnknownCause::EventsLost),
            End::Unknown(UnknownCause::PrimitiveFailedToStart),
            End::Negative(vec![0x7F, 0x22]),
        ];
        for kind in [
            LinkErrorKind::VciHardwareFault,
            LinkErrorKind::ResourceLocked,
            LinkErrorKind::Initialisation,
            LinkErrorKind::Receive,
            LinkErrorKind::Protocol,
            LinkErrorKind::Frame,
            LinkErrorKind::TesterPresent,
            LinkErrorKind::Transmit,
            LinkErrorKind::Other,
        ] {
            ends.push(End::LinkError(kind));
        }
        ends.extend((0..=0xFFu8).map(|code| End::Negative(vec![0x7F, 0x22, code])));
        for end in ends {
            for forwarded in [false, true] {
                for window in [Window::NotJudged, Window::WithinP2, Window::TimeoutAtLimit] {
                    for suppressed in [false, true] {
                        let mut case = evidence(&READ_F186, end.clone());
                        case.suppressed = suppressed;
                        case.timing.window = window;
                        if forwarded {
                            case.pending_visibility = PendingVisibility::Forwarded;
                        }
                        cases.push(case.clone());
                        // A confirmed send alone, which the rows standing on one take.
                        let mut confirmed = case.clone();
                        confirmed.transmit_evidence = Some(TransmitEvidence::TxIndication);
                        cases.push(confirmed);
                        case.pending = vec![pending()];
                        cases.push(case.clone());
                        case.link_errors = vec![LinkErrorKind::Receive];
                        case.channel_link_errors = vec![LinkErrorKind::Frame];
                        case.supply_voltage = Some(SupplyVoltage {
                            millivolts: 12_000,
                            age_ms: 5,
                        });
                        cases.push(case.clone());
                        case.flow_control = true;
                        cases.push(case.clone());
                        case.transmit_evidence = Some(TransmitEvidence::TxIndication);
                        cases.push(case);
                    }
                }
            }
        }
        cases
    }

    #[test]
    fn no_text_or_reason_names_an_internal_component() {
        // Words, matched without regard to case.
        const DENIED_WORDS: [&str; 8] = [
            "pdu_",
            "rpc",
            "grpc",
            "startcomprimitive",
            "worker",
            "cop",
            "host",
            "tonic",
        ];
        // The host error's variant names, matched as written (`NotAllowed` is left out: the
        // standard's name of code 0x39 holds it).
        const DENIED_NAMES: [&str; 13] = [
            "WorkerUnresponsive",
            "EventStreamEnded",
            "NoResponse",
            "EventsLost",
            "VciLost",
            "PrimitiveFailed",
            "BadService",
            "NoTransferActive",
            "TransferRefused",
            "BlockTooLong",
            "NoBlockLength",
            "UseFlashTransfer",
            "UndeclaredService",
        ];
        let cases = all_cases();
        assert!(cases.len() > 1000);
        for (index, case) in cases.into_iter().enumerate() {
            let mut state = LinkState::default();
            // An owed request makes the case an ownership case as well; every other case also
            // has a matched answer before it, which the rows on a confirmed send take.
            log(&mut state, 1, read([0xF1, 0x86], End::NoResponse));
            if index % 2 == 1 {
                log(
                    &mut state,
                    2,
                    read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
                );
            }
            let history = state.history_for(&case);
            let entry = ExchangeEntry::build(3, case, history);
            for field in [&entry.text, &entry.reason] {
                let lower = field.to_lowercase();
                for denied in DENIED_WORDS {
                    assert!(!lower.contains(denied), "{denied:?} in {field:?}");
                }
                for denied in DENIED_NAMES {
                    assert!(!field.contains(denied), "{denied:?} in {field:?}");
                }
            }
            assert!(!entry.text.is_empty() && !entry.reason.is_empty());
            // The send is worded from `sent`: "sent on the bus" only for a confirmed one.
            for field in [&entry.text, &entry.reason] {
                assert!(
                    entry.sent == Sent::Confirmed || !field.contains("Sent on the bus"),
                    "{:?} {:?}: {field:?}",
                    entry.sent,
                    entry.outcome
                );
            }
        }
    }

    fn entry_json(evidence: Evidence) -> Value {
        let history = LinkState::default().history_for(&evidence);
        serde_json::to_value(ExchangeEntry::build(7, evidence, history)).unwrap()
    }

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn the_entry_has_the_keys_of_design_8_11() {
        // Every key design 8.11's table lists for an exchange entry, with `event`.
        let mut all = vec![
            "event",
            "seq",
            "target",
            "link",
            "origin",
            "at",
            "request",
            "sent",
            "sent_evidence",
            "caused_by",
            "outcome",
            "answers",
            "pending",
            "pending_visibility",
            "timing",
            "times",
            "link_history",
            "supply_voltage",
            "text",
            "classification",
            "reason",
            "detail",
        ];
        all.sort_unstable();
        let mut full = evidence(&READ_F186, End::NoResponse);
        full.at = Some(At {
            pc: 212,
            steps: 1840,
            source: Some(SourcePos {
                file_index: 0,
                line: 57,
                column: 9,
            }),
            sections: vec![SectionAttrs {
                interruptible: Interruptible::No,
                idempotency: Idempotency::Safe,
                expected_millis: 180_000,
            }],
        });
        full.supply_voltage = Some(SupplyVoltage {
            millivolts: 12_100,
            age_ms: 40,
        });
        full.answers = vec![an_answer()];
        full.pending = vec![pending()];
        full.caused_by = Some(3);
        let json = entry_json(full);
        assert_eq!(keys(&json), all);
        assert_eq!(json["event"], "exchange");
        assert_eq!(json["seq"], 7);
        // The entry without the optional groups: no VM position, no answers list, no supply
        // reading.
        let json = entry_json(evidence(&READ_F186, End::NoResponse));
        let mut required: Vec<&str> = all.clone();
        required.retain(|key| !["at", "answers", "supply_voltage", "caused_by"].contains(key));
        assert_eq!(keys(&json), required);
        assert_eq!(json["detail"], json!({}));
        assert_eq!(json["request"]["sid"], "0x22");
        assert_eq!(json["request"]["service"], "ReadDataByIdentifier");
        assert_eq!(json["request"]["bytes"]["clear"], "22f186");
        assert_eq!(json["request"]["suppressed"], false);
        assert_eq!(
            keys(&json["link_history"]),
            [
                "answered",
                "answered_same_match",
                "answered_since_error",
                "owed_matching",
                "this_exchange",
            ]
        );
        assert_eq!(json["timing"]["window"], "not_judged");
        assert_eq!(json["timing"]["not_judged_reason"], "no_transmit_time");
        assert_eq!(json["classification"], "undetermined");
        // The groups: each carries the keys of design 8.11's table and no more.
        assert_eq!(
            keys(&json["request"]),
            ["bytes", "service", "sid", "suppressed"]
        );
        assert_eq!(keys(&json["request"]["bytes"]), ["clear"]);
        assert_eq!(keys(&json["outcome"]), ["result"]);
        assert_eq!(
            keys(&json["timing"]),
            ["measured_by", "not_judged_reason", "response_ms", "window"]
        );
        assert_eq!(keys(&json["times"]), ["agent"]);
        assert_eq!(
            keys(&json["times"]["agent"]),
            ["before_us", "end_us", "epoch"]
        );
        assert_eq!(keys(&json["target"]), ["link", "rx_id", "tx_id"]);
        let negative = entry_json(evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, 0x22])));
        assert_eq!(
            keys(&negative["outcome"]),
            ["nrc", "nrc_kind", "nrc_name", "response", "result"]
        );
        assert_eq!(
            negative["outcome"]["response"],
            json!({ "clear": "7f2222" })
        );
        let mut roled = evidence(&READ_F186, End::NoResponse);
        roled.target = Target::Role("primary".to_owned());
        assert_eq!(entry_json(roled)["target"], "primary");
        assert_eq!(
            json["target"],
            json!({ "link": 1, "tx_id": 0x7E0, "rx_id": RX })
        );
    }

    #[test]
    fn the_domain_enums_serialize_in_snake_case() {
        let value = |value: &dyn erased::Ser| value.to_value();
        for (got, want) in [
            (value(&Origin::Repeat), "repeat"),
            (value(&Origin::Identification), "identification"),
            (value(&Origin::Guard), "guard"),
            (value(&Origin::Precondition), "precondition"),
            (value(&Origin::Teardown), "teardown"),
            (value(&Window::TimeoutLate), "timeout_late"),
            (value(&Origin::RestartCheck), "restart_check"),
            (value(&Origin::AccessProgram), "access_program"),
            (value(&Origin::KeepAlive), "keep_alive"),
            (value(&Sent::NotSent), "not_sent"),
            (value(&SentEvidence::TxDoneTime), "tx_done_time"),
            (value(&SentEvidence::TxError), "tx_error"),
            (value(&Clock::Worker), "worker"),
            (value(&Clock::Device), "device"),
            (value(&PendingVisibility::AbsorbedByVci), "absorbed_by_vci"),
            (
                value(&MeasuredBy::VciTxDoneToRxStart),
                "vci_tx_done_to_rx_start",
            ),
            (
                value(&MeasuredBy::VciTxDoneToRxEnd),
                "vci_tx_done_to_rx_end",
            ),
            (value(&Window::WithinP2Star), "within_p2_star"),
            (value(&Window::PossiblePending), "possible_pending"),
            (value(&Window::TimeoutEarly), "timeout_early"),
            (value(&NrcKind::SecuredData), "secured_data"),
            (value(&NrcKind::UnknownCode), "unknown_code"),
            (value(&Classification::Undetermined), "undetermined"),
        ] {
            assert_eq!(got, json!(want));
        }
        // A role is a plain string, an endpoint an object (design 8.11's examples).
        let role = serde_json::to_value(Target::Role("primary".to_owned())).unwrap();
        assert_eq!(role, json!("primary"));
        let endpoint = serde_json::to_value(Target::Endpoint {
            link: 2,
            tx_id: 0x7DF,
            rx_id: 0x7E8,
        })
        .unwrap();
        assert_eq!(
            endpoint,
            json!({ "link": 2, "tx_id": 0x7DF, "rx_id": 0x7E8 })
        );
        let times = serde_json::to_value(Times {
            agent: AgentTimes {
                epoch: 1,
                before_us: 1,
                tx_confirmed_us: Some(2),
                end_us: 3,
                before_wall_unix_ms: Some(4),
                end_wall_unix_ms: Some(5),
            },
            vci: Some(VciTimes {
                tx_done: Some(VciTime::new(1, Some(Clock::Device))),
                rx_start: Some(VciTime::new(2, Some(Clock::Device))),
                rx_end: None,
                error: Some(VciTime::new(4, Some(Clock::Worker)).at(Some(Placed {
                    epoch: 3,
                    unwrapped: 4,
                }))),
                deadline: Some(ClockReading {
                    time: VciTime::new(9, Some(Clock::Device)),
                    round_trip_us: 700,
                }),
                response_read_us: Some(8),
                rx_timeout: None,
                finished: None,
            }),
        })
        .unwrap();
        // Each time carries its own clock and, once placed, its epoch; an absent one is left
        // out, and the unwrapped value is not written.
        assert_eq!(
            times["vci"],
            json!({
                "tx_done": { "us": 1, "clock": "device" },
                "rx_start": { "us": 2, "clock": "device" },
                "error": { "us": 4, "clock": "worker", "epoch": 3 },
                "deadline": { "us": 9, "clock": "device", "round_trip_us": 700 },
            })
        );
        // A time whose item named no clock has none, and no epoch.
        assert_eq!(
            serde_json::to_value(VciTime::new(7, None)).unwrap(),
            json!({ "us": 7 })
        );
        let pending = serde_json::to_value(Pending {
            vci_us: Some(1),
            clock: Some(Clock::Worker),
            epoch: None,
            agent_us: 2,
            since_previous_ms: Some(3.0),
        })
        .unwrap();
        assert_eq!(pending["clock"], "worker");
        let detail = serde_json::to_value(Detail {
            host_error: Some("a"),
            error_event: Some("b".to_owned()),
            extra_error_info: Some(3),
            extra_error_info_name: Some("c".to_owned()),
            grpc_status: Some("d"),
            worker_detail: Some("e".to_owned()),
            native_return_code: Some("f".to_owned()),
            cop_type: Some("startcomm"),
        })
        .unwrap();
        assert_eq!(keys(&detail).len(), 8);
        let span = SourceSpan {
            pc: 1,
            file: 2,
            line: 3,
            column: 4,
        };
        assert_eq!(
            SourcePos::from(&span),
            SourcePos {
                file_index: 2,
                line: 3,
                column: 4
            }
        );
        let section = Section {
            start_pc: 0,
            end_pc: 1,
            interruptible: Interruptible::RecoveryRequired,
            idempotency: Idempotency::CheckState,
            expected_millis: 180_000,
        };
        let attrs = SectionAttrs::from(&section);
        assert!(matches!(
            attrs.interruptible,
            Interruptible::RecoveryRequired
        ));
        assert_eq!(attrs.expected_millis, 180_000);
        assert_eq!(Expected::uds(&[]).positive_prefix, Vec::<u8>::new());
    }

    /// An answer of the answers list, for the evidence that holds one: a positive response with
    /// no time, so it is not judged.
    fn an_answer() -> Answer {
        Answer {
            rx_id: RX,
            end: End::Positive(vec![0x62, 0xF1, 0x86]),
            vci_us: None,
            clock: None,
            epoch: None,
            start_us: None,
            tx_done_us: None,
            agent_us: 1,
            pending: Vec::new(),
            timing: Timing::not_judged(),
            history: None,
        }
    }

    /// The matched answers and owed requests a link state gives a read of F186 and of F189.
    fn counts(state: &LinkState) -> (u32, u32) {
        let matched = state
            .history_for(&read([0xF1, 0x86], End::NoResponse))
            .answered_same_match;
        let owed = state
            .history_for(&read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89])))
            .owed_matching;
        (matched, owed)
    }

    /// A link with a matched answer (a read of F186) and an owed request (a read of F189),
    /// which then logs `request` as line 3 with `end`.
    fn link_after(request: &[u8], end: End) -> LinkState {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(counts(&state), (1, 1));
        log(&mut state, 3, evidence(request, end));
        state
    }

    #[test]
    fn an_owed_request_survives_a_reopened_link_and_only_the_matched_answers_start_over() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        assert_eq!(counts(&state), (1, 1));
        state.reopened();
        // The matched answer is gone, the owed request and the counts are not.
        assert_eq!(counts(&state), (0, 1));
        let history = state.history_for(&read([0xF1, 0x86], End::NoResponse));
        assert_eq!((history.answered, history.answered_since_error), (1, 1));
        // A later answer the owed request could own stays undetermined, with it counted.
        let late = log(
            &mut state,
            3,
            read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89, 7])),
        );
        assert_eq!(late.link_history.owed_matching, 1);
        assert_eq!(late.classification, Classification::Undetermined);
        assert_eq!(late.sent, Sent::Unknown);
    }

    #[test]
    fn a_channel_error_ends_the_matched_answers_of_an_idle_target_on_the_channel() {
        // Two targets on one physical channel: B is idle while A's exchange sees the error.
        let mut b = link_after(&[0x31, 0x01], End::Positive(vec![0x71, 0x01]));
        assert_eq!(counts(&b), (1, 1));
        b.channel_link_error(9, LinkErrorKind::Receive);
        assert_eq!(
            counts(&b),
            (0, 1),
            "matched answers end, the owed request stays"
        );
        assert_eq!(b.last_link_error(), Some((9, LinkErrorKind::Receive)));
        let history = b.history_for(&read([0xF1, 0x86], End::NoResponse));
        assert_eq!(history.answered_since_error, 0);
    }

    #[test]
    fn a_link_error_on_the_channel_counts_for_h3_and_ends_the_matched_answers() {
        // A pending then silence is `vehicle` only when no link error came on the link or on
        // another link of its channel.
        let mut silent = judged(forwarding(evidence(&READ_F186, End::NoResponse)));
        silent.pending = vec![pending()];
        silent.vci_ended = true;
        assert_eq!(verdict_of(&silent).0, Classification::Vehicle);
        silent.channel_link_errors = vec![LinkErrorKind::Receive];
        let (class, reason) = verdict_of(&silent);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("H3"), "{reason}");
        // The channel's error is evidence only: it adds no fault to an exchange that ended well.
        let mut positive = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86]),
        )));
        positive.channel_link_errors = vec![LinkErrorKind::VciHardwareFault];
        assert_eq!(verdict_of(&positive).0, Classification::None);
        // It ends the matched answers and the count since the error, not the owed requests.
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        log(
            &mut state,
            2,
            read([0xF1, 0x86], End::Positive(vec![0x62, 0xF1, 0x86, 1])),
        );
        let mut other = read([0xF1, 0x90], End::NoResponse);
        other.channel_link_errors = vec![LinkErrorKind::Frame];
        log(&mut state, 3, other);
        assert_eq!(state.last_link_error(), Some((3, LinkErrorKind::Frame)));
        assert_eq!(counts(&state), (0, 1));
        let history = state.history_for(&read([0xF1, 0x86], End::NoResponse));
        assert_eq!((history.answered, history.answered_since_error), (1, 0));
    }

    #[test]
    fn a_suppressed_silence_with_no_open_reading_does_not_contradict_itself() {
        let mut state = LinkState::default();
        log(
            &mut state,
            1,
            evidence(&[0x3E, 0x00], End::Positive(vec![0x7E, 0x00])),
        );
        let mut silent = forwarding(evidence(&[0x3E, 0x80], End::NoResponse));
        silent.suppressed = true;
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        silent.timing.window = Window::TimeoutAtLimit;
        silent.timing.not_judged_reason = None;
        let entry = log(&mut state, 2, silent.clone());
        assert_eq!(entry.link_history.answered_same_match, 1);
        // Sent, matched, no link error and a timeout at P2: nothing was missed (design 8.11).
        assert_eq!(entry.classification, Classification::None);
        assert!(!entry.reason.contains("but the evidence does not show"));
        assert!(
            entry
                .reason
                .contains("No response was expected, and none came"),
            "{}",
            entry.reason
        );
        // A timeout not judged does not show that the wait ran its course; on a link whose VCI
        // absorbs pendings, even a timeout at P2 may hide one (ADR-376).
        silent.timing.window = Window::NotJudged;
        let entry = log(&mut state, 3, silent.clone());
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(
            entry.reason.contains("not presumed right"),
            "{}",
            entry.reason
        );
        silent.timing.window = Window::TimeoutAtLimit;
        silent.pending_visibility = PendingVisibility::AbsorbedByVci;
        let entry = log(&mut state, 4, silent);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("(H4)"), "{}", entry.reason);
    }

    #[test]
    fn evidence_only_an_unimplemented_row_could_classify_takes_the_last_row_where_it_stands() {
        // A row before it still decides: a failure of the system with a window `timeout_late`,
        // a VCI fault with answers, a lost VCI with a window `late` and no pending.
        let mut failed = evidence(&READ_F186, End::Unknown(UnknownCause::RpcFailure));
        failed.timing.window = Window::TimeoutLate;
        assert_eq!(verdict_of(&failed).0, Classification::System);
        let mut fault = evidence(&READ_F186, End::LinkError(LinkErrorKind::VciHardwareFault));
        fault.answers = vec![an_answer()];
        assert_eq!(verdict_of(&fault).0, Classification::System);
        let mut refused = evidence(&READ_F186, End::NotSent(RefusalCause::Policy));
        refused.answers = vec![an_answer()];
        refused.timing.window = Window::TimeoutEarly;
        assert_eq!(verdict_of(&refused).0, Classification::System);
        let mut lost = evidence(
            &READ_F186,
            End::VciLost {
                refused_after_loss: false,
            },
        );
        lost.timing.window = Window::Late;
        let (class, reason) = verdict_of(&lost);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("lost"), "{reason}");
        // The rows after it still take the last row: a window `timeout_late` on a response.
        let mut late = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86]));
        late.timing.window = Window::TimeoutLate;
        let (class, reason) = verdict_of(&late);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("could not decide"), "{reason}");
    }

    #[test]
    fn an_echoed_sub_function_is_matched_without_its_suppress_bit() {
        let mut state = LinkState::default();
        let mut owed = evidence(&[0x3E, 0x80], End::NoResponse);
        owed.suppressed = true;
        log(&mut state, 1, owed);
        let answer = evidence(&[0x3E, 0x00], End::Positive(vec![0x7E, 0x00]));
        assert_eq!(state.history_for(&answer).owed_matching, 1);
        assert_eq!(Expected::uds(&[0x3E, 0x80]).positive_prefix, [0x7E, 0x00]);
        assert_eq!(
            Expected::uds(&[0x22, 0xF1, 0x86]).positive_prefix,
            [0x62, 0xF1, 0x86]
        );
        assert_eq!(
            Expected::uds(&[0x19, 0x82, 5]).positive_prefix,
            [0x59, 0x02]
        );
        assert_eq!(Expected::uds(&[0x27, 0x81]).positive_prefix, [0x67, 0x01]);
        assert_eq!(Expected::uds(&[0x31, 0x01, 0xFF]).positive_prefix, [0x71]);
        assert_eq!(Expected::uds(&[0x31]).negative_pattern, [0x7F, 0x31]);
    }

    #[test]
    fn a_flow_control_frame_is_partial_only_when_the_ecu_gave_no_response() {
        let mut silent = evidence(&READ_F186, End::NoResponse);
        silent.flow_control = true;
        assert_eq!(
            sent_of(&silent, &LinkHistory::empty()),
            (Sent::Partial, SentEvidence::FlowControl)
        );
        // An answer no owed request could own confirms the send.
        let mut answered = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86]));
        answered.flow_control = true;
        let history = LinkState::default().history_for(&answered);
        assert_eq!(
            sent_of(&answered, &history),
            (Sent::Confirmed, SentEvidence::Answer)
        );
        // One an owed request could own leaves the send unknown, not partial.
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x86], End::NoResponse));
        let history = state.history_for(&answered);
        assert_eq!(history.owed_matching, 1);
        assert_eq!(
            sent_of(&answered, &history),
            (Sent::Unknown, SentEvidence::None)
        );
        // So does a pending, and an answers list.
        let mut pending_seen = evidence(&READ_F186, End::NoResponse);
        pending_seen.flow_control = true;
        pending_seen.pending = vec![pending()];
        assert_eq!(sent_of(&pending_seen, &history).0, Sent::Unknown);
        let mut listed = evidence(&READ_F186, End::NoResponse);
        listed.flow_control = true;
        listed.answers = vec![an_answer()];
        assert_eq!(sent_of(&listed, &LinkHistory::empty()).0, Sent::Unknown);
    }

    #[test]
    fn a_pending_an_owed_request_could_own_does_not_say_the_request_was_sent() {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x86], End::NoResponse));
        let mut stalled = forwarding(read([0xF1, 0x86], End::NoResponse));
        stalled.pending = vec![pending()];
        let entry = log(&mut state, 2, stalled.clone());
        assert_eq!(entry.sent, Sent::Unknown);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(
            !entry.reason.contains("Sent on the bus"),
            "{}",
            entry.reason
        );
        assert!(!entry.text.contains("Sent on the bus"), "{}", entry.text);
        assert!(
            entry.reason.contains("not known whether"),
            "{}",
            entry.reason
        );
        // With a link error as well, the undetermined row words the send the same way.
        stalled.link_errors = vec![LinkErrorKind::Receive];
        let entry = log(&mut state, 3, stalled);
        assert!(
            !entry.reason.contains("Sent on the bus"),
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("not known whether"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn a_response_in_the_entry_passes_through_redaction_by_its_direction() {
        // A seed response shows its length only; a key request, the same.
        let seed = entry_json(evidence(
            &[0x27, 0x01],
            End::Positive(vec![0x67, 0x01, 0xDE, 0xAD, 0xBE, 0xEF]),
        ));
        assert_eq!(
            seed["outcome"]["response"],
            json!({ "clear": "6701", "data": { "kind": "secret", "len": 4, "all_zero": false } })
        );
        assert_eq!(seed["request"]["bytes"], json!({ "clear": "2701" }));
        assert!(!seed.to_string().contains("deadbeef"));
        let key = entry_json(forwarding(evidence(
            &[0x27, 0x02, 1, 2, 3, 4, 5],
            End::Negative(vec![0x7F, 0x27, 0x35]),
        )));
        assert_eq!(
            key["request"]["bytes"],
            json!({ "clear": "2702", "data": { "kind": "secret", "len": 5 } })
        );
        assert!(!key.to_string().contains("0102030405"));
        // A negative response stays whole, and the reason carries its bytes.
        assert_eq!(key["outcome"]["response"], json!({ "clear": "7f2735" }));
        assert!(key["reason"].as_str().unwrap().contains("7f2735"));
    }

    #[test]
    fn the_detail_holds_names_and_the_host_error_is_a_variant_name() {
        let mut with_detail = evidence(&READ_F186, End::NoResponse);
        with_detail.detail = Detail {
            host_error: Some("NoResponse"),
            error_event: Some("PDU_ERR_EVT_RX_TIMEOUT".to_owned()),
            extra_error_info: Some(0x101),
            extra_error_info_name: Some("RX_TIMEOUT_NO_RSP".to_owned()),
            ..Detail::default()
        };
        let json = entry_json(with_detail);
        assert_eq!(
            json["detail"],
            json!({
                "host_error": "NoResponse",
                "error_event": "PDU_ERR_EVT_RX_TIMEOUT",
                "extra_error_info": 257,
                "extra_error_info_name": "RX_TIMEOUT_NO_RSP",
            })
        );
    }

    #[test]
    fn the_causes_are_named_in_the_designs_words() {
        let cause = |end: End| entry_json(evidence(&READ_F186, end))["outcome"]["cause"].clone();
        assert_eq!(cause(End::Unknown(UnknownCause::RpcFailure)), "rpc_failure");
        assert_eq!(
            cause(End::Unknown(UnknownCause::WorkerNotAnswering)),
            "worker_not_answering"
        );
        assert_eq!(
            cause(End::Unknown(UnknownCause::EventsEnded)),
            "events_ended"
        );
        assert_eq!(cause(End::Unknown(UnknownCause::EventsLost)), "events_lost");
        assert_eq!(
            cause(End::NotSent(RefusalCause::UndeclaredService)),
            "undeclared_service"
        );
        // No reason says "link service", which is the wording of an internal component.
        for cause in [
            UnknownCause::RpcFailure,
            UnknownCause::WorkerNotAnswering,
            UnknownCause::EventsEnded,
            UnknownCause::EventsLost,
            UnknownCause::PrimitiveFailedToStart,
        ] {
            assert!(!cause.phrase().contains("link service"), "{cause:?}");
        }
    }

    #[test]
    fn an_accepted_reset_of_any_type_or_session_change_ends_the_evidence() {
        // Every accepted ECUReset ends the matched answers and the owed requests (ADR-346
        // item 6), with or without the suppress bit.
        for sub in [0x01, 0x02, 0x03, 0x04, 0x05, 0x40, 0x81] {
            let state = link_after(&[0x11, sub], End::Positive(vec![0x51, sub & 0x7F]));
            assert_eq!(counts(&state), (0, 0), "{sub:#04x}");
            assert_eq!(state.last_reset(), Some(3), "{sub:#04x}");
        }
        let state = link_after(&[0x10, 0x02], End::Positive(vec![0x50, 0x02]));
        assert_eq!(counts(&state), (0, 0));
        // A refused reset ends nothing.
        let state = link_after(&[0x11, 0x01], End::Negative(vec![0x7F, 0x11, 0x22]));
        assert_eq!(counts(&state), (1, 1));
    }

    #[test]
    fn an_unusable_answer_to_a_restart_ends_the_matched_answers_but_no_owed_request() {
        let state = link_after(&[0x10, 0x03], End::PositiveUnusable(vec![0x50]));
        assert_eq!(counts(&state), (0, 1));
        assert_eq!(state.last_reset(), None);
        let state = link_after(&[0x11, 0x01], End::PositiveUnusable(vec![0x51]));
        assert_eq!(counts(&state), (0, 1));
        // Another service's unusable answer ends nothing.
        let state = link_after(&[0x31, 0x01], End::PositiveUnusable(vec![0x71]));
        assert_eq!(counts(&state), (1, 1));
    }

    #[test]
    fn the_names_are_single_words_and_the_service_table_leaves_reserved_identifiers_unnamed() {
        assert_eq!(
            nrc_name(0x78),
            Some("requestCorrectlyReceivedResponsePending")
        );
        assert_eq!(nrc_name(0x8A), Some("throttlePedalTooHigh"));
        assert_eq!(nrc_name(0x8B), Some("throttlePedalTooLow"));
        assert_eq!(nrc_name(0x8F), Some("brakeSwitchesNotClosed"));
        assert_eq!(nrc_name(0x94), Some("ResourceTemporarilyNotAvailable"));
        // 0x83 is reserved in the service identifier table; its neighbours are services.
        assert_eq!(service_name(0x83), None);
        for sid in [0x84, 0x85, 0x86, 0x87] {
            assert!(service_name(sid).is_some(), "{sid:#04x}");
        }
        assert_eq!(service_name(0x38), Some("RequestFileTransfer"));
        assert_eq!(service_name(0x3D), Some("WriteMemoryByAddress"));
    }

    #[test]
    fn the_negative_reasons_carry_the_response_bytes_and_the_notes_of_the_table() {
        let reason = |request: &[u8], code: u8| {
            let evidence = forwarding(evidence(
                request,
                End::Negative(vec![0x7F, request[0], code]),
            ));
            verdict_of(&evidence).1
        };
        let key = reason(&[0x27, 0x02, 1, 2], 0x35);
        assert!(key.contains("7f2735"), "{key}");
        assert!(key.contains("invalidKey (0x35)"), "{key}");
        assert!(key.contains("key the system computed"), "{key}");
        assert!(key.contains("not told apart"), "{key}");
        assert!(key.contains("zero seed"), "{key}");
        for code in [0x36, 0x37] {
            let attempts = reason(&[0x27, 0x01], code);
            assert!(
                attempts.contains("earlier job or another tester"),
                "{attempts}"
            );
            assert!(attempts.contains(&format!("7f27{code:02x}")), "{attempts}");
        }
        let state = reason(&READ_F186, 0x22);
        assert!(state.contains("7f2222"), "{state}");
        let other = reason(&READ_F186, 0x31);
        assert!(
            !other.contains("zero seed") && !other.contains("earlier job"),
            "{other}"
        );
    }

    mod erased {
        use serde::Serialize;

        pub(super) trait Ser {
            fn to_value(&self) -> serde_json::Value;
        }

        impl<T: Serialize> Ser for T {
            fn to_value(&self) -> serde_json::Value {
                serde_json::to_value(self).unwrap()
            }
        }
    }

    // ----- The answers rows (design 8.11 "Exchange entry" `answers`, ADR-400)

    const PRESENCE: [u8; 2] = [0x3E, 0x00];
    const POSITIVE: [u8; 2] = [0x7E, 0x00];

    /// An answer on `rx_id`, judged `window`, `ms` after the transmit.
    fn answer(rx_id: u32, end: End, window: Window, ms: f64) -> Answer {
        let mut timing = Timing::not_judged();
        timing.window = window;
        timing.response_ms = Some(ms);
        timing.not_judged_reason =
            (window == Window::NotJudged).then_some(NotJudgedReason::NoMargin);
        Answer {
            rx_id,
            end,
            vci_us: Some(1_000),
            clock: Some(Clock::Device),
            epoch: Some(0),
            start_us: None,
            tx_done_us: None,
            agent_us: 2_000,
            pending: Vec::new(),
            timing,
            history: None,
        }
    }

    fn positive(rx_id: u32, window: Window) -> Answer {
        answer(rx_id, End::Positive(POSITIVE.to_vec()), window, 5.0)
    }

    /// A collecting presence exchange on a link that forwards pendings, whose window the VCI
    /// ended at its limit, with a confirmed send.
    fn collecting(answers: Vec<Answer>) -> Evidence {
        let end = answers
            .first()
            .map_or(End::NoResponse, |first| first.end.clone());
        let mut evidence = forwarding(evidence(&PRESENCE, end));
        evidence.collecting = true;
        evidence.vci_ended = true;
        evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
        evidence.timing.window = Window::TimeoutAtLimit;
        evidence.timing.not_judged_reason = None;
        evidence.answers = answers;
        evidence
    }

    /// A collecting exchange the agent's deadline ended before the VCI ended its window, its
    /// VCI limits not shown past (ADR-434).
    fn deadline_ended(answers: Vec<Answer>) -> Evidence {
        let mut evidence = collecting(answers);
        evidence.end = End::VciDidNotEnd;
        evidence.vci_ended = false;
        evidence.timing.window = Window::NotJudged;
        evidence.timing.not_judged_reason = Some(NotJudgedReason::NoTimeToJudge);
        evidence
    }

    /// A final answer on `rx_id` whose own response pending came after the limit in force.
    fn late_pending_answer(rx_id: u32) -> Answer {
        let mut late = positive(rx_id, Window::Late);
        late.pending = vec![pending()];
        late.timing.window_by_ms = Some(3.0);
        late.timing.late_pending = Some(0);
        late
    }

    fn entry_of(evidence: Evidence) -> ExchangeEntry {
        let history = LinkState::default().history_for(&evidence);
        ExchangeEntry::build(1, evidence, history)
    }

    fn classes(entry: &ExchangeEntry) -> Vec<Classification> {
        entry
            .answers
            .iter()
            .map(|item| item.classification)
            .collect()
    }

    #[test]
    fn a_collecting_answer_within_p2_at_a_window_judged_at_its_limit_is_none() {
        // Case 1.
        let entry = entry_of(collecting(vec![positive(RX, Window::WithinP2)]));
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(
            entry.classification,
            Classification::None,
            "{}",
            entry.reason
        );
        assert_eq!(entry.answers[0].response_ms, Some(5.0));
        assert!(
            entry.reason.contains("1 answer on 1 identifier"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn a_collecting_window_not_judged_at_its_limit_leaves_the_entry_undetermined() {
        // Case 2, as on the J2534 worker, which ends an answered window with no timeout item.
        let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
        evidence.timing.window = Window::NotJudged;
        evidence.timing.not_judged_reason = Some(NotJudgedReason::NoTimeToJudge);
        let entry = entry_of(evidence);
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(
            entry.reason.contains("not judged against the limit"),
            "{}",
            entry.reason
        );
        // Case 27: the not-judged upgrade of a final response is not applied to the entry.
        assert!(
            !entry.reason.contains("Its time is not judged"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn two_answers_on_one_identifier_are_each_judged_and_named() {
        // Case 3.
        let mut second = positive(RX, Window::WithinP2);
        second.timing.response_ms = Some(1_020.0);
        let entry = entry_of(collecting(vec![positive(RX, Window::WithinP2), second]));
        assert_eq!(classes(&entry), [Classification::None; 2]);
        assert_eq!(entry.classification, Classification::None);
        assert!(
            entry.reason.contains("2 answers on 1 identifier"),
            "{}",
            entry.reason
        );
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["answers"][1]["response_ms"], 1_020.0);
        assert_eq!(json["answers"][1]["clock"], "device");
        assert_eq!(json["answers"][1]["epoch"], 0);
    }

    #[test]
    fn a_late_second_answer_makes_the_entry_system() {
        // Case 4.
        let mut late = positive(RX, Window::Late);
        late.timing.window_by_ms = Some(3.0);
        let entry = entry_of(collecting(vec![positive(RX, Window::WithinP2), late]));
        assert_eq!(
            classes(&entry),
            [Classification::None, Classification::System]
        );
        assert_eq!(entry.classification, Classification::System);
        assert!(entry.reason.contains("#2 on 0x7e8"), "{}", entry.reason);
    }

    #[test]
    fn a_negative_answer_is_classified_as_a_single_exchange_would_be() {
        // Case 5.
        let negative = End::Negative(vec![0x7F, 0x3E, 0x22]);
        let alone = judged(forwarding(evidence(&PRESENCE, negative.clone())));
        let (class, _) = verdict_of(&alone);
        let entry = entry_of(collecting(vec![answer(
            RX,
            negative,
            Window::WithinP2,
            5.0,
        )]));
        assert_eq!(classes(&entry), [class]);
        assert_eq!(entry.classification, class);
        assert_eq!(class, Classification::Vehicle);
    }

    #[test]
    fn a_pending_before_its_final_answer_restarts_the_window_and_is_none() {
        // Case 6.
        let mut one = positive(RX, Window::WithinP2Star);
        one.pending = vec![pending()];
        let entry = entry_of(collecting(vec![one]));
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(entry.classification, Classification::None);
    }

    #[test]
    fn a_lone_pending_answer_takes_the_pending_rows() {
        // Case 7.
        let lone = answer(
            RX,
            End::Negative(vec![0x7F, 0x3E, 0x78]),
            Window::WithinP2,
            5.0,
        );
        let as_exchange = collecting(vec![lone.clone()]).as_answer(&lone);
        assert!(matches!(as_exchange.end, End::NoResponse));
        assert_eq!(as_exchange.pending.len(), 1);
        let entry = entry_of(collecting(vec![lone]));
        assert_eq!(classes(&entry), [Classification::Undetermined]);
        assert_eq!(entry.classification, Classification::Undetermined);
    }

    #[test]
    fn a_silent_collecting_window_is_none_only_when_everything_is_shown() {
        // Case 10.
        let entry = entry_of(collecting(Vec::new()));
        assert_eq!(
            entry.classification,
            Classification::None,
            "{}",
            entry.reason
        );
        assert!(entry.answers.is_empty());
        // Case 11: no evidence the request reached the bus.
        let mut unknown = collecting(Vec::new());
        unknown.transmit_evidence = None;
        let entry = entry_of(unknown);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("(H1)"), "{}", entry.reason);
        // Not judged at the limit, or a link that absorbs pendings: never `vehicle` or `none`.
        let mut open = collecting(Vec::new());
        open.timing.window = Window::NotJudged;
        assert_eq!(entry_of(open).classification, Classification::Undetermined);
        let mut absorbing = collecting(Vec::new());
        absorbing.pending_visibility = PendingVisibility::AbsorbedByVci;
        assert_eq!(
            entry_of(absorbing).classification,
            Classification::Undetermined
        );
        // A link error of the silent window.
        let mut error = collecting(Vec::new());
        error.channel_link_errors = vec![LinkErrorKind::Frame];
        assert_eq!(entry_of(error).classification, Classification::Undetermined);
    }

    #[test]
    fn an_own_link_error_names_itself_beside_the_answers() {
        // Case 12.
        let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
        evidence.link_errors = vec![LinkErrorKind::VciHardwareFault];
        let entry = entry_of(evidence);
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(entry.classification, Classification::System);
        assert!(
            entry.reason.contains("1 answer on 1 identifier"),
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("A link error also came"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn another_links_error_leaves_the_entry_undetermined() {
        // Case 13.
        let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
        evidence.channel_link_errors = vec![LinkErrorKind::Frame];
        let entry = entry_of(evidence);
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("another link"), "{}", entry.reason);
    }

    #[test]
    fn a_window_a_receive_error_ended_is_decided_by_the_link_error_row_and_lists_its_answers() {
        // Case 14.
        let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
        evidence.end = End::LinkError(LinkErrorKind::VciHardwareFault);
        let entry = entry_of(evidence);
        assert_eq!(entry.classification, Classification::System);
        assert_eq!(classes(&entry), [Classification::None]);
    }

    #[test]
    fn an_absorbing_link_leaves_the_entry_undetermined_and_judges_the_answers_absorbing() {
        // Case 15.
        let mut evidence = collecting(vec![
            positive(RX, Window::WithinP2),
            positive(0x7E9, Window::PossiblePending),
        ]);
        evidence.pending_visibility = PendingVisibility::AbsorbedByVci;
        let entry = entry_of(evidence);
        assert_eq!(
            classes(&entry),
            [Classification::None, Classification::Undetermined]
        );
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(
            entry.reason.contains("2 answers on 2 identifiers"),
            "{}",
            entry.reason
        );
        assert!(entry.reason.contains("(H4)"), "{}", entry.reason);
    }

    #[test]
    fn the_rows_before_the_answers_decide_and_the_answers_are_still_listed() {
        // Cases 16 and 17.
        for (end, class) in [
            (End::VciDidNotEnd, Classification::Undetermined),
            (
                End::Unknown(UnknownCause::EventsLost),
                Classification::System,
            ),
            (
                End::VciLost {
                    refused_after_loss: false,
                },
                Classification::Undetermined,
            ),
        ] {
            let mut evidence = collecting(vec![
                positive(RX, Window::WithinP2),
                positive(0x7E9, Window::WithinP2),
            ]);
            evidence.end = end;
            let entry = entry_of(evidence);
            assert_eq!(entry.classification, class, "{}", entry.reason);
            assert_eq!(entry.answers.len(), 2);
        }
    }

    #[test]
    fn a_deadline_ended_collecting_entry_with_in_time_answers_is_undetermined_naming_the_deadline()
    {
        // ADR-434 case 1: the answers weigh none, the deadline stays open.
        let entry = entry_of(deadline_ended(vec![positive(RX, Window::WithinP2)]));
        assert_eq!(classes(&entry), [Classification::None]);
        assert_eq!(
            entry.classification,
            Classification::Undetermined,
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("1 answer on 1 identifier"),
            "{}",
            entry.reason
        );
        assert!(
            entry
                .reason
                .contains("whether the VCI would have ended it at its limit is not shown"),
            "{}",
            entry.reason
        );
        assert!(!entry.reason.contains("on one clock"), "{}", entry.reason);
    }

    #[test]
    fn a_deadline_ended_collecting_entry_with_a_late_pending_answer_is_system() {
        // ADR-434 case 2: the answer's own late pending is the system's.
        let entry = entry_of(deadline_ended(vec![late_pending_answer(RX)]));
        assert_eq!(classes(&entry), [Classification::System]);
        assert_eq!(
            entry.classification,
            Classification::System,
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("The answer that set it: #1"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn a_deadline_ended_functional_window_names_the_answers_on_both_identifiers() {
        // ADR-434 case 3, on a functional link receiving on 0x7E8 and 0x7E9: a final answer and
        // a lone pending.
        let mut evidence = deadline_ended(vec![positive(RX, Window::WithinP2), lone(RX2)]);
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        let entry = entry_of(evidence);
        assert_eq!(
            entry.classification,
            Classification::Undetermined,
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("2 answers on 2 identifiers"),
            "{}",
            entry.reason
        );
        // A late pending on 0x7E9 makes it the system's, as on a physical link.
        let mut evidence = deadline_ended(vec![
            positive(RX, Window::WithinP2),
            late_pending_answer(RX2),
        ]);
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        let entry = entry_of(evidence);
        assert_eq!(
            entry.classification,
            Classification::System,
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("The answer that set it: #2 on 0x7e9"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn a_deadline_ended_collecting_entry_with_a_channel_link_error_names_h3() {
        // ADR-434 case 8.
        let mut evidence = deadline_ended(vec![positive(RX, Window::WithinP2)]);
        evidence.channel_link_errors = vec![LinkErrorKind::Frame];
        let entry = entry_of(evidence);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(entry.reason.contains("(H3)"), "{}", entry.reason);
    }

    #[test]
    fn a_collecting_window_the_vci_ended_early_or_late_is_system() {
        // Case 18.
        for window in [Window::TimeoutEarly, Window::TimeoutLate] {
            let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
            evidence.timing.window = window;
            evidence.timing.window_by_ms = Some(4.0);
            let entry = entry_of(evidence);
            assert_eq!(
                entry.classification,
                Classification::System,
                "{}",
                entry.reason
            );
            assert_eq!(classes(&entry), [Classification::None]);
        }
    }

    #[test]
    fn answers_whose_time_is_not_judged_are_undetermined() {
        // Cases 19 and 20.
        let entry = entry_of(collecting(vec![
            positive(RX, Window::NotJudged),
            positive(0x7E9, Window::NotJudged),
        ]));
        assert_eq!(classes(&entry), [Classification::Undetermined; 2]);
        assert_eq!(entry.classification, Classification::Undetermined);
    }

    #[test]
    fn a_second_responder_to_a_request_meant_for_one_ecu_is_the_vehicle() {
        // Case 22: the entry's outcome stays the first answer's, and both answers are listed.
        let mut evidence = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
        )));
        evidence.answers = vec![
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 1]),
                Window::WithinP2,
                5.0,
            ),
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 2]),
                Window::WithinP2,
                9.0,
            ),
        ];
        let entry = entry_of(evidence);
        assert_eq!(
            entry.classification,
            Classification::Vehicle,
            "{}",
            entry.reason
        );
        assert!(
            entry.reason.contains("second responder"),
            "{}",
            entry.reason
        );
        assert_eq!(entry.answers.len(), 2);
        assert!(matches!(entry.outcome, Outcome::Positive { .. }));
    }

    #[test]
    fn extra_answers_no_more_than_the_owed_requests_are_undetermined() {
        // Case 23.
        let mut evidence = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
        )));
        evidence.answers = vec![
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 1]),
                Window::WithinP2,
                5.0,
            ),
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 2]),
                Window::WithinP2,
                9.0,
            ),
        ];
        let mut history = LinkHistory::empty();
        history.owed_matching = 1;
        history.owners = vec![1];
        let (class, reason) = several_answers_row(&evidence, &history);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("earlier request"), "{reason}");
        // Two extras against one owed request show a second responder.
        evidence.answers.push(evidence.answers[1].clone());
        assert_eq!(
            several_answers_row(&evidence, &history).0,
            Classification::Vehicle
        );
    }

    #[test]
    fn the_worst_answer_of_a_request_meant_for_one_ecu_decides_when_it_is_worse() {
        // Case 24.
        let mut evidence = judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
        )));
        evidence.answers = vec![
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 1]),
                Window::WithinP2,
                5.0,
            ),
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 2]),
                Window::Late,
                90.0,
            ),
        ];
        let entry = entry_of(evidence);
        assert_eq!(
            entry.classification,
            Classification::System,
            "{}",
            entry.reason
        );
        let mut open = judged(forwarding(evidence_with_two_unjudged()));
        open.answers[1].timing.window = Window::NotJudged;
        assert_eq!(entry_of(open).classification, Classification::Undetermined);
    }

    fn evidence_with_two_unjudged() -> Evidence {
        let mut evidence = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86, 1]));
        evidence.answers = vec![
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 1]),
                Window::WithinP2,
                5.0,
            ),
            answer(
                RX,
                End::Positive(vec![0x62, 0xF1, 0x86, 2]),
                Window::WithinP2,
                9.0,
            ),
        ];
        evidence
    }

    #[test]
    fn a_single_answer_of_an_ordinary_request_is_unchanged() {
        // Case 25.
        let entry = entry_of(judged(forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
        ))));
        assert!(entry.answers.is_empty());
        assert_eq!(entry.classification, Classification::None);
    }

    #[test]
    fn as_answer_keeps_what_belongs_to_one_answer_only() {
        let mut evidence = collecting(vec![positive(RX, Window::WithinP2)]);
        evidence.link_errors = vec![LinkErrorKind::Frame];
        evidence.channel_link_errors = vec![LinkErrorKind::Frame];
        evidence.end_rejected = true;
        let one = evidence.as_answer(&evidence.answers[0]);
        assert!(!one.collecting);
        assert!(one.answers.is_empty() && one.link_errors.is_empty());
        assert!(one.channel_link_errors.is_empty() && !one.end_rejected);
        assert_eq!(one.timing, evidence.answers[0].timing);
        assert!(matches!(one.end, End::Positive(_)));
    }

    #[test]
    fn a_late_response_settling_a_silent_collecting_exchange_takes_the_ordinary_rows() {
        // Case 29: `Kept::answered` on a collecting exchange with no answers restates the response
        // as its one answer, judged by the collecting row.
        let mut state = LinkState::default();
        let (_, kept) = log_kept(&mut state, 1, collecting(Vec::new()));
        let answered = kept.answered(&POSITIVE, &LateAnswer::default());
        assert!(answered.collecting && answered.answers.len() == 1);
        let (class, reason) = verdict_of(&answered);
        assert!(!reason.contains("No answer came"), "{reason}");
        assert_eq!(class, Classification::None, "{reason}");
    }

    const RX2: u32 = 0x7E9;

    /// A pending on `rx_id` the window did not follow with a final answer.
    fn lone(rx_id: u32) -> Answer {
        answer(
            rx_id,
            End::Negative(vec![0x7F, 0x3E, 0x78]),
            Window::WithinP2,
            5.0,
        )
    }

    /// The bytes of a wrong echo of the presence request (not `7E 00`).
    const WRONG_ECHO: [u8; 2] = [0x7E, 0x01];

    /// A result on `rx_id` the worker took and the agent cannot use (a wrong echo), as the host
    /// judges it (`positive_unusable`).
    fn rejected(rx_id: u32) -> Answer {
        answer(
            rx_id,
            End::PositiveUnusable(WRONG_ECHO.to_vec()),
            Window::WithinP2,
            5.0,
        )
    }

    /// `evidence` with the end and `end_rejected` the host derives from its answers.
    fn ended(mut evidence: Evidence) -> Evidence {
        (evidence.end, evidence.end_rejected) =
            collecting_end(&evidence.answers, &evidence.expected);
        evidence
    }

    /// The kept collecting exchange with `answers` (its end the first final answer's); on a
    /// functional link (`functional`) its own identifier is 0 and it receives on 0x7E8, 0x7E9.
    fn kept_collecting(answers: Vec<Answer>, functional: bool) -> Kept {
        kept_on(answers, if functional { &[RX, RX2] } else { &[] })
    }

    /// [`kept_collecting`] on a link that receives on `table` (empty: a physical link).
    fn kept_on(answers: Vec<Answer>, table: &[u32]) -> Kept {
        let mut evidence = collecting(answers);
        (evidence.end, evidence.end_rejected) =
            collecting_end(&evidence.answers, &evidence.expected);
        if !table.is_empty() {
            evidence.rx_id = 0;
            evidence.receive_ids = table.to_vec();
        }
        log_kept(&mut LinkState::default(), 1, evidence).1
    }

    fn late_on(rx_id: Option<u32>) -> LateAnswer {
        LateAnswer {
            rx_id,
            vci_us: Some(7_000),
            clock: Some(Clock::Device),
            epoch: Some(0),
            agent_us: 9_000,
        }
    }

    /// The identifier and end of each answer.
    fn shape(evidence: &Evidence) -> Vec<(u32, Option<Vec<u8>>)> {
        evidence
            .answers
            .iter()
            .map(|answer| (answer.rx_id, answer.end.response().map(<[u8]>::to_vec)))
            .collect()
    }

    /// Row A (ADR-425 decision 4, ADR-429): a final answer to a lone pending folds it, so no
    /// identifier is left open; the folded list is what the host's rule gives.
    #[test]
    fn a_late_final_answer_folds_the_lone_pending_of_its_identifier() {
        let kept = kept_collecting(vec![lone(RX)], false);
        assert_eq!(kept.evidence.open_pending_ids(), [RX]);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert!(answered.open_pending_ids().is_empty());
        assert_eq!(answered.answers.len(), 1);
        let last = answered.answers.last().unwrap();
        assert_eq!(last.pending.len(), 1);
        assert_eq!((last.rx_id, last.agent_us), (RX, 9_000));
        assert_eq!(last.timing.window, Window::WithinP2);
        assert_eq!(answered.end.response(), Some(&POSITIVE[..]));
        assert!(answered.settles_owed());
        // The host's rule over the answers and the final gives the same list.
        let mut items = kept.evidence.answers.clone();
        items.push(answered.answers[0].clone());
        let expected = &kept.evidence.expected;
        let standing = standing_answers(
            &items,
            |a| a.rx_id,
            |a| a.is_pending(expected),
            Answer::is_rejected,
        );
        let rule = super::super::keep_standing(items, &standing);
        assert_eq!(
            rule.iter()
                .map(|a| (a.rx_id, a.end.response().map(<[u8]>::to_vec)))
                .collect::<Vec<_>>(),
            shape(&answered)
        );
    }

    /// Rows A, B and C: how the settlement judges the folded exchange by the response's window.
    #[test]
    fn a_folded_lone_pending_no_longer_hides_where_the_vci_ended_its_wait() {
        use super::super::timing::LateWindow;
        let kept = kept_collecting(vec![lone(RX)], false);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        let settle = |window| {
            settle_late_response(
                &kept,
                &answered,
                window,
                &LinkHistory::empty(),
                Some(RX),
                &POSITIVE,
            )
        };
        // A.
        let (class, _) = settle(LateWindow::InTime { at_us: 1_000 });
        assert_eq!(class, Classification::System);
        // B: the VCI ended its wait at the limit; the pending no longer makes it undetermined.
        let (class, reason) = settle(LateWindow::Late { by_us: 2_000 });
        assert_eq!(class, Classification::Vehicle, "{reason}");
        assert!(reason.contains("VCI had ended its wait"), "{reason}");
        // C: the reason names the final answer, not a pending.
        let (class, reason) = settle(LateWindow::Open);
        assert_eq!(class, Classification::Undetermined, "{reason}");
        assert!(!reason.contains("pending"), "{reason}");
    }

    /// Row D: the other identifier's lone pending stays open, so the exchange stays owed on it
    /// alone (ADR-433): its rows and the places of its answers follow the answers it has now.
    #[test]
    fn a_late_final_answer_leaves_another_identifiers_lone_pending() {
        let kept = kept_collecting(vec![lone(RX), lone(RX2)], true);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert_eq!(answered.open_pending_ids(), [RX2]);
        assert_eq!(answered.answers.len(), 2);
        assert_eq!(answered.answers[1].pending.len(), 1);
        assert_eq!(answered.end.response(), Some(&POSITIVE[..]));
        assert!(!answered.settles_owed());
        assert_eq!(answered.owed_ids(), [RX2]);
        let (class, _) = verdict_of(&answered);
        assert_eq!(class, Classification::Undetermined);
        // The places of the folded answers: the pending of RX left with its place, the final
        // answer came last.
        let (placed, order, ends) = kept.answered_placed(&POSITIVE, &late_on(Some(RX)), None);
        assert_eq!(shape(&placed), shape(&answered));
        assert_eq!((order.len(), ends.len()), (2, 2));
        assert_eq!(order[0], usize::MAX);
        assert_ne!(order[1], usize::MAX);
    }

    /// ADR-433, row 13: the folded list keeps the places of the answers that stand: the pending
    /// the final answer folds leaves with its place, and the final answer comes after every item.
    #[test]
    fn the_places_follow_the_folded_answers() {
        let mut kept = kept_collecting(vec![lone(RX), lone(RX2)], true);
        kept.pending_order = vec![0, 1];
        kept.answer_order = vec![2, 3];
        kept.answer_ends = vec![None, None];
        let end = Some((
            Clock::Device,
            super::super::clocks::Placed {
                epoch: 0,
                unwrapped: 77,
            },
        ));
        let (answered, order, ends) = kept.answered_placed(&POSITIVE, &late_on(Some(RX)), end);
        assert_eq!(shape(&answered)[0].0, RX2);
        assert_eq!(order, [3, 4]);
        assert_eq!(ends, [None, end]);
    }

    /// ADR-433: settling an owed exchange on one identifier sets its rows to the identifiers
    /// still open and frees the later answers on the settled one; the other identifier's later
    /// answers keep it as an owner.
    #[test]
    fn settling_on_one_identifier_frees_the_later_answers_on_it_only() {
        let table = |mut evidence: Evidence| {
            evidence.rx_id = 0;
            evidence.receive_ids = vec![RX, RX2];
            evidence
        };
        let mut state = LinkState::default();
        let mut first = table(collecting(vec![lone(RX), lone(RX2)]));
        first.end = End::NoResponse;
        let (_, kept) = log_kept(&mut state, 1, first);
        state.keep(1, kept.clone());
        log_kept(
            &mut state,
            2,
            table(collecting(vec![positive(RX, Window::WithinP2)])),
        );
        log_kept(
            &mut state,
            3,
            table(collecting(vec![positive(RX2, Window::WithinP2)])),
        );
        assert_eq!(state.owed_row_ids(1), [RX, RX2]);
        assert_eq!(window_history(&state, 2).owners, [1]);
        // Entry 2 answered on RX only: it is itself owed on the silent RX2 (ADR-435), so it and
        // entry 1 could both own entry 3's answer there.
        assert_eq!(window_history(&state, 3).owners, [1, 2]);
        let (answered, order, ends) = kept.answered_placed(&POSITIVE, &late_on(Some(RX)), None);
        let history = state.history_settling(&answered, 1);
        let entry = ExchangeEntry::build(1, answered.clone(), history);
        let dropped = state.settle_on(
            1,
            answered,
            entry.sent,
            kept.transmit,
            (order, ends),
            Classification::Undetermined,
        );
        assert_eq!(dropped, [RX]);
        assert_eq!(state.owed_row_ids(1), [RX2]);
        assert!(state.kept(1).is_some());
        assert_eq!(state.kept(1).unwrap().answer_order.len(), 2);
        // The exchange is still owed: its owner entry stays until the rows no longer list it.
        let answer = entry.settled_answer(&kept, false).with_settled_ids(dropped);
        state.reclassify_after(&answer);
        assert!(window_history(&state, 2).owners.is_empty());
        assert_eq!(window_history(&state, 3).owners, [1, 2]);
    }

    /// ADR-441 fixtures: a request on the functional table `RX`, `RX2`.
    fn on_table(mut evidence: Evidence) -> Evidence {
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        evidence
    }

    /// A confirmed non-collecting request on the physical identifier.
    fn confirmed_request(request: &[u8], end: End) -> Evidence {
        let mut evidence = forwarding(evidence(request, end));
        evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
        evidence
    }

    /// Logs `evidence` as line `seq`, as the log does with each answer's owners, and keeps it
    /// while it is owed.
    fn log_and_keep(state: &mut LinkState, seq: u64, evidence: Evidence) {
        let (_, kept) = log_kept_owned(state, seq, evidence);
        state.keep(seq, kept);
    }

    /// `1`: a silent collecting presence request (owner), `2`: a silent session change, `3`: a
    /// collecting request answered on `RX` (its row on `RX` rests on `1`) and silent on `RX2`,
    /// then `fillers` answered reads (`WINDOW` of them take `3` out of the window). Returns the
    /// next line number.
    fn restart_survivor_state(fillers: usize) -> (LinkState, u64) {
        let mut state = LinkState::default();
        log_and_keep(&mut state, 1, on_table(collecting(Vec::new())));
        log_and_keep(
            &mut state,
            2,
            confirmed_request(&[0x10, 0x03], End::NoResponse),
        );
        log_and_keep(
            &mut state,
            3,
            on_table(collecting(vec![positive(RX, Window::WithinP2)])),
        );
        assert_eq!(state.owed_row_ids(3), [RX2, RX]);
        let mut seq = 4;
        for _ in 0..fillers {
            log_kept(
                &mut state,
                seq,
                confirmed_request(
                    &[0x22, 0xF1, 0x88],
                    End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                ),
            );
            seq += 1;
        }
        (state, seq)
    }

    /// ADR-441: an accepted session change takes the rows of the exchanges that survive it
    /// again, in the window and beyond it: the survivor's row on `RX` rested on the dropped
    /// owner `1` alone, so it goes; its record stays.
    #[test]
    fn an_accepted_restart_takes_the_rows_of_a_survivor_again_in_and_beyond_the_window() {
        for fillers in [0, WINDOW] {
            let (mut state, seq) = restart_survivor_state(fillers);
            assert_eq!(
                state.window.iter().any(|logged| logged.seq == 3),
                fillers == 0
            );
            log(
                &mut state,
                seq,
                confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            );
            assert_eq!(state.last_reset(), Some(seq), "{fillers}");
            assert!(state.owed_row_ids(1).is_empty() && state.kept(1).is_none());
            assert_eq!(state.owed_row_ids(3), [RX2], "{fillers}");
            assert!(state.kept(3).is_some(), "{fillers}");
            assert_eq!(state.owed_row_ids(2), [RX], "{fillers}");
        }
    }

    /// ADR-441: the same for a late acceptance, `reset_after`: the survivor's row goes and the
    /// freed answer is no owner of the later exchange's answer.
    #[test]
    fn a_late_accepted_restart_takes_the_rows_of_a_survivor_again_in_and_beyond_the_window() {
        for fillers in [0, WINDOW] {
            let (mut state, mut seq) = restart_survivor_state(fillers);
            log_and_keep(
                &mut state,
                seq,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            let reset = seq;
            seq += 1;
            log_and_keep(
                &mut state,
                seq,
                on_table(collecting(vec![positive(RX, Window::WithinP2)])),
            );
            assert_eq!(window_history(&state, seq).owners, [1, 3], "{fillers}");
            state.reset_after(reset, 2, true);
            assert_eq!(state.owed_row_ids(3), [RX2], "{fillers}");
            assert!(state.kept(3).is_some() && state.kept(1).is_none());
            assert!(window_history(&state, seq).owners.is_empty(), "{fillers}");
        }
    }

    /// ADR-441: a survivor's chain: the row of `4` on `RX` rested on `3`, which rested on the
    /// dropped `1`; `3` is visited first, so `4` finds no owner left either.
    #[test]
    fn a_chain_of_survivors_loses_its_rows_in_one_visit() {
        for fillers in [0, WINDOW] {
            let mut state = LinkState::default();
            log_and_keep(&mut state, 1, on_table(collecting(Vec::new())));
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            for seq in [3, 4] {
                log_and_keep(
                    &mut state,
                    seq,
                    on_table(collecting(vec![positive(RX, Window::WithinP2)])),
                );
            }
            assert_eq!(state.owed_row_ids(4), [RX2, RX]);
            for seq in 5..5 + fillers as u64 {
                log_kept(
                    &mut state,
                    seq,
                    confirmed_request(
                        &[0x22, 0xF1, 0x88],
                        End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                    ),
                );
            }
            let accept = 5 + fillers as u64;
            log(
                &mut state,
                accept,
                confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            );
            assert_eq!(state.owed_row_ids(3), [RX2], "{fillers}");
            assert_eq!(state.owed_row_ids(4), [RX2], "{fillers}");
        }
    }

    /// ADR-441: a survivor whose final answer another owner left (`3`, kept after the session
    /// change) could own too keeps its row on `RX`.
    #[test]
    fn a_survivor_keeps_a_row_an_owner_that_is_left_could_own() {
        for fillers in [0, WINDOW] {
            let mut state = LinkState::default();
            log_and_keep(&mut state, 1, on_table(collecting(Vec::new())));
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            log_and_keep(&mut state, 3, on_table(collecting(Vec::new())));
            log_and_keep(
                &mut state,
                4,
                on_table(collecting(vec![positive(RX, Window::WithinP2)])),
            );
            assert_eq!(state.owed_row_ids(4), [RX2, RX]);
            for seq in 5..5 + fillers as u64 {
                log_kept(
                    &mut state,
                    seq,
                    confirmed_request(
                        &[0x22, 0xF1, 0x88],
                        End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                    ),
                );
            }
            log(
                &mut state,
                5 + fillers as u64,
                confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            );
            assert!(state.kept(1).is_none() && state.kept(3).is_some());
            assert_eq!(state.owed_row_ids(4), [RX2, RX], "{fillers}");
        }
    }

    /// ADR-441: the rows that do not rest on an owner stay: an open pending (ADR-436) and the
    /// whole table of a rejected end (ADR-437).
    #[test]
    fn an_accepted_restart_keeps_the_pending_and_rejected_rows_of_a_survivor() {
        for fillers in [0, WINDOW] {
            let mut state = LinkState::default();
            log_and_keep(&mut state, 1, on_table(collecting(Vec::new())));
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            // A pending open on `RX` beside an owned final answer on `RX2`.
            log_and_keep(
                &mut state,
                3,
                on_table(ended(collecting(vec![
                    lone(RX),
                    positive(RX2, Window::WithinP2),
                ]))),
            );
            log_and_keep(
                &mut state,
                4,
                on_table(ended(collecting(vec![rejected(RX)]))),
            );
            let (pending_rows, rejected_rows) = (state.owed_row_ids(3), state.owed_row_ids(4));
            assert!(pending_rows.contains(&RX) && pending_rows.contains(&RX2));
            assert_eq!(rejected_rows.len(), 2);
            for seq in 5..5 + fillers as u64 {
                log_kept(
                    &mut state,
                    seq,
                    confirmed_request(
                        &[0x22, 0xF1, 0x88],
                        End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                    ),
                );
            }
            log(
                &mut state,
                5 + fillers as u64,
                confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            );
            // The final answer on `RX2` rested on the dropped owner: its row goes, the
            // pending's stays.
            assert_eq!(state.owed_row_ids(3), [RX], "{fillers}");
            assert_eq!(state.owed_row_ids(4), rejected_rows, "{fillers}");
            assert!(state.kept(3).is_some() && state.kept(4).is_some());
        }
    }

    /// ADR-441: a survivor on a physical link whose final answer only a dropped owner could own
    /// is placed alone: released whole, record and row, kept or not (a window exchange with no
    /// kept record is read from the window). A later read of the same request then sees no
    /// owner.
    #[test]
    fn an_accepted_restart_releases_a_survivor_that_only_a_dropped_owner_could_own() {
        for (keep, fillers) in [(true, 0), (false, 0), (true, WINDOW)] {
            let mut state = LinkState::default();
            let answer = End::Positive(vec![0x62, 0xF1, 0x86, 0x41]);
            log_and_keep(
                &mut state,
                1,
                confirmed_request(&READ_F186, End::NoResponse),
            );
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            let mut survivor = confirmed_request(&READ_F186, answer.clone());
            let history = state.history_for(&survivor);
            survivor.set_answer_histories(state.answer_histories(&survivor, &history, 3));
            let (_, kept) = log_kept(&mut state, 3, survivor);
            if keep {
                state.keep(3, kept);
            }
            assert_eq!(state.owed_row_ids(3), [RX]);
            for seq in 4..4 + fillers as u64 {
                log_kept(
                    &mut state,
                    seq,
                    confirmed_request(
                        &[0x22, 0xF1, 0x88],
                        End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                    ),
                );
            }
            let probe = confirmed_request(&READ_F186, answer.clone());
            assert_eq!(state.history_for(&probe).owed_matching, 2);
            log(
                &mut state,
                4 + fillers as u64,
                confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
            );
            let case = (keep, fillers);
            assert!(state.owed_row_ids(3).is_empty(), "{case:?}");
            assert!(state.kept(3).is_none(), "{case:?}");
            assert_eq!(state.history_for(&probe).owed_matching, 0, "{case:?}");
        }
    }

    /// ADR-441: the same for a late acceptance on a survivor of the window that has no kept
    /// record.
    #[test]
    fn a_late_accepted_restart_releases_an_unkept_survivor_of_the_window() {
        let mut state = LinkState::default();
        let answer = End::Positive(vec![0x62, 0xF1, 0x86, 0x41]);
        log_and_keep(
            &mut state,
            1,
            confirmed_request(&READ_F186, End::NoResponse),
        );
        log_and_keep(
            &mut state,
            2,
            confirmed_request(&[0x10, 0x03], End::NoResponse),
        );
        let mut survivor = confirmed_request(&READ_F186, answer.clone());
        let history = state.history_for(&survivor);
        survivor.set_answer_histories(state.answer_histories(&survivor, &history, 3));
        log_kept(&mut state, 3, survivor);
        log_and_keep(
            &mut state,
            4,
            confirmed_request(&[0x10, 0x03], End::NoResponse),
        );
        let probe = confirmed_request(&READ_F186, answer);
        assert_eq!(state.history_for(&probe).owed_matching, 2);
        state.reset_after(4, 2, true);
        assert!(state.owed_row_ids(3).is_empty());
        assert_eq!(state.history_for(&probe).owed_matching, 0);
    }

    /// ADR-441: a survivor whose send was `unknown` (no transmit evidence, an owner that could
    /// own the indication) has it confirmed when the restart leaves it no owner: the count of
    /// unknown sends drops, kept or not, on both paths.
    #[test]
    fn a_survivor_released_by_an_accepted_restart_confirms_its_unknown_send() {
        for (keep, late) in [(true, false), (false, false), (true, true), (false, true)] {
            let mut state = LinkState::default();
            let answer = End::Positive(vec![0x62, 0xF1, 0x86, 0x41]);
            log_and_keep(
                &mut state,
                1,
                confirmed_request(&READ_F186, End::NoResponse),
            );
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            let mut survivor = forwarding(evidence(&READ_F186, answer));
            let history = state.history_for(&survivor);
            survivor.set_answer_histories(state.answer_histories(&survivor, &history, 3));
            let (entry, kept) = log_kept(&mut state, 3, survivor);
            assert_eq!(entry.sent, Sent::Unknown);
            if keep {
                state.keep(3, kept);
            }
            let case = (keep, late);
            assert_eq!(state.unknown_sends, 1, "{case:?}");
            assert!(state.may_own_tx_indication());
            if late {
                log_and_keep(
                    &mut state,
                    4,
                    confirmed_request(&[0x10, 0x03], End::NoResponse),
                );
                state.reset_after(4, 2, true);
            } else {
                log(
                    &mut state,
                    4,
                    confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
                );
            }
            assert!(state.owed_row_ids(3).is_empty(), "{case:?}");
            assert_eq!(state.unknown_sends, 0, "{case:?}");
            assert!(!state.may_own_tx_indication(), "{case:?}");
        }
    }

    /// ADR-441: the pass beyond the window comes before the window's: a survivor beyond the
    /// window loses its row on `RX` first, so a survivor in the window whose final answer on `RX`
    /// the first could own finds no owner left (visited the other way round it would keep the
    /// row).
    #[test]
    fn the_survivors_beyond_the_window_are_visited_before_those_in_it() {
        for late in [false, true] {
            let mut state = LinkState::default();
            log_and_keep(&mut state, 1, on_table(collecting(Vec::new())));
            log_and_keep(
                &mut state,
                2,
                confirmed_request(&[0x10, 0x03], End::NoResponse),
            );
            for seq in [3, 4] {
                log_and_keep(
                    &mut state,
                    seq,
                    on_table(collecting(vec![positive(RX, Window::WithinP2)])),
                );
            }
            assert_eq!(window_history(&state, 4).owners, [1, 3]);
            // One survivor beyond the window, one inside it when the restart comes.
            let fillers = if late { 14 } else { 15 };
            for seq in 5..5 + fillers {
                log_kept(
                    &mut state,
                    seq,
                    confirmed_request(
                        &[0x22, 0xF1, 0x88],
                        End::Positive(vec![0x62, 0xF1, 0x88, 0x41]),
                    ),
                );
            }
            let accept = 5 + fillers;
            if late {
                log_and_keep(
                    &mut state,
                    accept,
                    confirmed_request(&[0x10, 0x03], End::NoResponse),
                );
                assert!(logged_beyond(&state, 3) && !logged_beyond(&state, 4));
                state.reset_after(accept, 2, true);
            } else {
                assert!(logged_beyond(&state, 3) && !logged_beyond(&state, 4));
                log(
                    &mut state,
                    accept,
                    confirmed_request(&[0x10, 0x03], End::Positive(vec![0x50, 0x03])),
                );
            }
            assert_eq!(state.owed_row_ids(3), [RX2], "{late}");
            assert_eq!(state.owed_row_ids(4), [RX2], "{late}");
        }
    }

    /// Row E: the end is the first standing final answer, which another identifier's earlier one
    /// is; whether the late response spans frames is read from its own bytes.
    #[test]
    fn a_late_final_answer_keeps_an_earlier_identifiers_final_as_the_end() {
        use super::super::timing::LateWindow;
        let mut kept = kept_collecting(vec![positive(RX2, Window::WithinP2), lone(RX)], true);
        let long = vec![0x7E; 20];
        let answered = kept.answered(&long, &late_on(Some(RX)));
        assert_eq!(answered.end.response(), Some(&POSITIVE[..]));
        assert_eq!(answered.answers.len(), 2);
        assert!(answered.open_pending_ids().is_empty());
        kept.late_start = Some(LateStart::InTime { at_us: 1_000 });
        kept.late_start_id = Some(RX);
        let (class, reason) = settle_late_response(
            &kept,
            &answered,
            LateWindow::Late { by_us: 1 },
            &LinkHistory::empty(),
            Some(RX),
            &long,
        );
        assert_eq!(class, Classification::System, "{reason}");
        assert!(reason.contains("started"), "{reason}");
    }

    /// Row F: an unnamed final answer folds the pending of the one identifier the exchange is
    /// owed on, keyed on that identifier (ADR-433 decision 6, ADR-435). With a lone pending on
    /// RX and RX2 silent the exchange is owed on both, so the unnamed final answer, which may be
    /// any identifier's, folds nothing and covers none: the exchange stays owed.
    #[test]
    fn an_unnamed_late_final_answer_folds_nothing_with_several_owed() {
        let kept = kept_collecting(vec![lone(RX)], true);
        assert_eq!(kept.evidence.owed_ids(), [RX, RX2]);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(answered.open_pending_ids(), [RX]);
        assert_eq!(answered.answers.len(), 2);
        assert_eq!(answered.answers[1].rx_id, 0);
        assert!(answered.answers[1].pending.is_empty());
        assert_eq!(answered.owed_ids(), [RX, RX2]);
        assert!(!answered.settles_owed());
    }

    /// Row F, one owed identifier: an unnamed final answer is keyed on the one identifier the
    /// exchange is owed on, folds its pending and covers it (ADR-433 decision 6, ADR-435).
    #[test]
    fn an_unnamed_late_final_answer_is_keyed_on_the_one_owed_identifier() {
        let kept = kept_collecting(vec![lone(RX), positive(RX2, Window::WithinP2)], true);
        assert_eq!(kept.evidence.owed_ids(), [RX]);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert!(answered.open_pending_ids().is_empty());
        assert_eq!(answered.answers.len(), 2);
        assert_eq!(answered.answers[1].rx_id, RX);
        assert_eq!(answered.answers[1].pending.len(), 1);
        assert!(answered.owed_ids().is_empty());
        assert!(answered.settles_owed());
    }

    /// Row G: an unnamed final answer with several identifiers open folds nothing.
    #[test]
    fn an_unnamed_late_final_answer_folds_nothing_with_several_open() {
        let kept = kept_collecting(vec![lone(RX), lone(RX2)], true);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(answered.answers.len(), 3);
        assert_eq!(answered.open_pending_ids(), [RX, RX2]);
        assert!(
            answered
                .answers
                .iter()
                .all(|a| a.rx_id != 0 || a.pending.is_empty())
        );
        assert!(!answered.settles_owed());
        assert_eq!(answered.end.response(), Some(&POSITIVE[..]));
    }

    /// An unnamed final answer folds an unnamed lone pending, as the host's fold keys both 0;
    /// on a physical link an unnamed answer is keyed on the link's own identifier.
    #[test]
    fn an_unnamed_late_final_answer_folds_an_unnamed_lone_pending() {
        for table in [&[RX, RX2][..], &[RX][..]] {
            let kept = kept_on(vec![lone(0)], table);
            let answered = kept.answered(&POSITIVE, &late_on(None));
            assert!(answered.open_pending_ids().is_empty(), "{table:?}");
            assert_eq!(answered.answers.len(), 1);
            assert_eq!(answered.answers[0].pending.len(), 1);
            // An unnamed final answer covers no identifier of a larger table (ADR-435).
            assert_eq!(answered.settles_owed(), table.len() == 1, "{table:?}");
        }
        let kept = kept_on(vec![lone(RX)], &[]);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(shape(&answered), [(RX, Some(POSITIVE.to_vec()))]);
        assert!(answered.open_pending_ids().is_empty());
    }

    /// A late start judged at the limit shows the window ended at the limit, though the kept
    /// timing, judged again with the start, no longer says so: the response that completes it is
    /// the vehicle's, with or without a lone pending.
    #[test]
    fn a_late_start_at_the_limit_keeps_a_collecting_exchange_the_vehicles() {
        use super::super::timing::LateWindow;
        for table in [&[][..], &[RX, RX2][..]] {
            for answers in [Vec::new(), vec![lone(RX)]] {
                let mut kept = kept_on(answers, table);
                kept.evidence.timing.window = Window::NotJudged;
                kept.evidence.timing.not_judged_reason = Some(NotJudgedReason::ResponseBegun);
                kept.late_start = Some(LateStart::Late {
                    by_us: 2_000,
                    at_limit: true,
                });
                kept.late_start_id = Some(RX);
                let late = late_on(Some(RX));
                let answered = kept.answered(&POSITIVE, &late);
                let (class, reason) = settle_late_response(
                    &kept,
                    &answered,
                    LateWindow::Late { by_us: 2_000 },
                    &LinkHistory::empty(),
                    Some(RX),
                    &POSITIVE,
                );
                assert_eq!(class, Classification::Vehicle, "{table:?} {reason}");
            }
        }
    }

    /// Row H: an unnamed lone pending (identifier 0) is no pending of a named identifier, so a
    /// named final answer folds nothing; the pending stays open for the other identifier.
    #[test]
    fn a_named_late_final_answer_folds_no_unnamed_pending() {
        let kept = kept_collecting(vec![lone(0)], true);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert_eq!(answered.answers.len(), 2);
        assert_eq!(answered.open_pending_ids(), [0]);
        assert_eq!(answered.owed_ids(), [RX2]);
    }

    /// ADR-433 decision C (design 8.11 "Owed request"): an unnamed pending is open while some
    /// identifier of the receive table has no final answer, and on none once every one has it;
    /// the owed identifiers read the same definition.
    #[test]
    fn an_unnamed_pending_is_open_only_while_a_table_identifier_lacks_a_final_answer() {
        // Every identifier answered: nothing open, the exchange settles.
        let both = kept_collecting(
            vec![
                lone(0),
                positive(RX, Window::WithinP2),
                positive(RX2, Window::WithinP2),
            ],
            true,
        );
        assert!(both.evidence.open_pending_ids().is_empty());
        assert!(both.evidence.owed_ids().is_empty());
        assert!(both.evidence.settles_owed());
        // One of two answered: the pending stays open on the other.
        let one = kept_collecting(vec![lone(0), positive(RX, Window::WithinP2)], true);
        assert_eq!(one.evidence.open_pending_ids(), [0]);
        assert_eq!(one.evidence.owed_ids(), [RX2]);
        assert!(!one.evidence.settles_owed());
        // A one-ECU table answered: covered; unanswered: open on the table's identifier.
        let lone_ecu = kept_on(vec![lone(0), positive(RX, Window::WithinP2)], &[RX]);
        assert!(lone_ecu.evidence.open_pending_ids().is_empty());
        assert!(lone_ecu.evidence.settles_owed());
        let waiting = kept_on(vec![lone(0)], &[RX]);
        assert_eq!(waiting.evidence.open_pending_ids(), [0]);
        assert_eq!(waiting.evidence.owed_ids(), [RX]);
        // No receive table: nothing covers an identifier, the pending stays open.
        let physical = kept_on(vec![lone(0), positive(RX, Window::WithinP2)], &[]);
        assert_eq!(physical.evidence.open_pending_ids(), [0]);
    }

    /// ADR-433 decision C: the named final answer that covers the last uncovered identifier
    /// folds the unnamed pending, which leaves the list with its pending chain on the answer.
    #[test]
    fn a_named_late_final_answer_covering_the_table_folds_the_unnamed_pending() {
        let kept = kept_collecting(vec![lone(0), positive(RX, Window::WithinP2)], true);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX2)));
        assert_eq!(
            shape(&answered)
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            [RX, RX2]
        );
        assert!(answered.open_pending_ids().is_empty());
        assert!(answered.settles_owed());
        assert_eq!(answered.answers.last().unwrap().pending.len(), 1);
        // The first of two does not cover the table: the unnamed pending stays.
        let kept = kept_collecting(vec![lone(0)], true);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert_eq!(answered.open_pending_ids(), [0]);
        assert_eq!(answered.owed_ids(), [RX2]);
    }

    /// ADR-433 decision C: on a one-ECU table an unnamed final answer is keyed on the table's
    /// identifier as the host keys it, and folds the unnamed lone pending.
    #[test]
    fn an_unnamed_late_final_answer_on_a_one_ecu_table_is_keyed_on_its_identifier() {
        let kept = kept_on(vec![lone(0)], &[RX]);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(shape(&answered), [(RX, Some(POSITIVE.to_vec()))]);
        assert_eq!(answered.answers[0].pending.len(), 1);
        assert!(answered.open_pending_ids().is_empty());
        assert!(answered.settles_owed());
    }

    /// The covering rule is one function: the same answers fold to the same list through it,
    /// whichever restatement calls it.
    #[test]
    fn the_covering_fold_leaves_the_unnamed_pending_only_while_the_table_is_uncovered() {
        let expected = kept_on(Vec::new(), &[RX, RX2]).evidence.expected;
        let items = [
            lone(0),
            positive(RX, Window::WithinP2),
            positive(RX2, Window::WithinP2),
        ];
        let fold = |items: &[Answer], table: &[u32]| {
            standing_answers_covering(
                items,
                |a| a.rx_id,
                |a| a.is_pending(&expected),
                Answer::is_rejected,
                table,
            )
        };
        assert_eq!(fold(&items, &[RX, RX2]), [false, true, true]);
        assert_eq!(fold(&items[..2], &[RX, RX2]), [true, true]);
        assert_eq!(fold(&items, &[]), [true, true, true]);
        // The plain rule is the covering rule with no table.
        let plain = standing_answers(
            &items,
            |a| a.rx_id,
            |a| a.is_pending(&expected),
            Answer::is_rejected,
        );
        assert_eq!(plain, fold(&items, &[]));
    }

    /// A request meant for one ECU that got `count` answers, with one earlier request on the
    /// link still owed, built with the history the link gives it.
    fn owed_entry(count: usize) -> ExchangeEntry {
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x86], End::NoResponse));
        let reply = |extra| End::Positive(vec![0x62, 0xF1, 0x86, extra]);
        let mut evidence = judged(forwarding(read([0xF1, 0x86], reply(1))));
        evidence.answers = (0..count)
            .map(|extra| answer(RX, reply(extra as u8 + 1), Window::WithinP2, 5.0))
            .collect();
        let history = state.history_for(&evidence);
        assert_eq!(history.owed_matching, 1);
        ExchangeEntry::build(2, evidence, history)
    }

    #[test]
    fn extra_answers_are_weighed_against_the_owed_requests_the_link_gives() {
        // One extra answer may be the owed request's late one; two cannot both be.
        let one_extra = owed_entry(2);
        assert_eq!(one_extra.classification, Classification::Undetermined);
        let two_extra = owed_entry(3);
        assert_eq!(
            two_extra.classification,
            Classification::Vehicle,
            "{}",
            two_extra.reason
        );
        assert!(
            two_extra.reason.contains("second responder"),
            "{}",
            two_extra.reason
        );
        // The owed sentence is said once by the entry, and not by its answers.
        let said = |entry: &ExchangeEntry| entry.reason.matches("could also belong").count();
        assert_eq!(said(&one_extra), 1, "{}", one_extra.reason);
        assert_eq!(said(&two_extra), 0);
        for entry in [&one_extra, &two_extra] {
            assert!(
                entry
                    .answers
                    .iter()
                    .all(|item| !item.reason.contains("could also belong")),
                "{:?}",
                entry.answers
            );
            assert!(
                entry
                    .answers
                    .iter()
                    .all(|item| item.classification == Classification::None)
            );
        }
    }

    /// A read of 22 F1 90 answered by `replies` (the first its own), on a link where one request
    /// per entry of `owed` (a data identifier) is still owed; every answer carries its own
    /// history, as the log gives it.
    fn entry_with_owed(owed: &[[u8; 2]], replies: &[Vec<u8>]) -> ExchangeEntry {
        let mut state = LinkState::default();
        for (seq, did) in owed.iter().enumerate() {
            log(&mut state, seq as u64 + 1, read(*did, End::NoResponse));
        }
        let end = |bytes: &Vec<u8>| {
            if bytes[0] == 0x7F {
                End::Negative(bytes.clone())
            } else {
                End::Positive(bytes.clone())
            }
        };
        let mut evidence = judged(forwarding(read([0xF1, 0x90], end(&replies[0]))));
        evidence.answers = replies
            .iter()
            .map(|bytes| answer(RX, end(bytes), Window::WithinP2, 5.0))
            .collect();
        let history = state.history_for(&evidence);
        let seq = owed.len() as u64 + 1;
        evidence.set_answer_histories(state.answer_histories(&evidence, &history, seq));
        ExchangeEntry::build(seq, evidence, history)
    }

    /// ADR-400 decision 4C: an answer's owners follow the owed requests as a settlement leaves
    /// them. Read A is owed; read B got two compatible answers, which A could own; A's final
    /// response then shows the second one is a second responder.
    #[test]
    fn an_owed_reads_final_response_leaves_the_later_reads_extra_answer_a_second_responder() {
        let mut state = LinkState::default();
        let confirmed = |did: [u8; 2], end| {
            let mut evidence = judged(forwarding(read(did, end)));
            evidence.transmit_evidence = Some(TransmitEvidence::TxIndication);
            evidence
        };
        let (_, kept) = log_kept(&mut state, 1, confirmed([0xF1, 0x91], End::NoResponse));
        state.keep(1, kept.clone());
        let mut second = confirmed([0xF1, 0x90], End::Positive(vec![0x62, 0xF1, 0x90, 1]));
        second.answers = [vec![0x62, 0xF1, 0x90, 1], vec![0x7F, 0x22, 0x22]]
            .into_iter()
            .map(|bytes| {
                let end = if bytes[0] == 0x7F {
                    End::Negative(bytes)
                } else {
                    End::Positive(bytes)
                };
                answer(RX, end, Window::WithinP2, 5.0)
            })
            .collect();
        let history = state.history_for(&second);
        second.set_answer_histories(state.answer_histories(&second, &history, 2));
        let (entry, _) = log_kept(&mut state, 2, second);
        assert_eq!(
            entry.classification,
            Classification::Undetermined,
            "{}",
            entry.reason
        );
        let (_, changed, _) = settle_late(&mut state, 1, &kept, &[0x62, 0xF1, 0x91, 7]);
        let later = changed.iter().find(|later| later.seq == 2);
        assert_eq!(
            later.map(|later| later.classification),
            Some(Classification::Vehicle),
            "{changed:?}"
        );
        let logged = state.window.iter().find(|logged| logged.seq == 2).unwrap();
        assert!(logged.evidence.answers.iter().all(|answer| {
            answer
                .history
                .as_ref()
                .is_some_and(|own| own.owners.is_empty() && own.owed_matching == 0)
        }));
    }

    #[test]
    fn an_extra_answer_an_earlier_request_explains_is_not_a_second_responder() {
        // The first answer matches no owed request; the extra negative one matches the owed read.
        let entry = entry_with_owed(
            &[[0xF1, 0x91]],
            &[vec![0x62, 0xF1, 0x90, 1], vec![0x7F, 0x22, 0x22]],
        );
        assert_eq!(
            entry.classification,
            Classification::Undetermined,
            "{}",
            entry.reason
        );
        assert_eq!(
            entry
                .reason
                .matches("could also belong to 1 earlier")
                .count(),
            1,
            "{}",
            entry.reason
        );
        assert!(
            !entry.reason.contains("second responder"),
            "{}",
            entry.reason
        );
    }

    #[test]
    fn two_extras_with_one_compatible_owner_are_a_second_responder() {
        let entry = entry_with_owed(
            &[[0xF1, 0x91]],
            &[
                vec![0x62, 0xF1, 0x90, 1],
                vec![0x7F, 0x22, 0x22],
                vec![0x7F, 0x22, 0x22],
            ],
        );
        assert_eq!(
            entry.classification,
            Classification::Vehicle,
            "{}",
            entry.reason
        );
        // An extra no owed request fits is one too.
        let none = entry_with_owed(
            &[[0xF1, 0x91]],
            &[vec![0x62, 0xF1, 0x90, 1], vec![0x62, 0xF1, 0x90, 2]],
        );
        assert_eq!(
            none.classification,
            Classification::Vehicle,
            "{}",
            none.reason
        );
    }

    #[test]
    fn extras_each_explained_by_a_distinct_owner_are_undetermined() {
        let entry = entry_with_owed(
            &[[0xF1, 0x91], [0xF1, 0x92]],
            &[
                vec![0x62, 0xF1, 0x90, 1],
                vec![0x7F, 0x22, 0x22],
                vec![0x62, 0xF1, 0x92, 7],
            ],
        );
        assert_eq!(
            entry.classification,
            Classification::Undetermined,
            "{}",
            entry.reason
        );
        assert_eq!(
            entry
                .reason
                .matches("could also belong to 2 earlier")
                .count(),
            1,
            "{}",
            entry.reason
        );
    }

    #[test]
    fn owners_absorb_reassigns_a_shared_owner() {
        assert!(owners_absorb(&[]));
        assert!(owners_absorb(&[&[1, 2], &[1]]));
        assert!(!owners_absorb(&[&[1], &[1]]));
        assert!(!owners_absorb(&[&[1], &[]]));
    }

    #[test]
    fn a_lone_pending_answer_that_came_late_is_system_as_a_single_exchange_is() {
        let mut lone = answer(
            RX,
            End::Negative(vec![0x7F, 0x3E, 0x78]),
            Window::Late,
            90.0,
        );
        lone.timing.late_pending = Some(0);
        lone.timing.window_by_ms = Some(3.0);
        let (end, pending) = lone.as_exchange(&Expected::uds(&PRESENCE));
        assert!(matches!(end, End::NoResponse));
        assert_eq!(pending.len(), 1);
        let entry = entry_of(collecting(vec![lone]));
        assert_eq!(classes(&entry), [Classification::System]);
        assert_eq!(entry.classification, Classification::System);
    }

    #[test]
    fn each_answer_is_classified_against_its_own_history() {
        let mut second = positive(0x7E9, Window::WithinP2);
        let mut owed = LinkHistory::empty();
        owed.this_exchange = true;
        owed.owed_matching = 1;
        second.history = Some(owed);
        let entry = entry_of(collecting(vec![positive(RX, Window::WithinP2), second]));
        assert_eq!(
            classes(&entry),
            [Classification::None, Classification::Undetermined]
        );
        assert_eq!(entry.classification, Classification::Undetermined);
    }

    /// ADR-435 (case a): a final answer on one identifier of the table leaves the other, which
    /// is silent, owed: the exchange does not settle, and the silent identifier's late final
    /// answer settles it whole.
    #[test]
    fn a_silent_identifier_beside_a_final_answer_is_owed_and_unsettled() {
        let kept = kept_collecting(vec![positive(RX2, Window::WithinP2)], true);
        assert_eq!(kept.evidence.silent_ids(), [RX]);
        assert_eq!(kept.evidence.owed_ids(), [RX]);
        assert!(!kept.evidence.settles_owed());
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert!(answered.silent_ids().is_empty());
        assert!(answered.owed_ids().is_empty());
        assert!(answered.settles_owed());
        // A physical link has no table: nothing is silent.
        let physical = kept_collecting(vec![positive(RX, Window::WithinP2)], false);
        assert!(physical.evidence.silent_ids().is_empty());
        assert!(physical.evidence.settles_owed());
    }

    /// ADR-435 (case d): an unnamed final answer covers no identifier of the table, so both are
    /// owed. A late final answer naming one settles that one only; a late one naming none is
    /// keyed on the one identifier then owed and settles the exchange, and with both owed it
    /// settles none.
    #[test]
    fn an_unnamed_final_answer_covers_no_identifier_and_a_late_one_is_keyed_on_the_one_owed() {
        let kept = kept_collecting(vec![positive(0, Window::WithinP2)], true);
        assert_eq!(kept.evidence.owed_ids(), [RX, RX2]);
        assert!(!kept.evidence.settles_owed());
        let both = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(both.owed_ids(), [RX, RX2]);
        assert!(!both.settles_owed());
        let named = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert_eq!(named.owed_ids(), [RX2]);
        assert!(!named.settles_owed());
        // Kept again as it now stands, the one identifier owed takes an unnamed final answer.
        let rekept = kept_on(named.answers.clone(), &[RX, RX2]);
        assert_eq!(rekept.evidence.owed_ids(), [RX2]);
        let unnamed = rekept.answered(&POSITIVE, &late_on(None));
        assert_eq!(unnamed.answers.last().map(|a| a.rx_id), Some(RX2));
        assert!(unnamed.owed_ids().is_empty());
        assert!(unnamed.settles_owed());
    }

    /// An answer whose history shows an owner, for the owned-final tests.
    fn owned(mut answer: Answer) -> Answer {
        let mut history = LinkHistory::empty();
        history.owners = vec![9];
        history.owed_matching = 1;
        answer.history = Some(history);
        answer
    }

    /// ADR-435 (cases f and f'): a final answer an owed request could own stays an owed
    /// identifier whatever else is owed: beside a silent identifier, and beside an open pending.
    #[test]
    fn an_owned_final_answer_is_owed_beside_a_silent_identifier_and_an_open_pending() {
        // Case f: final answer of RX2 owned, RX silent.
        let silent = kept_collecting(vec![owned(positive(RX2, Window::WithinP2))], true);
        assert_eq!(silent.evidence.owed_ids(), [RX, RX2]);
        assert!(!silent.evidence.settles_owed());
        // Case f': final answer of RX2 owned, a pending open on RX.
        let pending = kept_collecting(vec![owned(positive(RX2, Window::WithinP2)), lone(RX)], true);
        assert_eq!(pending.evidence.open_pending_ids(), [RX]);
        assert_eq!(pending.evidence.owed_ids(), [RX, RX2]);
        // Not owned: only the open pending's identifier.
        let unowned = kept_collecting(vec![positive(RX2, Window::WithinP2), lone(RX)], true);
        assert_eq!(unowned.evidence.owed_ids(), [RX]);
    }

    /// ADR-435: an unnamed late final answer is keyed on the one identifier that is unanswered
    /// (silent, or with a pending open), not on one owed only because an owner could own its
    /// final answer.
    #[test]
    fn an_unnamed_late_final_answer_is_not_keyed_on_an_identifier_owed_through_an_owner() {
        let kept = kept_collecting(vec![owned(positive(RX, Window::WithinP2)), lone(RX2)], true);
        assert_eq!(kept.evidence.unanswered_ids(), [RX2]);
        assert_eq!(kept.evidence.owed_ids(), [RX2, RX]);
        let answered = kept.answered(&POSITIVE, &late_on(None));
        assert_eq!(answered.answers.last().map(|a| a.rx_id), Some(RX2));
        assert!(answered.open_pending_ids().is_empty());
        assert!(answered.settles_owed());
    }

    /// ADR-435, ADR-437: a result the worker rejected is no answer on its identifier, so an
    /// exchange whose only results are rejected ones ends as `positive_unusable` with the first
    /// one's bytes, is not settled and is owed on every receive identifier; a final answer
    /// delivered late ends it with that answer, and settles it once no identifier is silent.
    #[test]
    fn a_rejected_only_window_is_owed_on_every_identifier_and_a_late_final_answer_ends_it() {
        let mut other = rejected(RX2);
        other.end = End::PositiveUnusable(vec![0x7E, 0x02]);
        let kept = kept_collecting(vec![rejected(RX), other], true);
        let evidence = &kept.evidence;
        assert!(evidence.end_rejected);
        assert!(matches!(&evidence.end, End::PositiveUnusable(bytes) if bytes == &WRONG_ECHO));
        assert_eq!(evidence.silent_ids(), [RX, RX2]);
        assert!(!evidence.settles_owed());
        assert_eq!(evidence.owed_ids(), [RX, RX2]);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX)));
        assert!(!answered.end_rejected);
        assert_eq!(answered.end.response(), Some(&POSITIVE[..]));
        assert_eq!(answered.silent_ids(), [RX2]);
        assert!(!answered.settles_owed());
        assert_eq!(answered.owed_ids(), [RX2]);
    }

    /// ADR-437: a rejected result beside a usable answer on another identifier changes neither
    /// the end (the usable answer's, whatever the order) nor the debt on its own identifier: the
    /// identifier stays silent and owed.
    #[test]
    fn a_rejected_result_beside_a_usable_answer_leaves_its_identifier_silent() {
        for answers in [
            vec![rejected(RX), positive(RX2, Window::WithinP2)],
            vec![positive(RX2, Window::WithinP2), rejected(RX)],
        ] {
            let evidence = kept_collecting(answers, true).evidence;
            assert!(!evidence.end_rejected);
            assert_eq!(evidence.end.response(), Some(&POSITIVE[..]));
            assert_eq!(evidence.silent_ids(), [RX]);
            assert_eq!(evidence.owed_ids(), [RX]);
            assert!(!evidence.settles_owed());
        }
    }

    /// ADR-437: the whole-table debt of a rejected end (ADR-435 decision 7) holds whenever the
    /// end is a rejected result, whatever the answers beside it: pendings, a physical link, or a
    /// result the exchange's owners could own.
    #[test]
    fn a_rejected_end_is_always_owed_on_the_whole_table() {
        let table = [RX, RX2];
        for answers in [
            vec![rejected(RX)],
            vec![rejected(RX2)],
            vec![lone(RX), rejected(RX)],
            vec![rejected(RX), lone(RX2)],
            vec![lone(RX), lone(RX2), rejected(RX2)],
            vec![owned(rejected(RX)), rejected(RX2)],
        ] {
            let evidence = kept_collecting(answers, true).evidence;
            assert!(evidence.end_rejected, "{:?}", evidence.answers);
            let owed = evidence.owed_ids();
            for id in table {
                assert!(owed.contains(&id), "{id:#x} missing in {owed:x?}");
            }
            assert!(owed.iter().all(|id| table.contains(id)), "{owed:x?}");
        }
    }

    /// ADR-437: a rejected result folds no pending and covers no unnamed pending; the end is
    /// the first standing answer that is neither pending nor rejected, else the first rejected
    /// result's, else no response.
    #[test]
    fn the_fold_and_the_end_treat_a_rejected_result_as_no_answer_on_its_identifier() {
        let expected = Expected::uds(&PRESENCE);
        let fold = |items: &[Answer], table: &[u32]| {
            standing_answers_covering(
                items,
                |a| a.rx_id,
                |a| a.is_pending(&expected),
                Answer::is_rejected,
                table,
            )
        };
        // A rejected result after a pending of its identifier leaves the pending standing; a
        // usable one folds it.
        assert_eq!(fold(&[lone(RX), rejected(RX)], &[RX, RX2]), [true, true]);
        assert_eq!(
            fold(&[lone(RX), positive(RX, Window::WithinP2)], &[RX, RX2]),
            [false, true]
        );
        // The unnamed pending stays while a rejected result stands for one identifier.
        let items = [lone(0), rejected(RX), positive(RX2, Window::WithinP2)];
        assert_eq!(fold(&items, &[RX, RX2]), [true, true, true]);
        let items = [
            lone(0),
            positive(RX, Window::WithinP2),
            positive(RX2, Window::WithinP2),
        ];
        assert_eq!(fold(&items, &[RX, RX2]), [false, true, true]);
        // The end rule: the first usable answer wins over an earlier rejected one; with none, the
        // first rejected result's bytes; with only pendings or nothing, no response.
        let first = |items: &[Answer]| format!("{:?}", collecting_end(items, &expected));
        let mut second = rejected(RX2);
        second.end = End::PositiveUnusable(vec![0x7E, 0x02]);
        assert_eq!(
            first(&[rejected(RX), positive(RX2, Window::WithinP2)]),
            "(Positive([126, 0]), false)"
        );
        assert_eq!(
            first(&[lone(RX), rejected(RX2), second.clone()]),
            "(PositiveUnusable([126, 1]), true)"
        );
        assert_eq!(
            first(&[second, rejected(RX)]),
            "(PositiveUnusable([126, 2]), true)"
        );
        assert_eq!(first(&[lone(RX)]), "(NoResponse, false)");
        assert_eq!(first(&[]), "(NoResponse, false)");
    }

    /// ADR-437: a rejected answer is no final answer standing on its identifier, and its
    /// owners do not make the identifier owed through step three of the owed rule.
    #[test]
    fn a_rejected_answer_is_no_final_answer_and_owes_nothing_through_its_owners() {
        let mut evidence = ended(collecting(vec![
            owned(rejected(RX)),
            owned(positive(RX2, Window::WithinP2)),
        ]));
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        assert!(!evidence.final_standing(RX));
        assert!(evidence.final_standing(RX2));
        // RX is silent (rejected); RX2 is owed only through its owner.
        assert_eq!(evidence.silent_ids(), [RX]);
        assert_eq!(evidence.owed_ids(), [RX, RX2]);
        // With the rejected answer alone owned, the owner adds nothing beyond the silence.
        let mut evidence = ended(collecting(vec![
            owned(rejected(RX)),
            positive(RX2, Window::WithinP2),
        ]));
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        assert_eq!(evidence.owed_ids(), [RX]);
        // A rejected result that names no identifier may have come on any of them, yet it owns
        // none: with both identifiers answered usably, nothing is owed through its owners.
        let mut evidence = ended(collecting(vec![
            owned(rejected(0)),
            positive(RX, Window::WithinP2),
            positive(RX2, Window::WithinP2),
        ]));
        evidence.rx_id = 0;
        evidence.receive_ids = vec![RX, RX2];
        assert!(!evidence.end_rejected);
        assert!(evidence.silent_ids().is_empty());
        assert!(evidence.owed_ids().is_empty());
    }

    /// ADR-437: a final answer delivered late folds the pending of its own identifier only: a
    /// rejected result read after a pending on another identifier folded nothing, so the pending
    /// is still open when the late answer comes on a different identifier.
    #[test]
    fn a_late_answer_on_another_identifier_leaves_the_pending_a_rejected_result_did_not_fold() {
        let kept = kept_collecting(vec![lone(RX), rejected(RX)], true);
        assert_eq!(kept.evidence.answers.len(), 2);
        assert_eq!(kept.evidence.open_pending_ids(), [RX]);
        let answered = kept.answered(&POSITIVE, &late_on(Some(RX2)));
        assert_eq!(answered.answers.len(), 3);
        assert_eq!(answered.open_pending_ids(), [RX]);
        assert!(!answered.end_rejected);
        assert!(!answered.settles_owed());
    }

    /// ADR-359 item 3, ADR-435 decision 7, ADR-443: a request that is not collecting and whose
    /// primitive finished on a wrong echo ends as `positive_unusable`, rejected, and is owed on
    /// its own identifier without settling anything; the final answer delivered late is a usable
    /// response, so it restates the exchange as `positive`, no longer rejected, with an answer
    /// head, settling what was owed.
    #[test]
    fn a_non_collecting_rejected_end_is_owed_and_a_late_usable_answer_settles_it() {
        let mut evidence = evidence(&[0x22, 0xF1, 0x86], End::PositiveUnusable(vec![0x62, 0xF1]));
        evidence.end_rejected = true;
        evidence.vci_ended = true;
        assert_eq!(evidence.owed_ids(), [RX]);
        // With a receive table (a functional link, own identifier 0) the rejected end is owed on
        // the whole table, as a collecting one is (ADR-435 decision 7, ADR-443), where a usable
        // final response is owed on the link's identifier alone.
        let mut table = evidence.clone();
        table.rx_id = 0;
        table.receive_ids = vec![RX, RX2];
        assert_eq!(table.owed_ids(), [0, RX, RX2]);
        table.end_rejected = false;
        table.end = End::Positive(vec![0x62, 0xF1, 0x86, 0x01]);
        assert_eq!(table.owed_ids(), [0, RX, RX2]);
        assert!(!evidence.settles_owed());
        assert!(evidence.answer_head().is_none());
        let kept = log_kept(&mut LinkState::default(), 1, evidence).1;
        assert!(kept.evidence.end_rejected && !kept.evidence.settles_owed());
        let answered = kept.answered(&[0x62, 0xF1, 0x86, 0x01], &late_on(None));
        assert!(!answered.end_rejected);
        assert!(
            matches!(&answered.end, End::Positive(bytes) if bytes == &[0x62, 0xF1, 0x86, 0x01])
        );
        assert!(answered.settles_owed());
        assert_eq!(
            answered.answer_head().as_deref(),
            Some(&[0x62, 0xF1, 0x86, 0x01][..])
        );
    }

    /// ADR-446: a non-collecting exchange with a usable final response on a link with a receive
    /// table is owed on its own identifier and, when that is 0, on every receive identifier; a
    /// record of it whose evidence settles it owns no late final, whatever ended the VCI's wait,
    /// where a rejected end, a silent one and a collecting exchange keep what they owned.
    #[test]
    fn a_settled_non_collecting_record_owns_no_late_final_and_is_owed_on_the_table() {
        let mut table = evidence(
            &[0x22, 0xF1, 0x86],
            End::Positive(vec![0x62, 0xF1, 0x86, 0x01]),
        );
        table.rx_id = 0;
        table.receive_ids = vec![RX, RX2];
        table.vci_ended = true;
        assert_eq!(table.owed_ids(), [0, RX, RX2]);
        // A named own identifier on the table is owed on it alone, as before.
        let mut named = table.clone();
        named.rx_id = RX2;
        assert_eq!(named.owed_ids(), [RX2]);
        let kept = log_kept(&mut LinkState::default(), 1, table.clone()).1;
        assert!(kept.evidence.settles_owed() && kept.evidence.vci_ended);
        assert!(!kept.late_final_settles());
        // The rejected end, the silent one and a collecting exchange are not changed.
        let mut rejected_end = table.clone();
        rejected_end.end = End::PositiveUnusable(vec![0x62, 0xF1]);
        rejected_end.end_rejected = true;
        assert_eq!(rejected_end.owed_ids(), [0, RX, RX2]);
        assert!(
            log_kept(&mut LinkState::default(), 1, rejected_end)
                .1
                .late_final_settles()
        );
        let mut silent = table.clone();
        silent.end = End::NoResponse;
        assert_eq!(silent.owed_ids(), [RX, RX2]);
        assert!(
            log_kept(&mut LinkState::default(), 1, silent)
                .1
                .late_final_settles()
        );
        let mut settled = collecting(vec![positive(RX, Window::WithinP2)]);
        settled.rx_id = 0;
        settled.receive_ids = vec![RX];
        assert!(settled.settles_owed() && settled.vci_ended);
        assert!(
            log_kept(&mut LinkState::default(), 1, settled)
                .1
                .late_final_settles()
        );
    }

    /// ADR-437: `as_answer` derives `end_rejected` from the answer, so a rejected answer is no
    /// answer head for ownership and a usable one is.
    #[test]
    fn as_answer_derives_end_rejected_from_the_answer() {
        let evidence = ended(collecting(vec![
            rejected(RX),
            positive(RX2, Window::WithinP2),
        ]));
        assert!(!evidence.end_rejected);
        let one = evidence.as_answer(&evidence.answers[0]);
        assert!(one.end_rejected && one.answer_head().is_none());
        let one = evidence.as_answer(&evidence.answers[1]);
        assert!(!one.end_rejected && one.answer_head().is_some());
    }

    /// ADR-437: the answers list keeps a rejected result with its own outcome, its bytes
    /// redacted as a rejected result's are (ADR-359 item 1), and a usable answer's as an answer's.
    /// A SecurityAccess key response read where the request was another service tells the two
    /// apart: an answer shows it whole, a rejected result keeps its data back.
    #[test]
    fn a_rejected_result_stays_in_the_answers_list_redacted() {
        let bytes = vec![0x67, 0x02, 0xAA, 0xBB];
        let mut stray = rejected(RX);
        stray.end = End::PositiveUnusable(bytes.clone());
        let entry = entry_of(ended(collecting(vec![
            stray,
            positive(RX2, Window::WithinP2),
        ])));
        assert_eq!(entry.answers.len(), 2);
        assert_ne!(
            redact_unanswering(&bytes),
            redact(Direction::Response, &bytes)
        );
        assert_eq!(entry.answers[0].response, redact_unanswering(&bytes));
        assert!(
            matches!(&entry.answers[0].outcome, Outcome::PositiveUnusable { response }
                if *response == redact_unanswering(&bytes)),
            "{:?}",
            entry.answers[0].outcome
        );
        assert_eq!(
            entry.answers[1].response,
            redact(Direction::Response, &POSITIVE)
        );
        assert!(
            matches!(entry.outcome, Outcome::Positive { .. }),
            "{:?}",
            entry.outcome
        );
    }

    /// ADR-435: a reset that ends the owners before it makes a later exchange drop the rows it
    /// held only because they could own its answers, and frees the answers after it from it.
    #[test]
    fn a_reset_that_drops_a_later_exchanges_row_frees_the_answers_after_it() {
        let table = |mut evidence: Evidence| {
            evidence.rx_id = 0;
            evidence.receive_ids = vec![RX, RX2];
            evidence
        };
        let mut state = LinkState::default();
        let mut first = table(collecting(vec![lone(RX), lone(RX2)]));
        first.end = End::NoResponse;
        let (_, kept) = log_kept_owned(&mut state, 1, first);
        state.keep(1, kept);
        let mut reset = judged(forwarding(evidence(&[0x10, 0x03], End::NoResponse)));
        reset.transmit_evidence = Some(TransmitEvidence::TxIndication);
        let (_, kept) = log_kept_owned(&mut state, 2, reset);
        state.keep(2, kept);
        let answered = || table(collecting(vec![positive(RX, Window::WithinP2)]));
        let (_, kept) = log_kept_owned(&mut state, 3, answered());
        state.keep(3, kept);
        log_kept_owned(&mut state, 4, answered());
        assert_eq!(window_history(&state, 3).owners, [1]);
        assert_eq!(state.owed_row_ids(3), [RX2, RX]);
        assert_eq!(window_history(&state, 4).owners, [1, 3]);
        state.reset_after(2, 2, true);
        // The first exchange ended with the reset: the third is owed on its silent RX2 only,
        // and no longer owns the fourth's answer.
        assert_eq!(state.owed_row_ids(3), [RX2]);
        assert!(window_history(&state, 4).owners.is_empty());
    }

    /// ADR-435: a result the worker rejected owes every receive identifier whatever else is
    /// owed, and a refresh of rows only drops them: a settlement of an earlier exchange does not
    /// make the rejected one owed on more, nor give its answers' owners to a later answer.
    #[test]
    fn a_rejected_end_is_owed_on_every_identifier_and_a_refresh_never_adds_rows() {
        let table = |mut evidence: Evidence| {
            evidence.rx_id = 0;
            evidence.receive_ids = vec![RX, RX2];
            evidence
        };
        let rejected_end = table(ended(collecting(vec![rejected(RX), lone(RX2)])));
        assert!(rejected_end.end_rejected);
        assert_eq!(rejected_end.owed_ids(), [RX2, RX]);
        let mut state = LinkState::default();
        let (_, kept) = log_kept_owned(
            &mut state,
            1,
            table(collecting(vec![positive(RX2, Window::WithinP2), lone(RX)])),
        );
        state.keep(1, kept.clone());
        let second = table(ended(collecting(vec![rejected(RX), rejected(RX2)])));
        let (_, second_kept) = log_kept_owned(&mut state, 2, second);
        state.keep(2, second_kept);
        log_kept_owned(
            &mut state,
            3,
            table(collecting(vec![
                positive(RX2, Window::WithinP2),
                positive(RX, Window::WithinP2),
            ])),
        );
        assert_eq!(state.owed_row_ids(2), [RX, RX2]);
        let owners = |state: &LinkState| {
            state
                .logged_evidence(3)
                .unwrap()
                .answers
                .iter()
                .map(|answer| answer.history.clone().unwrap().owners)
                .collect::<Vec<_>>()
        };
        let before = owners(&state);
        settle_late(&mut state, 1, &kept, &POSITIVE);
        assert_eq!(state.owed_row_ids(2), [RX, RX2]);
        // An answer's owners only shrink: 1 let go of RX, the answer on RX2 gains none.
        let after = owners(&state);
        assert_eq!(after[0], before[0]);
        assert!(after[1].iter().all(|owner| before[1].contains(owner)));
    }

    /// ADR-435: a late start applies to a response that names no identifier when one identifier
    /// is unanswered, though another is owed only because an owner could own its final answer,
    /// as the response is keyed on the unanswered one.
    #[test]
    fn a_late_start_applies_to_an_unnamed_response_keyed_on_the_one_unanswered_identifier() {
        let mut kept = kept_collecting(vec![owned(positive(RX, Window::WithinP2))], true);
        assert_eq!(kept.evidence.owed_ids(), [RX2, RX]);
        kept.late_start = Some(LateStart::Late {
            by_us: 2_000,
            at_limit: true,
        });
        kept.late_start_id = Some(RX2);
        assert!(kept.late_start_for(None).is_some());
        assert!(kept.late_start_for(Some(RX2)).is_some());
    }

    /// ADR-435: the same, through a late link error read after the first exchange's answer: the
    /// owner went (the log ended it), the third exchange drops its row, and the fourth's answer
    /// is freed from it.
    #[test]
    fn a_late_link_error_that_drops_a_row_frees_the_answers_after_it() {
        let table = |mut evidence: Evidence| {
            evidence.rx_id = 0;
            evidence.receive_ids = vec![RX, RX2];
            evidence
        };
        let mut state = LinkState::default();
        let mut first = table(collecting(vec![lone(RX), lone(RX2)]));
        first.end = End::NoResponse;
        let (_, kept) = log_kept_owned(&mut state, 1, first);
        state.keep(1, kept);
        let mut other = judged(forwarding(evidence(&[0x10, 0x03], End::NoResponse)));
        other.transmit_evidence = Some(TransmitEvidence::TxIndication);
        log_kept_owned(&mut state, 2, other);
        let answered = || table(collecting(vec![positive(RX, Window::WithinP2)]));
        let (_, kept) = log_kept_owned(&mut state, 3, answered());
        state.keep(3, kept);
        log_kept_owned(&mut state, 4, answered());
        assert!(window_history(&state, 4).owners.contains(&3));
        state.owed.retain(|owed| owed.seq != 1);
        state.error_after(Some(2), 9, LinkErrorKind::Receive, None);
        assert_eq!(state.owed_row_ids(3), [RX2]);
        assert!(!window_history(&state, 4).owners.contains(&3));
    }

    /// ADR-435: refreshing an owed exchange's rows only drops rows: one it no longer holds is
    /// not added back, though its answers' owners would owe it.
    #[test]
    fn refreshing_the_rows_of_an_owed_exchange_never_adds_one() {
        let table = |mut evidence: Evidence| {
            evidence.rx_id = 0;
            evidence.receive_ids = vec![RX, RX2];
            evidence
        };
        let mut state = LinkState::default();
        let mut first = table(collecting(vec![lone(RX), lone(RX2)]));
        first.end = End::NoResponse;
        let (_, kept) = log_kept_owned(&mut state, 1, first);
        state.keep(1, kept);
        let (entry, kept) = log_kept_owned(
            &mut state,
            2,
            table(collecting(vec![positive(RX, Window::WithinP2)])),
        );
        state.keep(2, kept.clone());
        assert_eq!(state.owed_row_ids(2), [RX2, RX]);
        let (sid, expected) = (kept.evidence.sid().unwrap(), kept.evidence.expected.clone());
        state.set_owed_rows(2, sid, &expected, &[RX2]);
        let (dropped, emptied) = state.refresh_owed_rows(2, &kept.evidence, &entry.link_history);
        assert!(dropped.is_empty() && !emptied);
        assert_eq!(state.owed_row_ids(2), [RX2]);
    }
}

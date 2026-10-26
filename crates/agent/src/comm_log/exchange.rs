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
//! would stand. What today's evidence cannot give: no transmit indication or loopback, no extra
//! error information, no timing margin, and one answer per send-receive (`num_receive_cycles`
//! 1). So `sent` is `confirmed` only by an answer of the ECU, `not_sent` only by a refusal, and
//! `window` is `not_judged`.
//!
//! `text` and `reason` are built from the domain enums only, never from an internal name (a
//! host error, a gRPC status, an error event); those go to [`Detail`].

use std::collections::HashMap;

use diag_ir::{Idempotency, Interruptible, Section, SourceSpan};
use serde::Serialize;

use super::redaction::{Direction, LoggedMessage, redact};
use crate::host::positive_prefix;

/// The `event` of an exchange entry.
const EVENT: &str = "exchange";

const SID_SESSION_CONTROL: u8 = 0x10;
const SID_ECU_RESET: u8 = 0x11;
const SID_NEGATIVE: u8 = 0x7F;
const NRC_PENDING: u8 = 0x78;

/// Who sent the request (design 8.11, `origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Origin {
    Procedure,
    /// A request re-sent after a resume (8.2.5).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// No link gives a transmit time yet.
    NoTransmitTime,
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

/// VCI times of an exchange, raw, with their clock and epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VciTimes {
    pub clock: Clock,
    pub epoch: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_done_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_start_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_end_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_us: Option<u64>,
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
/// Not given yet, with the rows that need them: `answers`. No outcome rests on a worker's extra
/// error information (ADR-351).
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

/// One answer of a request that collects every answer, or the further answers of one meant for
/// one ECU (design 8.11, `answers`). No producer gives them yet; the answers rows are not
/// implemented, so an entry that has one is classified by the last row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AnswerItem {
    pub rx_id: u32,
    pub response: LoggedMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vci_us: Option<u64>,
    pub agent_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_ms: Option<f64>,
    pub outcome: Outcome,
    pub classification: Classification,
    pub reason: String,
}

/// Which side an exchange's fault lies on (design 8.11 "Classification"), ordered by severity:
/// `system` over `undetermined` over `vehicle` over `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    fn matches(&self, answer: &[u8]) -> bool {
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
    pub timing: Timing,
    pub times: Times,
    /// A transmit confirmation the link gave. No link gives one yet.
    pub transmit_evidence: Option<TransmitEvidence>,
    /// A start-of-message indication for this exchange's response was attached: the first
    /// frame of a response is an answer of the ECU (design 8.11 "Answer of the ECU").
    pub response_started: bool,
    /// The VCI ended the exchange itself: the worker reported a receive timeout for the
    /// primitive. An exchange the agent's own deadline ended did not (design 8.11
    /// `vci_did_not_end`).
    pub vci_ended: bool,
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
    pub answers: Vec<AnswerItem>,
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
            flow_control: false,
            link_errors: Vec::new(),
            link_errors_before_answer: false,
            channel_link_errors: Vec::new(),
            answers: Vec::new(),
            supply_voltage: None,
            detail: Detail::default(),
        }
    }

    fn sid(&self) -> Option<u8> {
        self.request.first().copied()
    }

    /// The final response's bytes, when the exchange ended with one.
    fn final_response(&self) -> Option<&[u8]> {
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
        if let Some(response) = self.final_response() {
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

    /// The exchange holds an answer the worker bound to it: a final response or a pending. The
    /// first frame of a response alone is not one (ADR-354): it confirms the send and counts as
    /// an answer, but the worker may still have discarded the full response.
    fn bound_answer(&self) -> bool {
        self.answer_head().is_some()
    }

    fn absorbing(&self) -> bool {
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
    rx_id: u32,
    sent: Sent,
    this_exchange: bool,
    /// The answer is a final response or a pending, not a response start alone
    /// ([`Evidence::bound_answer`]): only such an answer is a matched answer.
    bound_answer: bool,
    /// The owed requests the answer could belong to.
    owners: Vec<u64>,
    final_response: bool,
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

/// Whether `request` is one whose acceptance ends the earlier requests' evidence: a
/// DiagnosticSessionControl or an ECUReset of any type (design 8.11 "Owed request", ADR-346
/// item 6).
fn restarts_ecu(request: &[u8]) -> bool {
    matches!(request, [SID_SESSION_CONTROL, ..] | [SID_ECU_RESET, ..])
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
}

impl LinkState {
    /// The history the classification of `evidence` uses. Ownership never decides: every owed
    /// request the exchange's answer could belong to is counted.
    pub(crate) fn history_for(&self, evidence: &Evidence) -> LinkHistory {
        let head = evidence.answer_head();
        let owners: Vec<u64> = head
            .as_deref()
            .map(|answer| {
                self.owed
                    .iter()
                    .filter(|owed| owed.rx_id == evidence.rx_id && owed.expected.matches(answer))
                    .map(|owed| owed.seq)
                    .collect()
            })
            .unwrap_or_default();
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

    /// The link was opened again, the worker restarted or the VCI reconnected (design 8.11
    /// "Matched answer"): the matched answers start over, since the link may now be set up
    /// differently. The owed requests and the counts `answered` and `answered_since_error` stay
    /// ("Owed request": a response already on its way can still arrive).
    pub(crate) fn reopened(&mut self) {
        self.matched.clear();
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

    /// A link error came on the physical channel this link shares, logged as line `seq` (design
    /// 8.11 "Link error", "Matched answer"): the matched answers and the count since the error
    /// end, the owed requests stay. The log calls it on every target's state on that channel,
    /// so a target that sent nothing during the error still loses its matched answers;
    /// [`LinkState::record`] calls it for an error the exchange it takes in saw.
    pub(crate) fn channel_link_error(&mut self, seq: u64, kind: LinkErrorKind) {
        self.matched.clear();
        self.answered_since_error = 0;
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
    /// - A response to such a request that was accepted but is unusable (a bad field) ends the
    ///   matched answers only, since the ECU may be restarting but the agent cannot tell.
    /// - The exchange is owed when its request may have reached the ECU whole (`sent`
    ///   `confirmed` or `unknown`) and no final response is placed with it alone.
    pub(crate) fn record(&mut self, entry: &ExchangeEntry) {
        let effect = &entry.effect;
        let accepting = effect.restarts_ecu
            && ((effect.positive && effect.final_response)
                || (effect.suppressed && entry.classification == Classification::None));
        let unsure_restart = effect.restarts_ecu && effect.unusable;
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
            self.matched.clear();
            self.last_reset = Some(entry.seq);
        } else if unsure_restart {
            self.matched.clear();
        }
        if effect.sent == Sent::Unknown {
            self.unknown_sends += 1;
        }
        let placed_alone = effect.owners.is_empty() && (effect.final_response || accepting);
        let may_have_arrived = matches!(effect.sent, Sent::Confirmed | Sent::Unknown);
        if let (true, false, Some(sid)) = (may_have_arrived, placed_alone, effect.sid) {
            self.owed.push(Owed {
                seq: entry.seq,
                rx_id: effect.rx_id,
                sid,
                expected: effect.expected.clone(),
            });
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

    /// The entry for `evidence` as line `seq`, with `history` from
    /// [`LinkState::history_for`], classified by [`classify`].
    pub(crate) fn build(seq: u64, evidence: Evidence, history: LinkHistory) -> Self {
        let (sent, sent_evidence) = sent_of(&evidence, &history);
        let verdict = classify(&evidence, &history);
        let sid = evidence.sid();
        let effect = LinkEffect {
            key: MatchKey::of(&evidence),
            sid,
            expected: evidence.expected.clone(),
            rx_id: evidence.rx_id,
            sent,
            this_exchange: history.this_exchange,
            bound_answer: evidence.bound_answer(),
            owners: history.owners.clone(),
            final_response: evidence.final_response().is_some(),
            positive: matches!(evidence.end, End::Positive(_)),
            unusable: matches!(evidence.end, End::PositiveUnusable(_)),
            suppressed: evidence.suppressed,
            restarts_ecu: restarts_ecu(&evidence.request),
            link_error: match evidence.end {
                End::LinkError(kind) => Some(kind),
                _ => evidence
                    .link_errors
                    .first()
                    .or_else(|| evidence.channel_link_errors.first())
                    .copied(),
            },
            answer_after_error: evidence.link_errors_before_answer
                && !evidence.link_errors.is_empty()
                && evidence.channel_link_errors.is_empty()
                && evidence.final_response().is_some(),
        };
        Self {
            event: EVENT,
            seq,
            target: evidence.target,
            link: evidence.link,
            origin: evidence.origin,
            at: evidence.at,
            request: Request {
                sid: sid.map(|sid| format!("{sid:#04x}")),
                service: sid.and_then(service_name),
                bytes: redact(Direction::Request, &evidence.request),
                suppressed: evidence.suppressed,
            },
            sent,
            sent_evidence,
            caused_by: evidence.caused_by,
            outcome: Outcome::of(&evidence.end),
            answers: evidence.answers,
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
    let mut outcome = match &evidence.end {
        End::Positive(_) => {
            let timing = match evidence.timing.window {
                Window::NotJudged => " (its time is not judged)",
                _ => "",
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
            (Window::TimeoutAtLimit, Some(p2)) => format!("no response within P2 ({p2} ms)"),
            _ => "no response came".to_owned(),
        },
        End::ResponseIncomplete => "the ECU began a response and it did not complete".to_owned(),
        End::VciDidNotEnd => {
            "the agent stopped waiting before the VCI ended the exchange".to_owned()
        }
        End::LinkError(kind) => format!("the link reported {}", kind.phrase()),
        End::NotSent(_) => unreachable!("returned above"),
        End::VciLost {
            refused_after_loss: true,
        } => "the VCI had been lost before the request, so the link refused it".to_owned(),
        End::VciLost { .. } => {
            "the connection to the VCI was lost while waiting for the response".to_owned()
        }
        End::Unknown(cause) => format!("the diagnostic system failed: {}", cause.phrase()),
    };
    if evidence.pending_seen() {
        outcome.push_str(&format!(
            ", after {} response-pending message(s) (0x78)",
            evidence.pending.len()
        ));
    }
    format!("{sent_phrase}; {outcome}.")
}

/// The entry's classification, reason and text: the first row of design 8.11's table that
/// matches, the most severe with a link error the exchange's own link reported during it, and
/// with `undetermined` when the exchange's answer could belong to an owed request (the log
/// never decides between possible owners).
///
/// The rows are in the table's order. A row that today's evidence cannot reach is named by a
/// comment where it would stand; evidence that only such a row could classify (a late pending
/// before any outcome, a window `timeout_early` or `timeout_late`, an answers list, a final
/// response with a window `late`) takes the last row at the place where that row stands, so a
/// row before it (a refusal, a lost VCI, a failure of the system, a link error) still decides.
pub(crate) fn classify(evidence: &Evidence, history: &LinkHistory) -> Verdict {
    let (mut classification, mut reason) = row(evidence, history);
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
    Verdict {
        classification,
        reason,
        text: text(evidence, history),
    }
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

/// The rows of the table, in its order, for the exchange's own outcome.
fn row(evidence: &Evidence, history: &LinkHistory) -> (Classification, String) {
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
        // Refused by the agent because of an earlier response.
        End::NotSent(cause @ RefusalCause::EarlierResponse) => (
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
        // Not implemented, and standing here: any outcome after a forwarded pending past the
        // limit in force before it (`system`). It needs a judged pending (the transmit time and
        // the limits in force, no producer yet), so a window `late` after a pending takes the
        // last row.
        _ if late && pending_seen => last_row(),
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
        // Not implemented: `vci_did_not_end` with the VCI's own limits past (`system`). Needs the
        // device clock reading at the agent's deadline, the start-of-message indications and
        // the limits in force.
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
        // Not implemented: `no_response` with `window` `timeout_early` or `timeout_late`
        // (`system`). Needs a judged window; evidence with one takes the last row. A window
        // `late` without a pending seen has no row for a missing response either.
        End::NoResponse if timed_out_unjudged || late => last_row(),
        End::NoResponse if suppressed && !this_exchange && !answers_listed => {
            // Not implemented, and standing here: a suppressed no-response that is `none`. It
            // needs `sent` `confirmed`, a matched answer and a judged timeout (or a forwarding
            // link), which no evidence gives together yet.
            let mut hidden = open_readings(evidence, history, sent, true);
            if absorbing && !matches!(window, Window::TimeoutAtLimit | Window::WithinP2) {
                hidden.push("a pending the VCI absorbed may have obliged an answer (H4)");
            }
            if hidden.is_empty() {
                (
                    Undetermined,
                    to_owned(
                        "No response was expected and none came, and no hidden cause (H1 to H4) is open; the log still does not decide, since not every condition that would show that nothing was missed can be checked yet.",
                    ),
                )
            } else {
                (
                    Undetermined,
                    format!("No response was expected, but {}.", hidden.join("; ")),
                )
            }
        }
        // Not implemented: both `answers` rows, and a request meant for one ECU with more than
        // one answer. Needs the answers of a request that collects every answer. Evidence of
        // one, or a window `timeout_early` or `timeout_late` on any outcome, takes the last row.
        _ if answers_listed || timed_out_unjudged => last_row(),
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
        // Not implemented: a final response with `window` `late` (`system`). Needs a judged
        // window; evidence with one takes the last row.
        End::Positive(_) | End::PositiveUnusable(_) | End::Negative(_) if late => last_row(),
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
        End::Positive(_) => {
            let note = if window == Window::NotJudged {
                " Its time is not judged: this rests on the one presumption that the VCI's own timer found it within the limit in force."
            } else {
                ""
            };
            (NoFault, format!("The ECU responded positively.{note}"))
        }
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
        // The agent's own deadline, not the VCI, ended a wait the ECU had extended with a
        // pending: the limit in force may not have run out, and a long transfer or a slow
        // worker can outlast the agent (design 8.11 `vci_did_not_end`).
        End::NoResponse if pending_answer && !evidence.vci_ended => (
            Undetermined,
            to_owned(
                "The ECU answered with a pending, and the agent stopped waiting before the VCI ended the exchange, so the limit in force may not have run out; a long transfer or a slow ECU can outlast the agent's deadline.",
            ),
        ),
        End::NoResponse if pending_answer && !any_link_error => {
            if history.owed_matching == 0 {
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
            (
                Vehicle,
                to_owned(
                    "Sent on the bus; no response within the limit in force, from an ECU that has answered requests matched this way.",
                ),
            )
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

    fn pending() -> Pending {
        Pending {
            vci_us: None,
            clock: None,
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
        let evidence = forwarding(evidence(
            &[0x31, 0x01, 0xFF, 0x00],
            End::Negative(vec![0x7F, 0x31, 0x22]),
        ));
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
            let evidence = forwarding(evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, code])));
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
    fn a_positive_response_on_a_forwarding_link_not_judged_is_none_resting_on_the_presumption() {
        let evidence = forwarding(evidence(
            &READ_F186,
            End::Positive(vec![0x62, 0xF1, 0x86, 1]),
        ));
        let verdict = verdict(&evidence);
        assert_eq!(verdict.classification, Classification::None);
        assert!(verdict.reason.contains("presumption"));
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

    #[test]
    fn a_pending_then_silence_is_vehicle_and_confirms_the_send() {
        let mut evidence = forwarding(evidence(&READ_F186, End::NoResponse));
        evidence.pending = vec![pending()];
        evidence.vci_ended = true;
        let entry = ExchangeEntry::build(3, evidence.clone(), {
            LinkState::default().history_for(&evidence)
        });
        assert_eq!(entry.classification, Classification::Vehicle);
        assert_eq!(entry.sent, Sent::Confirmed);
        assert_eq!(entry.sent_evidence, SentEvidence::Answer);
        assert!(entry.link_history.this_exchange);
        // The agent's own deadline ended the wait, not the VCI: the limit in force may not have
        // run out, so the silence is not the ECU's.
        let mut agent_deadline = evidence.clone();
        agent_deadline.vci_ended = false;
        let verdict = verdict_of(&agent_deadline);
        assert_eq!(verdict.0, Classification::Undetermined);
        assert!(verdict.1.contains("agent stopped waiting"), "{}", verdict.1);
        // A link error during the exchange makes it undetermined and names both.
        evidence.link_errors = vec![LinkErrorKind::Receive];
        let verdict = verdict_of(&evidence);
        assert_eq!(verdict.0, Classification::Undetermined);
        assert!(verdict.1.contains("H3"));
        assert!(verdict.1.contains("receive error"));
    }

    #[test]
    fn a_link_error_during_the_exchange_takes_the_most_severe() {
        let mut evidence = forwarding(evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86])));
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
        let mut evidence = forwarding(evidence_neg(0x22));
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
        // A positive response nobody asked for: vehicle on a forwarding link, undetermined on an
        // absorbing one.
        let mut positive = evidence(&[0x3E, 0x80], End::Positive(vec![0x7E, 0x00]));
        positive.suppressed = true;
        assert_eq!(verdict_of(&positive).0, Classification::Undetermined);
        let positive = forwarding(positive);
        assert_eq!(verdict_of(&positive).0, Classification::Vehicle);
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
    fn what_only_a_row_not_implemented_could_classify_takes_the_last_row() {
        let mut late = forwarding(evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86])));
        late.timing.window = Window::Late;
        let (class, reason) = verdict_of(&late);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("could not decide"));
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
        // The none row for a suppressed no-response is not implemented yet; a later part's
        // classification is stood in for here.
        entry.classification = Classification::None;
        state.record(&entry);
        assert_eq!(state.last_reset(), Some(2));
        let probe = read([0xF1, 0x89], End::Positive(vec![0x62, 0xF1, 0x89]));
        assert_eq!(state.history_for(&probe).owed_matching, 0);
        // As undetermined it ends nothing.
        let mut state = LinkState::default();
        log(&mut state, 1, read([0xF1, 0x89], End::NoResponse));
        let mut reset = evidence(&[0x11, 0x81], End::NoResponse);
        reset.suppressed = true;
        log(&mut state, 2, reset);
        assert_eq!(state.last_reset(), None);
        assert_eq!(state.history_for(&probe).owed_matching, 1);
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
        let mut silent = forwarding(read([0xF1, 0x86], End::NoResponse));
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
            End::NotSent(RefusalCause::EarlierResponse),
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
        full.answers = vec![AnswerItem {
            rx_id: RX,
            response: redact(Direction::Response, &[0x62, 0xF1, 0x86]),
            vci_us: Some(5),
            agent_us: 6,
            response_ms: Some(1.5),
            outcome: Outcome::Positive {
                response: redact(Direction::Response, &[0x62, 0xF1, 0x86]),
            },
            classification: Classification::None,
            reason: "x".to_owned(),
        }];
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
                clock: Clock::Device,
                epoch: 0,
                tx_done_us: Some(1),
                rx_start_us: Some(2),
                rx_end_us: Some(3),
                error_us: Some(4),
            }),
        })
        .unwrap();
        assert_eq!(times["vci"]["clock"], "device");
        let pending = serde_json::to_value(Pending {
            vci_us: Some(1),
            clock: Some(Clock::Worker),
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
        })
        .unwrap();
        assert_eq!(keys(&detail).len(), 7);
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

    /// An answers-list item, for the entries that hold one.
    fn an_answer() -> AnswerItem {
        AnswerItem {
            rx_id: RX,
            response: redact(Direction::Response, &[0x62, 0xF1, 0x86]),
            vci_us: None,
            agent_us: 1,
            response_ms: None,
            outcome: Outcome::NoResponse,
            classification: Classification::None,
            reason: "x".to_owned(),
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
        let mut silent = forwarding(evidence(&READ_F186, End::NoResponse));
        silent.pending = vec![pending()];
        silent.vci_ended = true;
        assert_eq!(verdict_of(&silent).0, Classification::Vehicle);
        silent.channel_link_errors = vec![LinkErrorKind::Receive];
        let (class, reason) = verdict_of(&silent);
        assert_eq!(class, Classification::Undetermined);
        assert!(reason.contains("H3"), "{reason}");
        // The channel's error is evidence only: it adds no fault to an exchange that ended well.
        let mut positive = forwarding(evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86])));
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
        let mut silent = evidence(&[0x3E, 0x80], End::NoResponse);
        silent.suppressed = true;
        silent.transmit_evidence = Some(TransmitEvidence::TxIndication);
        silent.timing.window = Window::TimeoutAtLimit;
        silent.timing.not_judged_reason = None;
        let entry = log(&mut state, 2, silent);
        assert_eq!(entry.link_history.answered_same_match, 1);
        assert_eq!(entry.classification, Classification::Undetermined);
        assert!(!entry.reason.contains("but the evidence does not show"));
        assert!(entry.reason.contains("no hidden cause"), "{}", entry.reason);
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
        // The rows after it still take the last row: a silence with a window `timeout_late`, an
        // answers list on a final response, a final response with a window `late`.
        for case in [
            {
                let mut e = evidence(&READ_F186, End::NoResponse);
                e.timing.window = Window::TimeoutLate;
                e
            },
            {
                let mut e = evidence(&READ_F186, End::Positive(vec![0x62, 0xF1, 0x86]));
                e.answers = vec![an_answer()];
                e
            },
            {
                let mut e = evidence(&READ_F186, End::Negative(vec![0x7F, 0x22, 0x22]));
                e.timing.window = Window::Late;
                e
            },
        ] {
            let (class, reason) = verdict_of(&case);
            assert_eq!(class, Classification::Undetermined);
            assert!(reason.contains("could not decide"), "{reason}");
        }
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
}

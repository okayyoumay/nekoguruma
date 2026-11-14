//! Protocol types shared by the server, agent, and worker.
//!
//! Design document sections: 5.1 job kinds / 5.3 handling disconnects / 9.5 capabilities /
//! 11.1 job instructions / 5.2 control channel messages (ADR-466)
//!
//! These types are also used for worker IPC, so only fixed-width types are used.
//! `usize` / `isize` / `c_long` must not appear in public types (7.4).

use serde::{Deserialize, Serialize};

mod control;
pub use control::{
    AgentEventKind, AgentEventMessage, AgentMessage, CLOSE_CODE_BINARY, CLOSE_CODE_GOING_AWAY,
    CLOSE_CODE_IDLE, CLOSE_CODE_PROTOCOL, CLOSE_CODE_REPLACED, CLOSE_CODE_REVOKED,
    CLOSE_CODE_TOO_BIG, CLOSE_CODES, CONTROL_SUBPROTOCOL, Checkpoint, ControlError,
    DEFAULT_HEARTBEAT_SECS, Delivery, HEARTBEAT_MISSED_LIMIT, HmiRequestBody,
    MAX_CONTROL_MESSAGE_LEN, MAX_HEARTBEAT_SECS, MAX_PROGRESS_PERMILLE, MIN_HEARTBEAT_SECS,
    OpenJob, ServerMessage, close_reason,
};
mod instruction;
pub use instruction::{
    INSTRUCTION_VERSION, InstructionError, InstructionRejectReason, JOB_ID_LEN, JobInstruction,
    MAX_INSTRUCTION_LEN, MAX_STRING_LEN, is_canonical_uuid,
};

pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------- Identifiers

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub String); // UUIDv7

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VciId(pub String); // Stable identifier from discovery results

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Vin(pub String);

// ---------------------------------------------------------------- Jobs

/// Job kinds from 5.1.
///
/// The job instruction carries the kind in its postcard encoding, which numbers the variants
/// in declaration order: a new kind is a new body version, added at the end, and the order
/// never changes (ADR-461).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobKind {
    Acquire,
    MonitorSession,
    MonitorCapture,
    RunSequence,
    WriteSettings,
    Reprogram,
    /// Vehicle identification (design 16.3, ADR-327).
    Identify,
}

// ---------------------------------------------------------------- Events

/// Agent -> server. Delivery is guaranteed via a sequence-numbered Outbox (5.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub agent_id: AgentId,
    pub seq: u64,
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventBody {
    /// Must-deliver event.
    JobStateChanged {
        job_id: JobId,
        state: JobState,
    },
    /// Event that may be thinned out (5.4).
    Progress {
        job_id: JobId,
        permille: u16,
        phase: String,
    },
    /// HMI request (8.6). The response comes back as a command.
    HmiRequest {
        job_id: JobId,
        request_id: String,
        form: String,
    },
    Capabilities(Box<Capabilities>),
}

/// Corresponds to the state transitions in 5.6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Received,
    Precheck,
    Ready,
    Running,
    Verifying,
    AwaitingHmi,
    CancelRequested,
    Completed,
    /// `procedure_code` is the code the procedure gave when it ended itself as failed, with
    /// `code` [`FailureCode::Procedure`]: the procedure's code travels apart from the agent's
    /// own, so it never reads as one (5.6, ADR-475). It is 1 to [`MAX_PROCEDURE_CODE_LEN`]
    /// bytes of UTF-8 and is `None` for every other failure.
    Failed {
        code: FailureCode,
        detail: String,
        procedure_code: Option<String>,
    },
    Cancelled,
    Expired,
    PreconditionFailed {
        code: FailureCode,
        detail: String,
    },
    NeedsOnSiteRecovery {
        detail: String,
    },
}

/// The longest procedure code a `jobState` event carries (5.6): the bound `diag.fail`'s code
/// has in the IR.
pub const MAX_PROCEDURE_CODE_LEN: usize = 128;

impl JobState {
    /// The state's name in camelCase, as the control channel's `jobState` event spells it (5.2).
    pub fn wire_name(&self) -> &'static str {
        match self {
            JobState::Received => "received",
            JobState::Precheck => "precheck",
            JobState::Ready => "ready",
            JobState::Running => "running",
            JobState::Verifying => "verifying",
            JobState::AwaitingHmi => "awaitingHmi",
            JobState::CancelRequested => "cancelRequested",
            JobState::Completed => "completed",
            JobState::Failed { .. } => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Expired => "expired",
            JobState::PreconditionFailed { .. } => "preconditionFailed",
            JobState::NeedsOnSiteRecovery { .. } => "needsOnSiteRecovery",
        }
    }

    /// The state as a `jobState` event of `job_id` for the control channel (5.2, 5.6).
    pub fn to_event_kind(&self, job_id: JobId) -> AgentEventKind {
        let (failure_code, procedure_code, detail) = match self {
            JobState::Failed {
                code,
                detail,
                procedure_code,
            } => (Some(code), procedure_code.clone(), Some(detail.clone())),
            JobState::PreconditionFailed { code, detail } => {
                (Some(code), None, Some(detail.clone()))
            }
            JobState::NeedsOnSiteRecovery { detail } => (None, None, Some(detail.clone())),
            _ => (None, None, None),
        };
        AgentEventKind::JobState {
            job_id,
            state: self.wire_name().to_owned(),
            failure_code: failure_code.map(|code| code.wire_name().to_owned()),
            procedure_code,
            detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureCode {
    DeviceLocked,
    VehicleLocked,
    VinMismatch,
    VariantMismatch,
    PowerCondition,
    SignatureInvalid,
    LibraryLoadFailed,
    UnsupportedAbi,
    Runtime32BitMissing,
    CpuOrKernelNo32Bit,
    GatewayAuthExpired,
    TicketExpired,
    /// The procedure ended itself as failed (`diag.fail`); its own code is the `Failed`
    /// state's `procedure_code` (5.6, ADR-475).
    Procedure,
    /// The procedure ran into an error of its own: the VM refused an instruction, or the run
    /// reached its step limit (5.6, ADR-475).
    ProcedureError,
    /// The agent refused the job's data or program before sending anything (5.6, ADR-475).
    JobRefused,
    /// The link to the vehicle could not be opened (5.6, ADR-475).
    LinkNotOpened,
    /// An exchange with the vehicle failed. Asserts neither a fault of the vehicle nor of the
    /// diagnostic system; the communication log tells which (8.11, ADR-375, ADR-475).
    ExchangeFailed,
    /// A fault in the agent itself. The only code that asserts one on the diagnostic system's
    /// side (5.6, ADR-475).
    Internal,
    Other,
}

impl FailureCode {
    /// The code's name in camelCase, as the control channel's `failureCode` spells it (5.2).
    pub fn wire_name(&self) -> &'static str {
        match self {
            FailureCode::DeviceLocked => "deviceLocked",
            FailureCode::VehicleLocked => "vehicleLocked",
            FailureCode::VinMismatch => "vinMismatch",
            FailureCode::VariantMismatch => "variantMismatch",
            FailureCode::PowerCondition => "powerCondition",
            FailureCode::SignatureInvalid => "signatureInvalid",
            FailureCode::LibraryLoadFailed => "libraryLoadFailed",
            FailureCode::UnsupportedAbi => "unsupportedAbi",
            FailureCode::Runtime32BitMissing => "runtime32BitMissing",
            FailureCode::CpuOrKernelNo32Bit => "cpuOrKernelNo32Bit",
            FailureCode::GatewayAuthExpired => "gatewayAuthExpired",
            FailureCode::TicketExpired => "ticketExpired",
            FailureCode::Procedure => "procedure",
            FailureCode::ProcedureError => "procedureError",
            FailureCode::JobRefused => "jobRefused",
            FailureCode::LinkNotOpened => "linkNotOpened",
            FailureCode::ExchangeFailed => "exchangeFailed",
            FailureCode::Internal => "internal",
            FailureCode::Other => "other",
        }
    }
}

// ---------------------------------------------------------------- capabilities

/// 9.5. The server uses this to decide whether delivery is possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub agent_version: String,
    pub protocol_version: u32,
    pub platform: Platform,
    pub workers: Vec<WorkerAbi>,
    pub vcis: Vec<VciStatus>,
    pub ir_schema_versions: Vec<u32>,
    pub extension_schema_versions: Vec<u32>,
    pub monitor_direct: bool,
    pub offline_capable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,          // "windows" | "linux"
    pub native_arch: String, // Native architecture, obtained via IsWow64Process2 etc.
    pub emulated: bool,
    pub mode: AgentMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMode {
    User,
    Device,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerAbi {
    pub abi: String, // "win-x64" | "win-x86" | "linux-x86_64" | ...
    pub available: bool,
    pub reason: Option<FailureCode>, // Reason code when unavailable
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VciStatus {
    pub vci: VciId,
    pub name: String,
    pub vendor: String,
    pub standard: VciStandard,
    /// ABI interpretation category from 7.1.2.
    pub abi_interpretation: AbiInterpretation,
    pub loadable: bool,
    pub reason: Option<FailureCode>,
    pub signature_verifier: SignatureVerifier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VciStandard {
    J2534 { version: String },
    DPduApi { version: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AbiInterpretation {
    /// Defined by the standard (J2534 on Windows).
    Standard,
    /// Defined by this software (J2534 on Linux; 7.1.1).
    ProductDefined,
    /// Not specified by the standard; inferred (ARM; 7.1.2).
    Inferred,
}

/// 11.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignatureVerifier {
    Library,
    Adapter,
    EcuOnly,
}

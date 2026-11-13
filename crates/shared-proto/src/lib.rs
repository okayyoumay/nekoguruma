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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobState {
    Received,
    Precheck,
    Ready,
    Running,
    Verifying,
    AwaitingHmi,
    CancelRequested,
    Completed,
    Failed { code: FailureCode, detail: String },
    Cancelled,
    Expired,
    PreconditionFailed { code: FailureCode },
    NeedsOnSiteRecovery { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Other,
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

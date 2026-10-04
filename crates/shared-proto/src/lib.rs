//! Protocol types shared by the server, agent, and worker.
//!
//! Design document sections: 5.1 job kinds / 5.3 handling disconnects / 9.5 capabilities
//!
//! These types are also used for worker IPC, so only fixed-width types are used.
//! `usize` / `isize` / `c_long` must not appear in public types (7.4).

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobKind {
    Acquire,
    MonitorSession,
    MonitorCapture,
    RunSequence,
    WriteSettings,
    Reprogram,
}

/// Job instruction signed and delivered by the server (layer 3 of 11.1).
/// The signature covers the raw bytes of `payload` exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedJob {
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
    pub key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub job_id: JobId,
    pub kind: JobKind,
    pub agent_id: AgentId,
    pub operator_id: String,
    pub vci: VciId,
    /// Target vehicle. None before discovery (two-stage locking, 8.8).
    pub vin: Option<Vin>,
    /// Hash of the IR package to use.
    pub ir_digest: String,
    /// Hash of the ECU distribution artifact (write operations only).
    pub artifact_digest: Option<String>,
    /// Start deadline (RFC3339). If exceeded, the job is not run and Expired is reported (5.3).
    pub start_deadline: String,
    /// Confirmation requirement (6.3).
    pub confirmation: Confirmation,
    pub min_agent_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Confirmation {
    NotifyOnly,
    GracePeriod { seconds: u32 },
    OnSiteApproval,
    TwoPerson { approver_id: String },
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
    Machine,
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

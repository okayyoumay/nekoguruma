//! IR schema, bytecode definitions and VM. Shared by the server and the agent (8.2).
//!
//! - Declaration part: FlatBuffers (see the separate .fbs file). This file handles the procedure part.
//! - Procedure part: custom bytecode VM. The entire execution state must be serializable (8.2.3).

use serde::{Deserialize, Serialize};

pub const IR_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------- Instructions

/// 8.2.4. No instructions are defined that amount to external access (files, network, process
/// spawning, dynamic code generation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    // Stack
    PushI64(i64),
    PushF64(f64),
    PushBytes(u32), // index into the constant pool
    Pop,
    Dup,
    Swap,

    // Arithmetic, logic, comparison
    Add,
    Sub,
    Mul,
    Div,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    CmpEq,
    CmpLt,
    CmpGt,
    Not,

    // Control
    Jump(u32),
    JumpIfFalse(u32),
    Call(u32),
    Ret,

    // Variables
    LoadLocal(u16),
    StoreLocal(u16),
    LoadGlobal(u16),
    StoreGlobal(u16),
    IndexGet,
    IndexSet,

    // Diagnostic primitives (8.2.4)
    ServiceRequest { service: u16 },
    ReadDtc { mask: u8 },
    RoutineControl { routine: u16, sub: u8 },
    SecurityAccess { level: u8 },
    FlashTransfer { block: u32 },
    Wait { millis: u32 },
    HmiRequest { form: u32 },
    RecordInput { template: u32 },       // 4.3.1 record template
    MonitorCapture { back_millis: u32 }, // 4.6 capture
    Log { level: u8, message: u32 },
}

// ---------------------------------------------------------------- Program

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Program {
    pub schema_version: u32,
    pub code: Vec<Op>,
    pub constants: Vec<Vec<u8>>,
    pub sections: Vec<Section>,
    /// 8.4: mapping table from bytecode back to the original source.
    pub source_map: Vec<SourceSpan>,
}

/// Interruptibility sections from 8.10.1. Expressed with the same mechanism as the idempotency attribute of the resume model (8.2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub start_pc: u32,
    pub end_pc: u32,
    pub interruptible: Interruptible,
    pub idempotency: Idempotency,
    /// Expected duration. Compared against the remaining OEM authentication time (8.10.1).
    pub expected_millis: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Interruptible {
    Yes,
    No,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Idempotency {
    /// Safe to re-execute.
    Safe,
    /// Query the state before deciding.
    CheckState,
    /// Must not be re-executed.
    Unsafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpan {
    pub pc: u32,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

// ---------------------------------------------------------------- Execution state

/// Written to the journal in its entirety (8.2.5). Serialized with postcard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub schema_version: u32,
    pub pc: u32,
    pub stack: Vec<Value>,
    pub locals: Vec<Value>,
    pub globals: Vec<Value>,
    pub call_stack: Vec<u32>,
    /// Most recently completed checkpoint. Resumption starts here, after a state check.
    pub checkpoint: Option<Checkpoint>,
    pub resume_count: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub pc: u32,
    pub section: u32,
    /// Version and target being written. A VIN match is required before resuming (8.2.5).
    pub vin: Option<String>,
    pub artifact_digest: Option<String>,
    pub at: String, // RFC3339 (correspondence with the monotonic clock is recorded separately)
}

// ---------------------------------------------------------------- Host boundary

/// The only path from the VM to the outside. Implementations are swapped between the agent (real hardware) and tests (mocks).
/// Operations not listed here cannot be executed from the VM.
pub trait DiagHost {
    type Error;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, Self::Error>;
    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, Self::Error>;
    /// The seed is sent to the server and a key is received (8.10). Fails when offline.
    fn security_access(&mut self, level: u8, seed: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), Self::Error>;
    fn wait(&mut self, millis: u32) -> Result<(), Self::Error>;
    fn hmi_request(&mut self, form: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn record_input(&mut self, template: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), Self::Error>;
    fn log(&mut self, level: u8, message: &str);
    /// Called when a checkpoint is reached. Writes to the journal.
    fn checkpoint(&mut self, state: &VmState) -> Result<(), Self::Error>;
}

pub struct Vm {
    pub state: VmState,
}

impl Vm {
    pub fn new(program: &Program) -> Self {
        Self {
            state: VmState {
                schema_version: program.schema_version,
                pc: 0,
                stack: Vec::new(),
                locals: Vec::new(),
                globals: Vec::new(),
                call_stack: Vec::new(),
                checkpoint: None,
                resume_count: 0,
            },
        }
    }

    /// Resume from an interrupted journal.
    pub fn resume(state: VmState) -> Self {
        Self { state }
    }

    pub fn step<H: DiagHost>(
        &mut self,
        _program: &Program,
        _host: &mut H,
    ) -> Result<StepOutcome, VmError> {
        // TODO: instruction dispatch. Return after each instruction so the caller can evaluate
        // interruption requests and section attributes (Section).
        todo!("implement instruction dispatch")
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StepOutcome {
    Continue,
    Finished,
    /// Waiting for HMI response. Keeps waiting even while disconnected (5.6).
    AwaitingHmi,
}

#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("schema version mismatch: program={program}, runtime={runtime}")]
    SchemaMismatch { program: u32, runtime: u32 },
    #[error("invalid instruction position: {0}")]
    BadPc(u32),
    #[error("stack underflow")]
    StackUnderflow,
    #[error("resume count limit reached")]
    ResumeLimit,
}

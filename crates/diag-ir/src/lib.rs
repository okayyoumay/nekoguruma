//! IR schema, bytecode definitions and VM. Shared by the server and the agent (8.2).
//!
//! - Declaration part: FlatBuffers (see the separate .fbs file). This file handles the procedure part.
//! - Procedure part: custom bytecode VM. The entire execution state must be serializable (8.2.3).

use serde::{Deserialize, Serialize};

pub const IR_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------- Instructions

/// 8.2.4. No instructions are defined that amount to external access (files, network, process
/// spawning, dynamic code generation).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl Op {
    /// Whether this instruction goes through [`DiagHost`]. The job runner journals around these
    /// (8.2.5 checkpoint granularity, ADR-233).
    pub fn is_diagnostic_primitive(&self) -> bool {
        matches!(
            self,
            Op::ServiceRequest { .. }
                | Op::ReadDtc { .. }
                | Op::RoutineControl { .. }
                | Op::SecurityAccess { .. }
                | Op::FlashTransfer { .. }
                | Op::Wait { .. }
                | Op::HmiRequest { .. }
                | Op::RecordInput { .. }
                | Op::MonitorCapture { .. }
                | Op::Log { .. }
        )
    }
}

// ---------------------------------------------------------------- Execution state

/// Maximum operand stack depth. A program that exceeds it fails with [`VmError::StackOverflow`].
pub const MAX_STACK: usize = 1024;
/// Maximum subroutine nesting. A program that exceeds it fails with [`VmError::CallDepthExceeded`].
pub const MAX_CALL_DEPTH: usize = 64;

/// Written to the journal in its entirety (8.2.5). Serialized with postcard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmState {
    pub schema_version: u32,
    pub pc: u32,
    pub stack: Vec<Value>,
    /// Locals of the running subroutine (or of the top level). `None` is an unset slot.
    pub locals: Vec<Option<Value>>,
    pub globals: Vec<Option<Value>>,
    pub call_stack: Vec<Frame>,
    /// Instructions completed so far. Survives resumption; used for audit and for step limits
    /// the job runner may enforce.
    pub steps: u64,
    /// Most recently completed checkpoint. Resumption starts here, after a state check.
    pub checkpoint: Option<Checkpoint>,
    pub resume_count: u16,
}

/// A suspended caller: where to return to and its locals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub return_pc: u32,
    pub locals: Vec<Option<Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    I64(i64),
    F64(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
///
/// Journaling is not part of this trait: the job runner writes checkpoints and intent markers
/// around [`Vm::step`] (ADR-233).
pub trait DiagHost {
    type Error: std::error::Error + Send + Sync + 'static;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, Self::Error>;
    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, Self::Error>;
    /// The seed is sent to the server and a key is received (8.10). `Ok(None)` means the key has
    /// not arrived yet (for example while the server is unreachable, 8.2.5); the VM then waits and
    /// the same request is made again on the next step. The host keeps track of what it already
    /// asked.
    fn security_access(&mut self, level: u8, seed: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), Self::Error>;
    fn wait(&mut self, millis: u32) -> Result<(), Self::Error>;
    /// `Ok(None)` means no answer yet; see [`DiagHost::security_access`].
    fn hmi_request(&mut self, form: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
    fn record_input(&mut self, template: &[u8]) -> Result<Vec<u8>, Self::Error>;
    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), Self::Error>;
    fn log(&mut self, level: u8, message: &str);
}

// ---------------------------------------------------------------- VM

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
                steps: 0,
                checkpoint: None,
                resume_count: 0,
            },
        }
    }

    /// Resume from an interrupted journal.
    pub fn resume(state: VmState) -> Self {
        Self { state }
    }

    /// The instruction the next [`Vm::step`] executes, or `None` when the program has finished.
    /// The job runner uses it to journal an intent marker before a primitive is sent (ADR-229).
    pub fn current_op<'p>(&self, program: &'p Program) -> Result<Option<&'p Op>, VmError> {
        let pc = self.state.pc as usize;
        match pc.cmp(&program.code.len()) {
            std::cmp::Ordering::Less => Ok(Some(&program.code[pc])),
            std::cmp::Ordering::Equal => Ok(None),
            std::cmp::Ordering::Greater => Err(VmError::BadPc(self.state.pc)),
        }
    }

    /// Executes one instruction (ADR-233).
    ///
    /// A step is atomic: it either completes (operands consumed, results pushed, `pc` advanced,
    /// `steps` incremented) or leaves the state unchanged. Everything the VM can check is checked
    /// before the host is called, so a host answer is never discarded. After a host error the
    /// state still points at the primitive, so stepping again repeats it (8.2.5); whether that
    /// is safe is the job runner's decision (`Section::idempotency`, ADR-229), not the VM's.
    pub fn step<H: DiagHost>(
        &mut self,
        program: &Program,
        host: &mut H,
    ) -> Result<StepOutcome, StepError<H::Error>> {
        if program.schema_version != IR_SCHEMA_VERSION {
            return Err(VmError::SchemaMismatch {
                program: program.schema_version,
                runtime: IR_SCHEMA_VERSION,
            }
            .into());
        }
        if self.state.schema_version != program.schema_version {
            return Err(VmError::SchemaMismatch {
                program: program.schema_version,
                runtime: self.state.schema_version,
            }
            .into());
        }
        let Some(op) = self.current_op(program)? else {
            return Ok(StepOutcome::Finished);
        };
        match self.execute(op, program, host)? {
            Executed::Done => {
                self.state.steps += 1;
                if self.state.pc as usize == program.code.len() {
                    Ok(StepOutcome::Finished)
                } else {
                    Ok(StepOutcome::Continue)
                }
            }
            Executed::Waiting(on) => Ok(StepOutcome::Waiting(on)),
        }
    }

    fn execute<H: DiagHost>(
        &mut self,
        op: &Op,
        program: &Program,
        host: &mut H,
    ) -> Result<Executed, StepError<H::Error>> {
        let next = self.state.pc + 1;
        match op {
            Op::PushI64(v) => self.push(Value::I64(*v))?,
            Op::PushF64(v) => self.push(Value::F64(*v))?,
            Op::PushBytes(index) => {
                let bytes = constant(program, *index)?.to_vec();
                self.push(Value::Bytes(bytes))?;
            }
            Op::Pop => {
                self.top(1)?;
                self.state.stack.pop();
            }
            Op::Dup => {
                let value = self.top(1)?[0].clone();
                self.push(value)?;
            }
            Op::Swap => {
                self.top(2)?;
                let len = self.state.stack.len();
                self.state.stack.swap(len - 1, len - 2);
            }

            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                let result = arithmetic(op, self.top(2)?)?;
                self.replace(2, result);
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => {
                let result = bitwise(op, self.top(2)?)?;
                self.replace(2, result);
            }
            Op::CmpEq | Op::CmpLt | Op::CmpGt => {
                let result = compare(op, self.top(2)?)?;
                self.replace(2, Value::Bool(result));
            }
            Op::Not => {
                let Value::Bool(b) = self.top(1)?[0] else {
                    return Err(VmError::TypeMismatch.into());
                };
                self.replace(1, Value::Bool(!b));
            }

            Op::Jump(target) => {
                self.state.pc = jump_target(program, *target)?;
                return Ok(Executed::Done);
            }
            Op::JumpIfFalse(target) => {
                let target = jump_target(program, *target)?;
                let Value::Bool(condition) = self.top(1)?[0] else {
                    return Err(VmError::TypeMismatch.into());
                };
                self.state.stack.pop();
                self.state.pc = if condition { next } else { target };
                return Ok(Executed::Done);
            }
            Op::Call(target) => {
                let target = jump_target(program, *target)?;
                if self.state.call_stack.len() >= MAX_CALL_DEPTH {
                    return Err(VmError::CallDepthExceeded.into());
                }
                let locals = std::mem::take(&mut self.state.locals);
                self.state.call_stack.push(Frame {
                    return_pc: next,
                    locals,
                });
                self.state.pc = target;
                return Ok(Executed::Done);
            }
            Op::Ret => {
                match self.state.call_stack.pop() {
                    Some(frame) => {
                        self.state.locals = frame.locals;
                        self.state.pc = frame.return_pc;
                    }
                    // A return from the top level ends the program.
                    None => self.state.pc = program.code.len() as u32,
                }
                return Ok(Executed::Done);
            }

            Op::LoadLocal(slot) => {
                let value = load(&self.state.locals, *slot)?;
                self.push(value)?;
            }
            Op::StoreLocal(slot) => {
                self.top(1)?;
                let value = self.state.stack.pop().expect("checked by top");
                store(&mut self.state.locals, *slot, value);
            }
            Op::LoadGlobal(slot) => {
                let value = load(&self.state.globals, *slot)?;
                self.push(value)?;
            }
            Op::StoreGlobal(slot) => {
                self.top(1)?;
                let value = self.state.stack.pop().expect("checked by top");
                store(&mut self.state.globals, *slot, value);
            }
            Op::IndexGet => {
                let [Value::Bytes(bytes), Value::I64(index)] = self.top(2)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let byte = *bytes
                    .get(byte_index(*index, bytes.len())?)
                    .expect("checked by byte_index");
                self.replace(2, Value::I64(byte.into()));
            }
            Op::IndexSet => {
                let [Value::Bytes(bytes), Value::I64(index), Value::I64(value)] = self.top(3)?
                else {
                    return Err(VmError::TypeMismatch.into());
                };
                let index = byte_index(*index, bytes.len())?;
                let value = u8::try_from(*value).map_err(|_| VmError::ByteOutOfRange(*value))?;
                self.state.stack.truncate(self.state.stack.len() - 2);
                let Some(Value::Bytes(bytes)) = self.state.stack.last_mut() else {
                    unreachable!("checked by top");
                };
                bytes[index] = value;
            }

            Op::ServiceRequest { service } => {
                let payload = self.bytes_operand()?;
                self.check_room(1, 1)?;
                let response = host
                    .service_request(*service, payload)
                    .map_err(StepError::Host)?;
                self.replace(1, Value::Bytes(response));
            }
            Op::ReadDtc { mask } => {
                self.check_room(0, 1)?;
                let response = host.read_dtc(*mask).map_err(StepError::Host)?;
                self.push(Value::Bytes(response))?;
            }
            Op::RoutineControl { routine, sub } => {
                let payload = self.bytes_operand()?;
                self.check_room(1, 1)?;
                let response = host
                    .routine_control(*routine, *sub, payload)
                    .map_err(StepError::Host)?;
                self.replace(1, Value::Bytes(response));
            }
            Op::SecurityAccess { level } => {
                let seed = self.bytes_operand()?;
                self.check_room(1, 1)?;
                match host
                    .security_access(*level, seed)
                    .map_err(StepError::Host)?
                {
                    Some(key) => self.replace(1, Value::Bytes(key)),
                    None => return Ok(Executed::Waiting(WaitingOn::SeedKey)),
                }
            }
            Op::FlashTransfer { block } => {
                let data = self.bytes_operand()?;
                host.flash_transfer(*block, data).map_err(StepError::Host)?;
                self.state.stack.pop();
            }
            Op::Wait { millis } => host.wait(*millis).map_err(StepError::Host)?,
            Op::HmiRequest { form } => {
                let form = constant(program, *form)?;
                self.check_room(0, 1)?;
                match host.hmi_request(form).map_err(StepError::Host)? {
                    Some(answer) => self.push(Value::Bytes(answer))?,
                    None => return Ok(Executed::Waiting(WaitingOn::Hmi)),
                }
            }
            Op::RecordInput { template } => {
                let template = constant(program, *template)?;
                self.check_room(0, 1)?;
                let input = host.record_input(template).map_err(StepError::Host)?;
                self.push(Value::Bytes(input))?;
            }
            Op::MonitorCapture { back_millis } => host
                .monitor_capture(*back_millis)
                .map_err(StepError::Host)?,
            Op::Log { level, message } => {
                let message = std::str::from_utf8(constant(program, *message)?)
                    .map_err(|_| VmError::InvalidUtf8(*message))?;
                host.log(*level, message);
            }
        }
        self.state.pc = next;
        Ok(Executed::Done)
    }

    /// The top `n` values, deepest first, or [`VmError::StackUnderflow`].
    fn top(&self, n: usize) -> Result<&[Value], VmError> {
        let len = self.state.stack.len();
        if len < n {
            return Err(VmError::StackUnderflow);
        }
        Ok(&self.state.stack[len - n..])
    }

    /// The top value as bytes, for primitives that send it.
    fn bytes_operand(&self) -> Result<&[u8], VmError> {
        match &self.top(1)?[0] {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(VmError::TypeMismatch),
        }
    }

    /// Checks that popping `pop` values and pushing `push` stays within [`MAX_STACK`].
    fn check_room(&self, pop: usize, push: usize) -> Result<(), VmError> {
        if self.state.stack.len() - pop + push > MAX_STACK {
            return Err(VmError::StackOverflow);
        }
        Ok(())
    }

    fn push(&mut self, value: Value) -> Result<(), VmError> {
        self.check_room(0, 1)?;
        self.state.stack.push(value);
        Ok(())
    }

    /// Replaces the top `n` values (already checked to exist) with `value`.
    fn replace(&mut self, n: usize, value: Value) {
        let len = self.state.stack.len();
        self.state.stack.truncate(len - n);
        self.state.stack.push(value);
    }
}

enum Executed {
    Done,
    Waiting(WaitingOn),
}

fn constant(program: &Program, index: u32) -> Result<&[u8], VmError> {
    program
        .constants
        .get(index as usize)
        .map(Vec::as_slice)
        .ok_or(VmError::BadConstant(index))
}

/// A jump or call target; `code.len()` is allowed and ends the program.
fn jump_target(program: &Program, target: u32) -> Result<u32, VmError> {
    if target as usize > program.code.len() {
        return Err(VmError::BadPc(target));
    }
    Ok(target)
}

fn load(slots: &[Option<Value>], slot: u16) -> Result<Value, VmError> {
    slots
        .get(slot as usize)
        .cloned()
        .flatten()
        .ok_or(VmError::UndefinedVariable(slot))
}

fn store(slots: &mut Vec<Option<Value>>, slot: u16, value: Value) {
    let slot = slot as usize;
    if slots.len() <= slot {
        slots.resize(slot + 1, None);
    }
    slots[slot] = Some(value);
}

fn byte_index(index: i64, len: usize) -> Result<usize, VmError> {
    usize::try_from(index)
        .ok()
        .filter(|&i| i < len)
        .ok_or(VmError::IndexOutOfRange(index))
}

/// Checked integer arithmetic, IEEE floating point; both operands of the same type.
fn arithmetic(op: &Op, operands: &[Value]) -> Result<Value, VmError> {
    match operands {
        [Value::I64(a), Value::I64(b)] => {
            let result = match op {
                Op::Add => a.checked_add(*b),
                Op::Sub => a.checked_sub(*b),
                Op::Mul => a.checked_mul(*b),
                Op::Div if *b == 0 => return Err(VmError::DivisionByZero),
                Op::Div => a.checked_div(*b),
                _ => unreachable!("not an arithmetic op"),
            };
            result.map(Value::I64).ok_or(VmError::Overflow)
        }
        [Value::F64(a), Value::F64(b)] => Ok(Value::F64(match op {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div => a / b,
            _ => unreachable!("not an arithmetic op"),
        })),
        _ => Err(VmError::TypeMismatch),
    }
}

/// Bitwise operations on the 64-bit pattern; shifts are logical and take 0 to 63.
fn bitwise(op: &Op, operands: &[Value]) -> Result<Value, VmError> {
    let [Value::I64(a), Value::I64(b)] = operands else {
        return Err(VmError::TypeMismatch);
    };
    let shift = || {
        u32::try_from(*b)
            .ok()
            .filter(|&s| s < 64)
            .ok_or(VmError::ShiftOutOfRange(*b))
    };
    let bits = *a as u64;
    Ok(Value::I64(match op {
        Op::BitAnd => a & b,
        Op::BitOr => a | b,
        Op::BitXor => a ^ b,
        Op::Shl => (bits << shift()?) as i64,
        Op::Shr => (bits >> shift()?) as i64,
        _ => unreachable!("not a bitwise op"),
    }))
}

/// Equality on any two values of the same type (floating point by IEEE rules, so NaN is not
/// equal to itself); ordering on numbers only.
fn compare(op: &Op, operands: &[Value]) -> Result<bool, VmError> {
    match (op, operands) {
        (Op::CmpEq, [a, b]) if std::mem::discriminant(a) == std::mem::discriminant(b) => Ok(a == b),
        (Op::CmpLt, [Value::I64(a), Value::I64(b)]) => Ok(a < b),
        (Op::CmpLt, [Value::F64(a), Value::F64(b)]) => Ok(a < b),
        (Op::CmpGt, [Value::I64(a), Value::I64(b)]) => Ok(a > b),
        (Op::CmpGt, [Value::F64(a), Value::F64(b)]) => Ok(a > b),
        _ => Err(VmError::TypeMismatch),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    Continue,
    Finished,
    /// Waiting for an answer from outside; the state is unchanged and the caller steps again
    /// later. Keeps waiting even while disconnected (5.6, 8.2.5).
    Waiting(WaitingOn),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingOn {
    /// An HMI request has not been answered.
    Hmi,
    /// The key for a security access seed has not arrived from the server.
    SeedKey,
}

#[derive(Debug, thiserror::Error)]
pub enum StepError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Vm(#[from] VmError),
    #[error("host: {0}")]
    Host(#[source] E),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VmError {
    #[error("schema version mismatch: program={program}, runtime={runtime}")]
    SchemaMismatch { program: u32, runtime: u32 },
    #[error("invalid instruction position: {0}")]
    BadPc(u32),
    #[error("stack underflow")]
    StackUnderflow,
    #[error("stack overflow")]
    StackOverflow,
    #[error("call depth exceeded")]
    CallDepthExceeded,
    #[error("operand type mismatch")]
    TypeMismatch,
    #[error("integer overflow")]
    Overflow,
    #[error("division by zero")]
    DivisionByZero,
    #[error("shift amount out of range: {0}")]
    ShiftOutOfRange(i64),
    #[error("invalid constant index: {0}")]
    BadConstant(u32),
    #[error("constant {0} is not valid UTF-8")]
    InvalidUtf8(u32),
    #[error("variable slot {0} is not set")]
    UndefinedVariable(u16),
    #[error("index out of range: {0}")]
    IndexOutOfRange(i64),
    #[error("byte value out of range: {0}")]
    ByteOutOfRange(i64),
    #[error("resume count limit reached")]
    ResumeLimit,
}

#[cfg(test)]
mod tests;

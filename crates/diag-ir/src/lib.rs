//! IR schema, bytecode definitions and VM. Shared by the server and the agent (8.2).
//!
//! - Declaration part: FlatBuffers (`schema/ir.fbs`), read through [`declaration`]; responses
//!   are decoded with it by [`decode`], and request operands encoded by [`encode`]. This file handles the procedure part.
//! - Procedure part: custom bytecode VM. The entire execution state must be serializable (8.2.3).

// The FlatBuffers bindings of the declaration part name `alloc::` paths (`declaration`).
extern crate alloc;

pub mod declaration;
pub mod decode;
pub mod encode;
mod recovery;

pub use recovery::*;
use serde::{Deserialize, Serialize};

use crate::declaration::EcuVariant;

/// 2: `Program` gained the restart declaration (`identity`, `preconditions`, `flash`, ADR-245)
/// and `VmState` lost `checkpoint` and `resume_count`, so both postcard encodings changed.
/// 3: `Op` gained `Fail` (ADR-280). Programs of version 2 encode the same, but a runtime of
/// version 2 cannot decode one that uses `Fail`, so the version a server matches against the
/// agent's `capabilities` (9.5) changes.
/// 4: `Op` gained `Length`, for the same reason (ADR-233).
/// 5: `Op` gained `Rem`, for the same reason.
/// 6: `Op` gained `Neg`, for the same reason.
/// 7: `Op` gained `Concat`, for the same reason; `VmError` gained `BytesTooLong`.
/// 8: `Op` gained `Slice`, for the same reason.
/// 9: `Op` gained `IntToFloat` and `FloatToInt`, for the same reason; `VmError` gained
/// `FloatNotConvertible`.
/// 10: `VariantPattern` gained `pattern`, which groups a variant's matching parameters into
/// patterns (ADR-315); a document of version 9 would read every entry as one pattern.
/// 11: `Op` gained `DecodeField`, and `VmError` gained `NoDeclarations`, `UnknownField`,
/// `ShapedField` and `Undecodable` (ADR-316).
pub const IR_SCHEMA_VERSION: u32 = 11;

/// The longest code, in bytes, that `Program::validate` accepts for a `Fail` (ADR-280).
pub const MAX_FAILURE_CODE_LEN: usize = 128;
/// The longest detail of a `Fail`, in bytes of text, that a caller keeps (ADR-280).
pub const MAX_FAILURE_DETAIL_LEN: usize = 1024;
/// The longest byte string `Concat` builds: the bound of an encoded request
/// ([`encode::MAX_ENCODED_BYTES`]), since building request payloads is what it is for.
pub const MAX_BYTES_LEN: usize = encode::MAX_ENCODED_BYTES;

/// The part of a `Fail`'s detail operand that [`Failure::detail`] carries: the operand is an
/// unbounded stack value, and a caller keeps at most [`MAX_FAILURE_DETAIL_LEN`] bytes of its
/// text. Lossy UTF-8 decoding never shrinks its input, so every byte of the decoded text up to
/// that limit comes from this prefix; the three extra bytes let a character that straddles the
/// limit decode as it would in full, before the caller cuts the text at a character boundary.
pub fn failure_detail_prefix(detail: &[u8]) -> &[u8] {
    &detail[..detail.len().min(MAX_FAILURE_DETAIL_LEN + 3)]
}

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
    ServiceRequest {
        service: u16,
    },
    ReadDtc {
        mask: u8,
    },
    RoutineControl {
        routine: u16,
        sub: u8,
    },
    SecurityAccess {
        level: u8,
    },
    FlashTransfer {
        block: u32,
    },
    Wait {
        millis: u32,
    },
    HmiRequest {
        form: u32,
    },
    /// `template` is a 4.3.1 record template.
    RecordInput {
        template: u32,
    },
    /// 4.6 capture.
    MonitorCapture {
        back_millis: u32,
    },
    Log {
        level: u8,
        message: u32,
    },

    // Declared failure (8.2.4, ADR-280). Appended last: the variant order is part of the
    // encoding (ADR-233).
    /// Ends the procedure as failed. `code` is a constant-pool index (UTF-8, like `Log::message`;
    /// `Program::validate` checks it is 1 to [`MAX_FAILURE_CODE_LEN`] bytes); the detail is the
    /// `Bytes` on top of the stack.
    Fail {
        code: u32,
    },

    // Byte strings. Appended after `Fail`: the variant order is part of the encoding (ADR-233).
    /// Pushes the length of the `Bytes` on top of the stack as an `I64`, leaving the bytes in
    /// place below it, as `IndexGet` does, so a length and an index into the same bytes need
    /// no copy.
    Length,

    // Arithmetic. Appended after `Length` (ADR-233).
    /// The remainder of the division below it by the value on top, both `I64` or both `F64`.
    /// It truncates like `Div`, so an integer remainder takes the dividend's sign and is never
    /// out of range (`i64::MIN` by -1 gives 0); dividing an integer by 0 is
    /// `VmError::DivisionByZero`, a float by 0 gives NaN.
    Rem,
    /// Negates the `I64` or `F64` on top of the stack. Negating `i64::MIN` is
    /// `VmError::Overflow`; a float only changes its sign, NaN and zeros included.
    Neg,

    // Byte strings. Appended after `Neg` (ADR-233).
    /// Replaces the `Bytes` below the top of the stack and the `Bytes` on top with their
    /// concatenation, the lower one first. A result longer than [`MAX_BYTES_LEN`] is
    /// `VmError::BytesTooLong`, so a loop that keeps doubling a value cannot exhaust memory.
    Concat,
    /// Replaces a `Bytes`, a start and a stop (two `I64`s, the stop on top) with the bytes from
    /// start up to stop, as Starlark slices bytes: a negative index counts from the end, both
    /// are then clamped to the length, and a stop before the start gives empty bytes. A stop of
    /// `i64::MAX` therefore slices to the end. No index is out of range.
    Slice,

    // Conversions. Appended after `Slice` (ADR-233).
    /// Replaces the `I64` on top of the stack with the nearest `F64`, as Starlark's `float`
    /// does; integers past 2^53 may round.
    IntToFloat,
    /// Replaces the `F64` on top of the stack with its integer part, rounding toward zero, as
    /// Starlark's `int` does. NaN, an infinity, or a value whose integer part is outside the
    /// `I64` range is `VmError::FloatNotConvertible`.
    FloatToInt,

    // Declarations. Appended after `FloatToInt` (ADR-233, ADR-316).
    /// Replaces the `Bytes` on top of the stack, a whole positive response starting with its
    /// SID, with the value of field `field` (`Field.id`) among the top-level fields of service
    /// `service`'s (`Service.id`) declared positive response, decoded through the declaration
    /// part the VM is stepped with ([`Vm::step_with_declarations`]) by
    /// [`decode::decode_positive_response`]. An unsigned or signed integer becomes an `I64`, a
    /// float an `F64`, text its UTF-8 `Bytes`, and bytes `Bytes`; a text table gives its
    /// internal value. A shaped field, an unknown service or field, a response that does not
    /// decode, an unsigned value past `i64::MAX` and stepping without declarations are
    /// `VmError`s.
    DecodeField {
        service: u32,
        field: u32,
    },
}

// ---------------------------------------------------------------- Program

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub schema_version: u32,
    pub code: Vec<Op>,
    pub constants: Vec<Vec<u8>>,
    pub sections: Vec<Section>,
    /// 8.4: mapping table from bytecode back to the original source.
    pub source_map: Vec<SourceSpan>,
    /// Where the identity of the ECU is read from, for the restart order (ADR-229, ADR-245).
    #[serde(default)]
    pub identity: IdentitySources,
    /// Sources of the declared flash preconditions (8.9, ADR-245).
    #[serde(default)]
    pub preconditions: Preconditions,
    /// Recovery plans, one per flash session (8.10.1, ADR-245). [`Program::validate`] checks them.
    #[serde(default)]
    pub flash: Vec<FlashRecovery>,
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
        // Exhaustive on purpose: a new instruction must be classified here.
        match self {
            Op::ServiceRequest { .. }
            | Op::ReadDtc { .. }
            | Op::RoutineControl { .. }
            | Op::SecurityAccess { .. }
            | Op::FlashTransfer { .. }
            | Op::Wait { .. }
            | Op::HmiRequest { .. }
            | Op::RecordInput { .. }
            | Op::MonitorCapture { .. }
            | Op::Log { .. } => true,
            Op::PushI64(_)
            | Op::PushF64(_)
            | Op::PushBytes(_)
            | Op::Pop
            | Op::Dup
            | Op::Swap
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::CmpEq
            | Op::CmpLt
            | Op::CmpGt
            | Op::Not
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::Call(_)
            | Op::Ret
            | Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::LoadGlobal(_)
            | Op::StoreGlobal(_)
            | Op::IndexGet
            | Op::IndexSet
            | Op::Length
            | Op::Rem
            | Op::Neg
            | Op::Concat
            | Op::Slice
            | Op::IntToFloat
            | Op::FloatToInt
            | Op::DecodeField { .. }
            | Op::Fail { .. } => false,
        }
    }
}

// ---------------------------------------------------------------- Execution state

/// Maximum operand stack depth. A program that exceeds it fails with [`VmError::StackOverflow`].
pub const MAX_STACK: usize = 1024;
/// Maximum subroutine nesting. A program that exceeds it fails with [`VmError::CallDepthExceeded`].
pub const MAX_CALL_DEPTH: usize = 64;

/// Written to the journal in its entirety (8.2.5). Serialized with postcard.
///
/// `PartialEq` compares floats by value, so a state holding NaN is not equal to itself; compare
/// the postcard bytes to check that a state did not change.
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
    /// makes the same call again on a later step.
    ///
    /// `inquiry` identifies the request: it stays the same while the VM waits on it (and after a
    /// host error or a resume from the journal at the same instruction), and a later instruction,
    /// including the same instruction reached again in a loop, gets a new one. Returning `Some`
    /// closes the inquiry.
    fn security_access(
        &mut self,
        inquiry: u64,
        level: u8,
        seed: &[u8],
    ) -> Result<Option<Vec<u8>>, Self::Error>;
    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), Self::Error>;
    /// Returns `Ok(true)` once `millis` have passed since the first call for `inquiry`, and
    /// `Ok(false)` before that, so the job runner stays responsive (cancellation, keep-alive)
    /// while a procedure waits. `inquiry` as for [`DiagHost::security_access`].
    fn wait(&mut self, inquiry: u64, millis: u32) -> Result<bool, Self::Error>;
    /// `Ok(None)` means no answer yet; `inquiry` as for [`DiagHost::security_access`].
    fn hmi_request(&mut self, inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
    /// Asks the operator to fill in a record template (4.3.1), a kind of HMI request: `Ok(None)`
    /// means no answer yet; `inquiry` as for [`DiagHost::security_access`].
    fn record_input(
        &mut self,
        inquiry: u64,
        template: &[u8],
    ) -> Result<Option<Vec<u8>>, Self::Error>;
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
            },
        }
    }

    /// Resume from an interrupted journal.
    pub fn resume(state: VmState) -> Self {
        Self { state }
    }

    /// The instruction the next [`Vm::step`] executes, or `None` when the program has finished.
    /// The job runner uses it to journal an intent marker before a primitive is sent (ADR-229).
    ///
    /// It runs [`Vm::check_state`] and, for a diagnostic primitive, the checks [`Vm::step`] makes
    /// before it calls the host (operand, constant, UTF-8, room for an answer that is pushed). So for a primitive, `Ok(Some(op))` means the next `step` reaches
    /// the host (ADR-233): the runner never journals an intent for a request that is not sent.
    /// Other instructions can still fail in `step` (arithmetic, types, limits); no intent
    /// precedes them.
    pub fn current_op<'p>(&self, program: &'p Program) -> Result<Option<&'p Op>, VmError> {
        self.check_state(program)?;
        let op = program.code.get(self.state.pc as usize);
        if let Some(op) = op {
            self.check_primitive(op, program)?;
        }
        Ok(op)
    }

    /// Checks everything [`Vm::step`] checks before it runs an instruction (ADR-233), so the
    /// job runner can validate a state restored from the journal before it acts on it.
    ///
    /// A resumed state is not trusted: besides the schema, program size and step counter, this
    /// checks the invariants the instructions otherwise maintain themselves (stack size, call
    /// depth, every frame's return position and `pc`), so a damaged or edited journal can
    /// neither exceed the limits nor jump outside the program. The contents of values and
    /// variable slots need no check, since every instruction checks the types it uses. A
    /// finished state that breaks an invariant is refused too, rather than reported finished.
    pub fn check_state(&self, program: &Program) -> Result<(), VmError> {
        if program.schema_version != IR_SCHEMA_VERSION {
            return Err(VmError::SchemaMismatch {
                program: program.schema_version,
                runtime: IR_SCHEMA_VERSION,
            });
        }
        if self.state.schema_version != program.schema_version {
            return Err(VmError::StateSchemaMismatch {
                program: program.schema_version,
                state: self.state.schema_version,
            });
        }
        // Positions are u32 in the bytecode and the state.
        if u32::try_from(program.code.len()).is_err() {
            return Err(VmError::ProgramTooLarge);
        }
        if self.state.steps == u64::MAX {
            return Err(VmError::StepLimit);
        }
        if self.state.stack.len() > MAX_STACK {
            return Err(VmError::StackOverflow);
        }
        if self.state.call_stack.len() > MAX_CALL_DEPTH {
            return Err(VmError::CallDepthExceeded);
        }
        for frame in &self.state.call_stack {
            if frame.return_pc as usize > program.code.len() {
                return Err(VmError::BadReturnPc(frame.return_pc));
            }
        }
        if self.state.pc as usize > program.code.len() {
            return Err(VmError::BadPc(self.state.pc));
        }
        Ok(())
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
        self.step_with_declarations(program, None, host)
    }

    /// [`Vm::step`] with the declaration part of the variant the program runs on, which
    /// [`Op::DecodeField`] decodes through (ADR-316). Without it, `DecodeField` is
    /// [`VmError::NoDeclarations`].
    pub fn step_with_declarations<H: DiagHost>(
        &mut self,
        program: &Program,
        declarations: Option<&EcuVariant<'_>>,
        host: &mut H,
    ) -> Result<StepOutcome, StepError<H::Error>> {
        let Some(op) = self.current_op(program)? else {
            return Ok(StepOutcome::Finished);
        };
        match self.execute(op, program, declarations, host)? {
            Executed::Done => {
                self.state.steps += 1;
                if self.state.pc as usize == program.code.len() {
                    Ok(StepOutcome::Finished)
                } else {
                    Ok(StepOutcome::Continue)
                }
            }
            Executed::Waiting(on) => Ok(StepOutcome::Waiting(on)),
            Executed::Failed(failure) => Ok(StepOutcome::Failed(failure)),
        }
    }

    fn execute<H: DiagHost>(
        &mut self,
        op: &Op,
        program: &Program,
        declarations: Option<&EcuVariant<'_>>,
        host: &mut H,
    ) -> Result<Executed, StepError<H::Error>> {
        let next = self.state.pc + 1;
        // Unchanged until the instruction completes, so it identifies a waiting inquiry.
        let inquiry = self.state.steps;
        // Every check a primitive needs before the host call, shared with `current_op`. Nothing
        // after a host call below can fail, so a `VmError` always means the host was not called.
        let input = self.check_primitive(op, program)?;
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

            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => {
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
            Op::Neg => {
                let result = match self.top(1)?[0] {
                    Value::I64(a) => Value::I64(a.checked_neg().ok_or(VmError::Overflow)?),
                    Value::F64(a) => Value::F64(-a),
                    _ => return Err(VmError::TypeMismatch.into()),
                };
                self.replace(1, result);
            }
            Op::Concat => {
                let [Value::Bytes(a), Value::Bytes(b)] = self.top(2)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let len = a.len() + b.len();
                if len > MAX_BYTES_LEN {
                    return Err(VmError::BytesTooLong(len as u64).into());
                }
                let mut joined = Vec::with_capacity(len);
                joined.extend_from_slice(a);
                joined.extend_from_slice(b);
                self.replace(2, Value::Bytes(joined));
            }
            Op::Slice => {
                let [Value::Bytes(bytes), Value::I64(start), Value::I64(stop)] = self.top(3)?
                else {
                    return Err(VmError::TypeMismatch.into());
                };
                let (from, to) = slice_bounds(*start, *stop, bytes.len());
                let sliced = bytes[from..to].to_vec();
                self.replace(3, Value::Bytes(sliced));
            }
            Op::IntToFloat => {
                let [Value::I64(n)] = self.top(1)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let converted = *n as f64;
                self.replace(1, Value::F64(converted));
            }
            Op::FloatToInt => {
                let [Value::F64(f)] = self.top(1)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let converted = float_to_int(*f).ok_or(VmError::FloatNotConvertible)?;
                self.replace(1, Value::I64(converted));
            }
            Op::DecodeField { service, field } => {
                let [Value::Bytes(message)] = self.top(1)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let declarations = declarations.ok_or(VmError::NoDeclarations)?;
                let value = decode_field(declarations, *service, *field, message)?;
                self.replace(1, value);
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
                    // The return position was checked by `check_state` or `Call`.
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
                // The bytes stay on the stack, so reading several bytes needs no copies.
                let [Value::Bytes(bytes), Value::I64(index)] = self.top(2)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                let byte = *bytes
                    .get(byte_index(*index, bytes.len())?)
                    .expect("checked by byte_index");
                self.replace(1, Value::I64(byte.into()));
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
            Op::Length => {
                let [Value::Bytes(bytes)] = self.top(1)? else {
                    return Err(VmError::TypeMismatch.into());
                };
                // A Vec's length is at most isize::MAX, so it fits an i64.
                let len = i64::try_from(bytes.len()).expect("a length fits an i64");
                self.push(Value::I64(len))?;
            }

            Op::ServiceRequest { service } => {
                let response = host
                    .service_request(*service, input.bytes())
                    .map_err(StepError::Host)?;
                self.replace(1, Value::Bytes(response));
            }
            Op::ReadDtc { mask } => {
                let response = host.read_dtc(*mask).map_err(StepError::Host)?;
                self.push_checked(Value::Bytes(response));
            }
            Op::RoutineControl { routine, sub } => {
                let response = host
                    .routine_control(*routine, *sub, input.bytes())
                    .map_err(StepError::Host)?;
                self.replace(1, Value::Bytes(response));
            }
            Op::SecurityAccess { level } => {
                match host
                    .security_access(inquiry, *level, input.bytes())
                    .map_err(StepError::Host)?
                {
                    Some(key) => self.replace(1, Value::Bytes(key)),
                    None => return Ok(Executed::Waiting(WaitingOn::SeedKey)),
                }
            }
            Op::FlashTransfer { block } => {
                host.flash_transfer(*block, input.bytes())
                    .map_err(StepError::Host)?;
                self.state.stack.pop();
            }
            Op::Wait { millis } => {
                if !host.wait(inquiry, *millis).map_err(StepError::Host)? {
                    return Ok(Executed::Waiting(WaitingOn::Timer));
                }
            }
            Op::HmiRequest { .. } => {
                match host
                    .hmi_request(inquiry, input.bytes())
                    .map_err(StepError::Host)?
                {
                    Some(answer) => self.push_checked(Value::Bytes(answer)),
                    None => return Ok(Executed::Waiting(WaitingOn::Hmi)),
                }
            }
            Op::RecordInput { .. } => {
                match host
                    .record_input(inquiry, input.bytes())
                    .map_err(StepError::Host)?
                {
                    Some(answer) => self.push_checked(Value::Bytes(answer)),
                    None => return Ok(Executed::Waiting(WaitingOn::Hmi)),
                }
            }
            Op::MonitorCapture { back_millis } => host
                .monitor_capture(*back_millis)
                .map_err(StepError::Host)?,
            Op::Log { level, .. } => host.log(*level, input.text()),

            // Terminal and not a primitive: the state stays on the instruction, like a wait, so
            // stepping again gives the same outcome (ADR-280).
            Op::Fail { code } => {
                let code = std::str::from_utf8(constant(program, *code)?)
                    .map_err(|_| VmError::InvalidUtf8(*code))?
                    .to_owned();
                let detail = failure_detail_prefix(self.bytes_operand()?).to_vec();
                return Ok(Executed::Failed(Failure { code, detail }));
            }
        }
        self.state.pc = next;
        Ok(Executed::Done)
    }

    /// The checks a diagnostic primitive needs before the host is called, and what it sends.
    /// Shared by [`Vm::current_op`] and `execute`, so the two cannot drift; exhaustive, so a new
    /// instruction must declare its checks here.
    fn check_primitive<'a>(
        &'a self,
        op: &Op,
        program: &'a Program,
    ) -> Result<HostInput<'a>, VmError> {
        Ok(match op {
            // Send the bytes on top of the stack and replace them with the answer, which needs
            // no room.
            Op::ServiceRequest { .. } | Op::RoutineControl { .. } | Op::SecurityAccess { .. } => {
                HostInput::Bytes(self.bytes_operand()?)
            }
            // Send the bytes on top of the stack, which are then dropped.
            Op::FlashTransfer { .. } => HostInput::Bytes(self.bytes_operand()?),
            // Push the answer.
            Op::ReadDtc { .. } => {
                self.check_room(0, 1)?;
                HostInput::Nothing
            }
            // Send a constant and push the answer.
            Op::HmiRequest { form: index } | Op::RecordInput { template: index } => {
                let bytes = constant(program, *index)?;
                self.check_room(0, 1)?;
                HostInput::Bytes(bytes)
            }
            Op::Log { message, .. } => HostInput::Text(
                std::str::from_utf8(constant(program, *message)?)
                    .map_err(|_| VmError::InvalidUtf8(*message))?,
            ),
            Op::Wait { .. } | Op::MonitorCapture { .. } => HostInput::Nothing,
            // Not a diagnostic primitive: checked during execution.
            Op::PushI64(_)
            | Op::PushF64(_)
            | Op::PushBytes(_)
            | Op::Pop
            | Op::Dup
            | Op::Swap
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::CmpEq
            | Op::CmpLt
            | Op::CmpGt
            | Op::Not
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::Call(_)
            | Op::Ret
            | Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::LoadGlobal(_)
            | Op::StoreGlobal(_)
            | Op::IndexGet
            | Op::IndexSet
            | Op::Length
            | Op::Rem
            | Op::Neg
            | Op::Concat
            | Op::Slice
            | Op::IntToFloat
            | Op::FloatToInt
            | Op::DecodeField { .. } => HostInput::Nothing,
            // The checks of `Fail` run in `execute`, before it changes anything.
            Op::Fail { .. } => HostInput::Nothing,
        })
    }

    /// Pushes a host answer whose room [`Vm::check_primitive`] already checked; cannot fail, so
    /// nothing after a host call can.
    fn push_checked(&mut self, value: Value) {
        debug_assert!(
            self.state.stack.len() < MAX_STACK,
            "room checked before the host call"
        );
        self.state.stack.push(value);
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
        if self.state.stack.len().saturating_sub(pop) + push > MAX_STACK {
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
    Failed(Failure),
}

/// What a diagnostic primitive passes to the host, validated by `check_primitive`.
enum HostInput<'a> {
    Nothing,
    Bytes(&'a [u8]),
    Text(&'a str),
}

impl<'a> HostInput<'a> {
    fn bytes(&self) -> &'a [u8] {
        match self {
            HostInput::Bytes(bytes) => bytes,
            _ => unreachable!("check_primitive returns bytes for this instruction"),
        }
    }

    fn text(&self) -> &'a str {
        match self {
            HostInput::Text(text) => text,
            _ => unreachable!("check_primitive returns text for this instruction"),
        }
    }
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

/// The value of field `field` of service `service`'s positive response in `message`, decoded
/// through `variant` ([`Op::DecodeField`], ADR-316).
fn decode_field(
    variant: &EcuVariant<'_>,
    service: u32,
    field: u32,
    message: &[u8],
) -> Result<Value, VmError> {
    let unknown = || VmError::UnknownField { service, field };
    let positive = variant
        .services()
        .and_then(|services| services.iter().find(|s| s.id() == service))
        .and_then(|s| s.positive())
        .ok_or_else(unknown)?;
    // Checked before decoding, so an unknown field is not reported as an undecodable response.
    if !positive
        .fields()
        .is_some_and(|fields| fields.iter().any(|f| f.id() == field))
    {
        return Err(unknown());
    }
    let fields = decode::decode_positive_response(&positive, variant.layouts(), message)
        .map_err(|_| VmError::Undecodable { service })?;
    let decoded = fields
        .into_iter()
        .find(|f| f.id == field)
        .ok_or_else(unknown)?;
    Ok(match decoded.value {
        decode::PhysicalValue::Unsigned(n) => {
            Value::I64(i64::try_from(n).map_err(|_| VmError::Overflow)?)
        }
        decode::PhysicalValue::Signed(n) => Value::I64(n),
        decode::PhysicalValue::Float(x) => Value::F64(x),
        decode::PhysicalValue::Text(text) => Value::Bytes(text.into_bytes()),
        decode::PhysicalValue::Bytes(bytes) => Value::Bytes(bytes),
        decode::PhysicalValue::Group(_)
        | decode::PhysicalValue::Repeat(_)
        | decode::PhysicalValue::Select { .. } => {
            return Err(VmError::ShapedField { service, field });
        }
    })
}

/// `f` rounded toward zero, or `None` when it is NaN, infinite, or its integer part is outside
/// the `i64` range (whose bounds are -2^63, exactly representable, and 2^63, excluded).
fn float_to_int(f: f64) -> Option<i64> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0; // 2^63
    let t = f.trunc();
    (-LIMIT..LIMIT).contains(&t).then_some(t as i64)
}

/// The byte range of a slice from `start` to `stop` of `len` bytes, as Starlark computes it: a
/// negative index counts from the end, both are clamped to `0..=len`, and the range is empty
/// when the stop comes before the start. `Op::Slice` uses it, and so can a frontend that slices
/// constants at ingestion, so both agree.
pub fn slice_bounds(start: i64, stop: i64, len: usize) -> (usize, usize) {
    // A Vec's length is at most isize::MAX, so it fits an i64.
    let len = i64::try_from(len).expect("a length fits an i64");
    let clamp = |n: i64| {
        let n = if n < 0 {
            n.saturating_add(len).max(0)
        } else {
            n.min(len)
        };
        usize::try_from(n).expect("clamped to 0..=len")
    };
    let (from, to) = (clamp(start), clamp(stop));
    (from, to.max(from))
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
                Op::Rem if *b == 0 => return Err(VmError::DivisionByZero),
                // Only `i64::MIN % -1` wraps, and its exact remainder, 0, is what it wraps to.
                Op::Rem => Some(a.wrapping_rem(*b)),
                _ => unreachable!("not an arithmetic op"),
            };
            result.map(Value::I64).ok_or(VmError::Overflow)
        }
        [Value::F64(a), Value::F64(b)] => Ok(Value::F64(match op {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            Op::Div => a / b,
            Op::Rem => a % b,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Continue,
    Finished,
    /// Waiting for an answer from outside; the state is unchanged and the caller steps again
    /// later. Keeps waiting even while disconnected (5.6, 8.2.5).
    Waiting(WaitingOn),
    /// The procedure ended itself as failed (`Op::Fail`, ADR-280). The state is unchanged, so
    /// stepping again gives the same outcome; the caller treats it as terminal.
    Failed(Failure),
}

/// The declared failure of a procedure (ADR-280).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Chosen by the procedure; valid UTF-8, not otherwise limited by the VM.
    pub code: String,
    /// Bytes built at run time, at most [`failure_detail_prefix`] of the operand, so a long
    /// operand is not copied. Not checked by the VM; may be invalid UTF-8.
    pub detail: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingOn {
    /// An HMI request, or a record template input (4.3.1), has not been answered.
    Hmi,
    /// The key for a security access seed has not arrived from the server.
    SeedKey,
    /// A `Wait` instruction's time has not passed.
    Timer,
}

#[derive(Debug, thiserror::Error)]
pub enum StepError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Vm(#[from] VmError),
    #[error("host error")]
    Host(#[source] E),
}

/// Serializable because the agent journals the error that stopped a run (ADR-288): the encoding
/// is part of the journal format, so variants are only ever appended, never reordered or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum VmError {
    #[error("schema version mismatch: program={program}, runtime={runtime}")]
    SchemaMismatch { program: u32, runtime: u32 },
    #[error("state was saved for schema version {state}, program has {program}")]
    StateSchemaMismatch { program: u32, state: u32 },
    #[error("program has more instructions than a u32 position can address")]
    ProgramTooLarge,
    #[error("step counter exhausted")]
    StepLimit,
    #[error("invalid instruction position: {0}")]
    BadPc(u32),
    #[error("call frame returns to an invalid position: {0}")]
    BadReturnPc(u32),
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
    #[error("byte string of {0} bytes is longer than {MAX_BYTES_LEN}")]
    BytesTooLong(u64),
    #[error("float is NaN, infinite or outside the integer range")]
    FloatNotConvertible,
    #[error("the VM was stepped without a declaration part to decode with")]
    NoDeclarations,
    #[error("service {service} has no declared positive response field {field}")]
    UnknownField { service: u32, field: u32 },
    #[error("field {field} of service {service} is a nested layout, not a value")]
    ShapedField { service: u32, field: u32 },
    #[error("the response does not decode as service {service}'s positive response")]
    Undecodable { service: u32 },
}

#[cfg(test)]
mod tests;

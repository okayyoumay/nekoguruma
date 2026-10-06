# ADR-233: diag-ir VM Execution Semantics

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `diag-ir` (`src/lib.rs`: `Vm::step`, `VmState`, `DiagHost`, `StepOutcome`, `VmError`), `docs/system-architecture.md` 8.2.4

## Context

Design 8.2.3 to 8.2.5 fix the procedure VM's purpose and the resume model:
- The VM state is serialized in full.
- After a worker crash or VCI disconnect, execution continues from the diagnostic primitive with the state retained.
- After an agent crash, it continues from the latest journal checkpoint.
- Checkpoints are taken per diagnostic primitive (per block for flash transfers).

The design does not say:
- whether an instruction can fail halfway;
- who writes the journal;
- how a wait on the server (seed-key, HMI) shows up to the caller;
- how operands are passed;
- what the type rules are.

ADR-229 also requires write-ahead intent markers committed before RequestDownload and RequestTransferExit are sent. Something must be able to act before a primitive runs.

No IR programs, transpiler or journals existed when this was decided, so `VmState` and `DiagHost` could still change freely.

## Decision

1. **Atomic steps.** `Vm::step` runs one instruction and either completes it or leaves `VmState` unchanged.
   - Completing means: operands consumed, results pushed, `pc` advanced and `steps` incremented.
   - Everything the VM can check is checked before the host is called, so a host answer is never discarded. This covers operand types, constant indexes and stack room for the result.
   - After a host error the state still points at the primitive, so stepping again repeats it (8.2.5).
2. **`step` holds no policy.** The job runner decides whether repeating a primitive is safe, using the section's idempotency attribute and ADR-229's transfer rules. 8.2.5's "the VM follows this attribute" means the IR runtime as a whole (VM plus runner), not `step`.
   - `step` returns `Result<StepOutcome, StepError<H::Error>>`, where `StepError` separates VM errors from host errors.
   - `DiagHost::Error` must implement `std::error::Error + Send + Sync + 'static`.
3. **The caller journals.**
   - `DiagHost::checkpoint` is removed. A checkpoint written inside `step` would either come after the state advanced (a failed write would break item 1) or be unable to precede the request (ADR-229's intent markers).
   - The runner writes the journal around `step`, using two helpers. `Vm::current_op` shows the next instruction before it runs. `Op::is_diagnostic_primitive` tells whether that instruction goes to the host.
   - `VmState::checkpoint` and `resume_count` are left to the write-job journal.
4. **Waits are a step outcome.**
   - `hmi_request` and `security_access` return `Ok(None)` while no answer has arrived (8.2.5 lists both as waits while the server is unreachable). `step` then returns `StepOutcome::Waiting(WaitingOn::Hmi | WaitingOn::SeedKey)` without changing the state.
   - The caller steps again later and the same request is made again. The host keeps track of what it already asked.
   - The VM has no pending flag, so a loop that reaches the same `HmiRequest` again makes a new inquiry.
5. **Values and types.**
   - No implicit conversion: mixed operand types are an error.
   - `I64` arithmetic is checked: overflow and division by zero are errors, nothing wraps. `F64` follows IEEE 754.
   - Bitwise operations take `I64` only and act on the 64-bit pattern. `Shl` and `Shr` take a shift of 0 to 63; `Shr` is logical, and `Shl` drops shifted-out bits without an error.
   - `CmpEq` compares two values of the same type; `F64` uses IEEE equality, so NaN is not equal to itself. `CmpLt` and `CmpGt` take numbers only. `Not` and `JumpIfFalse` take `Bool`.
   - `IndexGet` and `IndexSet` read and write single bytes of a `Bytes` value, which has value semantics. An out-of-range index or a byte value outside 0 to 255 is an error.
6. **Frames.**
   - `call_stack` holds `Frame { return_pc, locals }`. `Call` saves the caller's locals and starts the subroutine with none; `Ret` restores them.
   - `Ret` at the top level sets `pc` to the end of the code, so "finished" is a property of the state and stepping a finished VM changes nothing.
   - Locals and globals are `Option` slots: reading an unset slot is an error, and a store grows the vector.
   - A jump or call target may equal the code length (ending the program). A larger target is `BadPc`.
7. **Primitive operands.**
   - The primitives that send data take it as `Bytes` from the top of the stack: service request payload, routine control payload, seed, flash block.
   - Primitives that return data push it as `Bytes`.
   - HMI forms, record templates and log messages are constant-pool indexes. A log message must be valid UTF-8, because the server's dry run at ingestion (12.1) should find a bad constant, not the vehicle.
8. **Limits.** The operand stack holds at most 1024 values (`MAX_STACK`) and calls nest at most 64 deep (`MAX_CALL_DEPTH`). `step` has no step budget; the runner owns the loop. `VmState::steps` counts completed instructions across resumption, for audit (8.2.3) and for any limit the runner enforces.
9. **Schema check.** `step` refuses a program whose schema version differs from the runtime's, or a state whose version differs from the program's.

## Consequences

- The job runner must implement the idempotency and section policy, the intent markers and the journal. The VM gives it a state that is either before or after each instruction, never in between.
- `DiagHost` is synchronous, while the agent's worker client is asynchronous (tonic). The runner must bridge the two, for example with a per-job blocking thread, or the trait must become async. Changing the trait after more implementations exist costs a migration.
- Large `Bytes` values on the stack (flash blocks) make every journaled `VmState` large. A transpiler should push block data immediately before `FlashTransfer`; a cap on the total bytes held may be needed later.
- The instruction set has gaps a transpiler will need, such as length, remainder, negation, integer/float conversion, slicing and concatenation. `Op` is postcard-encoded with varint discriminants, so variants can be appended without breaking existing programs.

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
   - `wait` returns `Ok(false)` until its time has passed, reported as `WaitingOn::Timer`. A `Wait` therefore never blocks inside `step`, and the runner can keep servicing cancellation and the S3 keep-alive (8.2.5) while it waits.
   - The caller steps again later and the same call is made again.
   - Each of these calls carries an `inquiry` number, which is the state's `steps` counter. It does not change while the VM waits, after a host error, or across a resume at the same instruction. It does change when a later instruction runs, including the same instruction reached again in a loop. The host uses it to tell a poll of an open inquiry from a new one. Returning an answer closes the inquiry. The VM itself has no pending flag.
5. **Values and types.**
   - No implicit conversion: mixed operand types are an error.
   - `I64` arithmetic is checked: overflow and division by zero are errors, nothing wraps. `F64` follows IEEE 754.
   - Bitwise operations take `I64` only and act on the 64-bit pattern. `Shl` and `Shr` take a shift of 0 to 63; `Shr` is logical, and `Shl` drops shifted-out bits without an error.
   - `CmpEq` compares two values of the same type; `F64` uses IEEE equality, so NaN is not equal to itself. `CmpLt` and `CmpGt` take numbers only. `Not` and `JumpIfFalse` take `Bool`.
   - Integer division truncates toward zero.
   - `IndexGet` reads one byte of a `Bytes` value and leaves the bytes on the stack below the result, so reading several bytes needs no copies. `IndexSet` replaces one byte. `Bytes` has value semantics. An out-of-range index or a byte value outside 0 to 255 is an error.
   - `VmState`'s `PartialEq` compares floats by value, so a state holding NaN is not equal to itself. To check that a state did not change, compare its postcard bytes.
6. **Frames.**
   - `call_stack` holds `Frame { return_pc, locals }`. `Call` saves the caller's locals and starts the subroutine with none; `Ret` restores them.
   - `Ret` at the top level sets `pc` to the end of the code, so "finished" is a property of the state and stepping a finished VM changes nothing.
   - Locals and globals are `Option` slots: reading an unset slot is an error, and a store grows the vector.
   - A jump or call target may equal the code length (ending the program). A larger target is `BadPc`.
   - A resumed state is not trusted: `Ret` checks the frame's return position before using it.
7. **Primitive operands.**
   - The primitives that send data take it as `Bytes` from the top of the stack: service request payload, routine control payload, seed, flash block.
   - Primitives that return data push it as `Bytes`.
   - HMI forms, record templates and log messages are constant-pool indexes. A log message must be valid UTF-8, because the server's dry run at ingestion (12.1) should find a bad constant, not the vehicle.
8. **Limits.** The operand stack holds at most 1024 values (`MAX_STACK`) and calls nest at most 64 deep (`MAX_CALL_DEPTH`). `step` has no step budget; the runner owns the loop. `VmState::steps` counts completed instructions across resumption, for audit (8.2.3) and for any limit the runner enforces.
9. **Checks before each step.**
   - `step` refuses a program whose schema version differs from the runtime's (`SchemaMismatch`).
   - It refuses a state whose version differs from the program's (`StateSchemaMismatch`).
   - It refuses a program too long for `u32` positions (`ProgramTooLarge`).
   - It refuses a state whose step counter is exhausted (`StepLimit`).
10. **Encoding.** `Op` and `VmState` are stored with postcard, which encodes an enum variant by its index. The order of `Op`'s variants is therefore part of the stored format, and a test pins it. `Op::is_diagnostic_primitive` is an exhaustive match, so a new instruction cannot be added without classifying it.

## Consequences

- The job runner must implement the idempotency and section policy, the intent markers and the journal. The VM gives it a state that is either before or after each instruction, never in between.
- `DiagHost` is synchronous, while the agent's worker client is asynchronous (tonic). The runner must bridge the two, for example with a per-job blocking thread, or the trait must become async. Changing the trait after more implementations exist costs a migration.
- Large `Bytes` values on the stack (flash blocks) make every journaled `VmState` large. A transpiler should push block data immediately before `FlashTransfer`; a cap on the total bytes held may be needed later.
- The instruction set has gaps a transpiler will need, such as length, remainder, negation, integer/float conversion, slicing and concatenation. New variants are appended at the end of `Op`, so existing programs stay valid.
- A slot index can be as high as 65535, so a program can make `locals` or `globals` large, and every frame's locals are journaled.

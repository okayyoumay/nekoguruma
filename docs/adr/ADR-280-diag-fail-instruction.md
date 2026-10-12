# ADR-280: `diag.fail` Is a Terminal `Fail` Instruction, Journaled so a Restart Does Not Resume the Job

**Date:** 2026-10-12
**Status:** Accepted
**Affects:** `diag-ir` (`src/lib.rs`, `src/recovery.rs`), `agent` (`src/runner.rs`, `src/journal.rs`, `src/journaling.rs`, `src/restart.rs`, `src/policy.rs`, `docs/ngr-agent.md`), `diag-frontend` (`docs/starlark-subset.md`), `docs/system-architecture.md` 5.6, 8.2.4, 8.2.5; ADR-233, ADR-244, ADR-254, ADR-276

## Context

The Starlark subset replaces the built-in `fail` with `diag.fail(code, detail)`, so every abnormal
end of a procedure carries a code (ADR-254). ADR-254 left that call without an IR instruction, so
a procedure had no way to end a job as failed.

ADR-276 item 3 made the gap matter. The agent does not judge the erase's answer, so a procedure
whose erase the ECU refused has to end the job itself, and it cannot. The journal records no run
end either, so a run that ends with an open transfer is restart-eligible whatever the reason it
ended: a restart replays to the erase and sends it again.

## Decision

1. **`Op::Fail { code: u32 }` is appended as the last variant of `Op`, and `IR_SCHEMA_VERSION`
   becomes 3.** Appending keeps the encoding of every existing program (ADR-233 item 10), but a
   runtime of version 2 cannot decode a program that uses `Fail`. The version is what the agent
   reports in `capabilities` and a server matches before it sends a program (design 8.2.6 and
   9.5; `crates/diag-ir/schema/README.md`), so it changes with the instruction set. `code` is a
   constant-pool index holding UTF-8, like `Log::message`, because the server's dry run can then
   check it as it checks a log message (ADR-233 item 7). The detail is a `Bytes` operand popped
   from the stack, because a detail is built at run time. `Program::validate` also checks the
   code statically: it must exist, be UTF-8 and be 1 to 128 bytes (`MAX_FAILURE_CODE_LEN`), so
   the journal's record can always hold it. The Starlark frontend accepts 1 to 64 bytes, a
   stricter limit it may keep without the IR changing.
2. **The VM checks the operands before it changes anything:** the constant exists (`BadConstant`)
   and is UTF-8 (`InvalidUtf8`), and the top of the stack exists (`StackUnderflow`) and is
   `Bytes` (`TypeMismatch`). It does not check the detail's encoding or length: a `VmError` at a
   `Fail` would become `JobError::Vm`, which the journal does not record, so a restart would
   redo the transfer the procedure meant to end. It hands over only the operand's first
   1024 + 3 bytes (`failure_detail_prefix`), so a long run-time value is not copied: the kept text
   is at most 1024 bytes, lossy decoding never shrinks its input, and the extra bytes let a
   character straddling the limit decode whole. The runner decodes and bounds the detail
   (item 4).
3. **A new `StepOutcome::Failed(Failure { code, detail })` is returned with the state unchanged:**
   `pc` stays on the `Fail`, `steps` is not incremented and the stack is not popped. Stepping again
   gives the same outcome, as for a wait (ADR-233 item 4). `Fail` is not a diagnostic primitive:
   nothing reaches the host, so no intent marker precedes it and `check_primitive` has nothing
   to check for it.
4. **The runner ends the job in `JobError::ProcedureFailed { pc, code, detail }`.** The detail is
   decoded lossily and cut to 1024 bytes, the code to 128, on character boundaries; the journal
   refuses a record whose text is beyond those limits, also when it reads the file back, so a
   procedure cannot put an arbitrary amount of text into an error or a journal frame. When the
   job has a journal, the failure is committed first, as a new record
   `Record::ProcedureFailed { at, code, detail }` appended at the end of the record enum (no
   format version change, as for ADR-272). The code stays a string; the mapping to the shared
   protocol's `JobState` and `FailureCode` is not decided here.
5. **The journal folds the record into `RecoveryFacts::procedure_failed`.** It must come after the
   last step, and no record may follow it. The restart reads it first: `classify` returns
   `RestartDecision::Failed` before it looks at the transfer, and `resume_program_journaled` ends
   in the same `JobError::ProcedureFailed` with nothing sent to the ECU and no resume counted. It
   takes the journal's writer lock and reads the record before it opens the worker link, so the
   verdict does not depend on whether the worker or the VCI can be reached, and a journal that
   another run holds still ends in `JournalError::InUse` first (ADR-255 item 7). The checks every
   run makes before that (the program's schema and validity, the reprogramming slot, guards
   whose link is not confirmed closed, ADR-258) still come first; they delay the verdict, they
   do not change it.
6. **A flash plan may contain `Fail` anywhere, and it ends a path.** The plan validator treats it
   like `Ret` when it follows execution (a subroutine that ends in `Fail` does not reach a plan
   after it), but `Call` and `Ret` stay refused inside a plan.

Why terminal and not an interruption: design 5.6 defines Interrupted as power loss, a crash or
forced termination. Treating a refused-erase `Fail` as an interruption would make the restart
replay to the erase and send it again before the procedure's check, up to the resume limit,
overriding the procedure. A procedure that wants a retry jumps back to the erase itself. Why not
on-site intervention: the procedure itself decided, so there is nothing for a person to judge. The
precedent is `IdentityMismatch`, which also ends the job as Failed with a possibly erased ECU
(ADR-229 item 2).

## Consequences

- `StepOutcome` is no longer `Copy`, since `Failed` carries text; its callers were adjusted.
- One new journal record and one new `RecoveryFacts` field. A checkpoint summary encoded by an
  earlier build does not decode into the new `RecoveryFacts`; the summary is not stored across
  builds yet.
- A `Fail` is final even where a restart could have succeeded. That is the author's choice.
- The runtime checks the schema version by equality, for the program and for a journaled state,
  so with version 3 a program stored at version 2 is refused although it encodes the same, and a
  job interrupted under an agent of version 2 cannot be resumed by one of version 3 (its entry
  state is refused, which asks for on-site intervention). No program or journal of version 2 is
  stored outside this repository yet; accepting a range of versions is left until one is.
- A crash after the `Fail` step and before the failure record's commit leaves a journal without
  it. No request is sent in that window, and what the restart then redoes follows the existing
  rules (the redo of ADR-229, or the continuation of ADR-271), as if the procedure had not yet
  reached the `Fail`. A primitive outside a plan before the `Fail` leaves no step record (ADR-252),
  so a restart may send it again.
- A deterministic `VmError`, or the step limit, after the transfer-start marker is still redone by
  a restart until the resume limit; whether it gets the same terminal record is open.
- The mapping of `JobError::ProcedureFailed` to the shared protocol's `JobState` and `FailureCode`
  remains open.
- The Starlark-to-IR lowering of `diag.fail` and of the loop limit (`LOOP_LIMIT_EXCEEDED`) is
  specified in `crates/diag-frontend/docs/starlark-subset.md`; the transpiler does not exist yet.

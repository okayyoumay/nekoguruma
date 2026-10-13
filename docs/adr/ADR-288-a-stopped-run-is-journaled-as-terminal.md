# ADR-288: A Run Stopped on the Step Limit or a VM Error Is Journaled as a Terminal Record

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `agent` (`src/journal.rs`, `src/journaling.rs`, `src/restart.rs`, `src/runner.rs`,
`docs/ngr-agent.md`), `diag-ir` (`VmError`), `docs/system-architecture.md` 5.6; ADR-244, ADR-280,
ADR-286, ADR-287

## Context

A run can stop without reaching the program's end or a `Fail` in two ways that come from the
program itself:

- the job's step limit (`JobLimits::max_steps`, `JobError::StepLimit`), which stops a procedure
  that never ends;
- an instruction the VM refuses while it runs (`JobError::Vm`: a stack underflow, a type
  mismatch, an overflow and the like).

Neither left a record in the journal. ADR-280 (Consequences) left open whether they should end the
job as a declared failure does. ADR-287 (Consequences) made the gap worse before the erase: with
each run on its own step budget, a plain start after a run the limit stopped ran the program again
up to the limit. Its requests before the erase were sent again, SecurityAccess among them, which
counts against the ECU's failed-attempt limit, and no resume limit bounds plain starts. After the
transfer-start marker, a restart redid the transfer until the plan's resume limit stopped it,
erasing the ECU each time.

The maintainer decided that a step-limited run is terminal before and after the erase, journaled
like a declared failure, and that a VM error is the same kind of end, with the step limit's rule
as its default.

## Decision

1. **A run that stops on the step limit, or on an instruction the VM refuses, commits
   `Record::RunStopped { at, reason }` before it returns its error.** `reason` is
   `RunStop::StepLimit` or `RunStop::Vm(VmError)`. `at` is the instruction the run stopped at and
   the step count it had reached; for a VM error, any step the refused instruction completed
   comes before it. The record is appended at the end of the record enum, with no format version
   change, as for ADR-272 and ADR-280. `VmError` derives `Serialize` and `Deserialize` for it.
2. **It is terminal, like `Record::ProcedureFailed` (ADR-280) and `Record::IdentityMismatch`
   (ADR-286).**
   - It must come after the last step.
   - No record may follow it, and none of the three terminal records may follow another; this holds
     in a commit and when the file is read back.
   - The journal folds it into `RecoveryFacts::run_stopped`.
3. **A later resume ends in the same error, with nothing sent to the ECU and no resume counted.**
   - The resume returns `JobError::StepLimit` with the recorded step count, or `JobError::Vm` with
     the recorded position and error.
   - It reads the record under the journal's writer lock, before it opens the worker link, as for
     the other terminal records.
   - `restart::classify` returns `RestartDecision::Stopped` after a declared failure and an
     identity abort, and before it looks at the transfer.
   - So a plain start after a stop before the erase sends nothing, and a restart after a stop
     after the erase does not redo the transfer.
4. **The rule covers every run that keeps a journal.** That is a first run, a plain start on an
   existing journal, and a restart (its replay to the erase, the redo from the erase and the
   continuation after a read-back verification), since they share the step loop.
   - A VM error found before any run (the program's schema version, the restored state's schema
     version, `check_program`) is not a stop of a run. It is not journaled.
   - A cancel, a failed or unanswered step, and a wait nobody answers stay non-terminal, since
     another attempt can end differently.
5. **A commit that fails is the job's error instead (`JobError::Journal`), and the stop is only
   logged.** The journal never claims an ending it does not hold, as for ADR-280 and ADR-286.

Why terminal:

- The step limit stops a procedure that never ends, so running the same program again stops at
  the same place. The only effect of another run is to send its requests again.
- A VM error is the program's own fault. A redo replays the same instructions, so it reaches the
  same fault when the values come from the program. When they come from an ECU answer, ending the
  job is still the safe default: the program cannot handle that answer, and a person has to look
  at it.

Why not on-site intervention: the job cannot go on without a changed program or a changed limit,
which on-site intervention cannot supply. The ending is the same as `IdentityMismatch` and
`ProcedureFailed`: the job is over, with a possibly erased ECU.

## Consequences

- ADR-280's open consequence on a deterministic `VmError` or the step limit after the
  transfer-start marker is decided: neither is redone by a restart.
- ADR-287's open consequence is decided: a plain start after a run the limit stopped ends at once
  and sends nothing. ADR-287's per-run budget still applies to runs that follow an ending that is
  not terminal, such as a cancel or a failed step.
- A run that stops on the limit because the limit is too small for the job now ends the job. A
  first run with that limit fails the same way, so the limit or the procedure is wrong for that
  job.
- A crash after the stop and before its commit leaves the journal without it. The restart then
  follows the existing rules, as if the run had been interrupted at that step; no request is sent
  in that window.
- `VmError`'s encoding is now part of the journal format. Its variants are only appended, like the
  record enum's. Until the first release, breaking changes to the journal's stored state are
  accepted with no compatibility path (ADR-281 item 4).
- A checkpoint summary encoded by an earlier build does not decode into the new `RecoveryFacts`;
  the summary is not stored across builds yet.
- The mapping of these endings to the shared protocol's `JobState` and `FailureCode` remains open,
  as for `ProcedureFailed`.

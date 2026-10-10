# ADR-271: After a Restart, Read-Back Verification Is the State Check's Match, and the Program Continues from the Plan's End

**Date:** 2026-10-10
**Status:** Accepted (item 2's end step is a step from inside the plan, and its read-back follows ADR-274)
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `src/journaling.rs`), design 5.6 and 8.2.5, ADR-229 item 2 step 3, ADR-252 item 2, ADR-268 item 5

## Context

Design 5.6 has a restarted job move from `Interrupted` to `ReadBackVerification` and from there
to `Completed` or `Failed`. ADR-229 item 2 step 3, design 8.2.5, ADR-268 item 5 and ADR-269
send a restart there when the ECU reports the intended software version and the journal records
the interrupted pass's post-transfer steps as complete. None of them says what the verification
consists of, where the program goes on afterwards, or which job outcome it leads to:

- The IR declares no verification part. A flash plan's recovery boundaries are its entry, its
  erase, its RequestTransferExit and the end of its post-transfer steps; checks such as
  CheckMemory are among the post-transfer steps, which the journal already records as complete
  in this case.
- The journal keeps a VM state only for the step that reaches a plan's entry (ADR-252 item 2),
  so a restart can replay from the entry but cannot resume after the plan's end.
- `JobError` and `OnSiteReason` have no outcome for a verification that passes or fails.

## Decision

1. **The state check's match is the verification.** After a restart, read-back verification
   consists of what `restart::check_state` already established: the ECU reports the intended
   software version, and the journal records the post-transfer steps as complete for the
   interrupted pass (ADR-269). The agent runs no further reads of its own, and the IR gains no
   verification part. The completion record already covers the procedure's own checks, such as
   CheckMemory, and the version read covers the image the ECU now runs.
2. **The step that reaches a plan's end records the VM state.** The step that brings execution
   to a plan's `post_transfer_end_pc` commits a step record carrying the VM state after it,
   whatever the instruction is, before the post-transfer completion that ADR-252 item 2 commits
   right after it. This is the same record the entry step writes, so the journal format does
   not change. When a plan's end is the next plan's entry, the one record serves both.
   Reading the journal back keeps the states apart by the boundary they resume at: the newest
   state at the interrupted plan's entry and the newest at its end. A redo of the transfer uses
   the entry state and the continuation uses the end state, so a crash after the end state is
   synced but before the completion still redoes the transfer from the entry, as it does today.
   An entry state recorded after an end state (a new pass) makes that end state stale.
3. **The program continues from the plan's end.** On a passed verification the restart sends no
   erase and no further ECUReset for the verified plan. It restores the VM state recorded at the
   interrupted pass's plan end and runs the program's remaining instructions as a first run
   would, with the same journaling; a later flash plan or a final reset among them runs as it
   would on a first run, and a later plan takes the job back to `Writing` (design 5.6). A
   cancellation during the resumed program follows the first-run rules: it takes effect at the
   next interruptible point and ends the job in `Cancelled`. The job then ends in `Completed` or `Failed` exactly as that first
   run would.
4. **The continuation runs only when its instructions are safe to repeat.** The journal records
   no step outside a plan's range (ADR-252 item 2), so a restart cannot tell how far the program
   got after the plan's end: a primitive there may already have reached the ECU, and the
   continuation would send it again. The restart therefore continues only when every diagnostic
   primitive outside all of the program's plan ranges (`entry_pc` up to `post_transfer_end_pc`)
   lies in a section (`Section`) whose idempotency is `Safe`; `CheckState`, `Unsafe` and a
   primitive in no section count as not safe.
   The check is static over the whole program, so a jump back to earlier code is covered.
   Otherwise the job ends in on-site intervention with the image verified, which the report
   says. A primitive inside a later plan is guarded by that plan's own journaling: an
   interruption there puts the interruption point in that plan, and this path is not taken.
5. **A failed match keeps today's endings.** The state check's other outcomes are unchanged:
   the pre-erase version, the intended version without the completion, or the declared
   no-application response redo the transfer; any other version, or one that cannot be
   established, ends in on-site intervention. A journal whose completion is recorded but which
   holds no VM state for that plan end also ends in on-site intervention, since the program
   cannot be resumed there.

## Consequences

- A restart after a transfer that completed and validated finishes the job without rewriting
  the ECU, including the program's steps after the plan (further plans, a final reset, reads
  for the report).
- Every plan end now costs one more journaled VM state, the same size as the one at the entry.
- `ReadBackVerification` in design 5.6 has no work of its own after a restart: it passes on
  entry, and `Completed` or `Failed` comes from the rest of the program, through `Writing` again
  when the program has a later plan. During a first run,
  the program's own post-transfer steps remain the verification.
- The resumed program meets the ECU in its default session, which the restart confirmed, while
  a first run reaches the plan's end in whatever session the post-transfer steps left. A step
  after the plan that needs another session is refused by the ECU and ends the job as that
  refusal would end a first run; a procedure that relies on the session after its plan opens it
  again itself.
- A program with a primitive outside its plans that is not `Safe` (an unsafe routine, a
  state-dependent write) never continues after a restart; it ends in on-site intervention even
  though its image is verified. Journaling the steps after a plan, so the continuation could
  resume from the last one, would lift this and is left for when a program needs it.
- Implementing items 2 to 5 adds `OnSiteReason`s for a missing plan-end state and for a
  continuation that is not safe to repeat.

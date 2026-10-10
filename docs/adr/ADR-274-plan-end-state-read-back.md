# ADR-274: A Plan's End State Is Recorded from Inside the Plan, and the Restart Reads the Entry State Back Behind It

**Date:** 2026-10-10
**Status:** Accepted
**Affects:** `agent` (`src/journaling.rs`, `src/journal.rs`, `src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`), ADR-253 item 3, ADR-271 item 2, ADR-272 items 2 and 3, ADR-273 item 2

## Context

ADR-271 item 2 makes the step that brings execution to a plan's `post_transfer_end_pc` journal the
VM state after it, so a verified restart can continue from the plan's end. The journal treats VM
states as opaque bytes (ADR-244 item 6), so it cannot tell an end state from an entry state; the
restart has to find the plan's entry state behind an end state that is newer than it.

Three things complicate that:

- The validator lets a jump from outside a plan, such as a polling loop after it, land on the
  end pc. Recording a state for each such arrival would put an end state after every later
  state, and make the journal's last step move for steps that are not part of the plan.
- ADR-273 item 2 relied on the entry state being the journal's newest. That no longer holds
  once a pass has reached its end.
- With adjacent plans, one plan's end pc is the next one's entry pc. One record is then both the
  first plan's end state and the second plan's entry state, and the second plan's own end state
  is committed after it.

## Decision

1. **The end state is recorded by a step from inside the plan only.** `JobJournal::completed`
   records the state after a step whose pc is in `entry_pc..post_transfer_end_pc` and whose
   successor is `post_transfer_end_pc`. The entry rule is unchanged: any step that brings
   execution to a plan's `entry_pc` records its state. When both hold for one step, one record is
   written. A jump to the end pc from outside the plan journals nothing, as before ADR-271.
2. **The journal keeps the newest two VM states, and the restart reads the entry state like
   this.** It is the newest state if that stands at the plan's entry. It is the state before the
   newest only when the newest stands at the plan's end, is the current attempt's end-state record
   (the transfer's last post-transfer step is the step that carries it), and the interruption
   point is that same step, so no later step or request marker was journaled (the completion and
   a resume record may follow, since neither moves the interruption point). Any other newest state that is
   not at the entry gives `OnSiteReason::MissingEntryState`. ADR-253's refusal of adjacent plans
   stays: plan 1 complete, a pre-erase primitive of plan 2 journaled, then a crash, is on-site
   intervention. The invariant that two states suffice: at most one end state follows a pass's
   newest entry-standing state, because the end state is recorded only by an in-plan step and
   every run that starts from a restored entry state commits a run start (item 4).
3. **The end state for the continuation is the newest state, when it stands at the plan's end**
   (`restart::plan_end_state`). For adjacent plans that is the shared record.
4. **The replay of step 4b-2 commits a run start carrying the entry state; the continuation
   commits none.** This supersedes ADR-273 item 2. The run start sits newer than any earlier end
   state, so a crash in the replay restarts from the same entry state even when an earlier pass
   had completed, and ADR-272 item 3's rule that the newest state decides keeps holding.
5. **`last_step` and `last_post_step` move on the end-state record**, as they did under
   ADR-271 item 2.

## Alternatives rejected

- **A second slot keyed at fold time** (the journal files a state under "end" or "entry" when it
  folds the record, by whether the step was a post-transfer one): wrong for adjacent plans. The
  shared record is plan 1's end and plan 2's entry, and plan 2's end state overwrites it.
- **Keying on the step's pc, or decoding `VmState.pc` in the journal**: the first depends on the
  program, the second breaks the opaque-state rule (ADR-244 item 6).
- **A new `PlanEnd` record so `last_step` does not move**: contradicts ADR-271 item 2 (no format
  change), and the adjacent case needs one record to serve as both an end and an entry.
- **Gating the fallback on the interruption point being inside the plan, or on
  `transfer.interrupted`**: misses a redo of step 4c interrupted in its blocks.

## Consequences

- A redo costs one more record: the replay's run start.
- `JournalState` holds two VM states, bounded however long the program loops.
- A `FromPc` recovery point on a non-primitive tail instruction can end a crash in the one-sync
  window between the end-state record and the completion in on-site intervention. This errs on
  the safe side, as ADR-253 notes for Wait and Log.
- A VM state over the journal's 1 MiB frame fails the job at the end step with no completion
  record, so the next restart redoes a verified transfer (backlog).
- A crash in a later plan's pre-erase steps still ends in on-site intervention (ADR-253).

# ADR-253: Restart Classification and a Write-Ahead Intent at the Recovery-Required Point

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/journaling.rs`, `src/journal.rs`), ADR-244 (record set), ADR-252 item 2

## Context

A restart (ADR-229 item 2) first decides from the journal how the job goes on. It can make a
plain start, follow the restart order of an interrupted transfer, or stop with on-site
intervention. ADR-229 item 1 places the interruption point at the later of the last confirmed
step and the request guarded by the newest write-ahead marker. ADR-245 item 3 rules a restart out
when that point is at or past a plan's recovery-required point (`RecoveryRequired::FromPc`) and
before the plan's end, or inside a section marked `RecoveryRequired`.

The journal records a step after each completed primitive inside a plan (ADR-252 item 2). Only
two requests are written ahead of their requests: the erase, by the transfer-start marker, and
RequestTransferExit, by its marker. A recovery-required point can sit on any other request, such
as a CheckMemory routine among the post-transfer steps. Take a crash after that request was sent
but before its response was recorded. The journal's last step is then the step before it, so the
point falls before the recovery-required point. The procedure forbids an automatic restart there,
and the restart would go ahead anyway.

Two ways to close this were considered:
- Treat every primitive reachable from the last step, without passing another primitive, as
  possibly sent. This needs control-flow analysis at restart time. It also sends to on-site
  intervention every crash right after the step before the point, even when the next request was
  never sent.
- Write an intent ahead of the request at the point. This costs one record per job and needs no
  analysis.

## Decision

1. **A request intent is written ahead at the recovery-required point.** The journal gains one
   record variant, `Intent { at }`, appended after the existing ones (ADR-244 item 1). It folds
   into `RecoveryFacts::last_intent`. Like the other markers, it must come after the last step,
   and intents must come in step order.

   Before the first diagnostic primitive at or past a plan's `FromPc`, and before the plan's
   end, the runner commits an intent for that instruction, once the VM's checks of it passed
   (ADR-252 item 1).
   - When that instruction is the erase or RequestTransferExit, its own marker already does
     this, and no intent is added.
   - One intent per plan is enough. The validator forbids any jump or call from at or past the
     point back before it (ADR-245 item 4), so execution never crosses the point again.
   - Any primitive counts, including a `Wait`. That errs on the side of on-site intervention.
   - Execution that comes back to a plan's entry (which the validator allows only when the
     point is the entry itself) runs the plan again and writes its intent again.
   - A cancelled job commits no intent. A request the VM refuses after the intent (a check only
     `step` makes) or a cancel after it leaves an intent for a request that was not sent, which
     also errs on the side of on-site intervention.
2. **The interruption point** is the latest, by step count, of these records:
   - the last completed step;
   - the transfer-start marker;
   - the RequestTransferExit marker;
   - the last intent.

   A marker or an intent names a request that may have been sent; a step names one that was.
3. **`restart::classify`** reads the journal (`Journal::read` or `Journal::open`) together
   with the program, contacts nothing, and decides one of three things.
   - **No journal** (`NotFound`): a plain start.
   - **An unreadable journal** (corrupt, of an unknown format version, of another job, or I/O):
     `OnSiteInterventionRequired`.
   - **An interruption point that rules a restart out**: on-site intervention. That is a point
     at or past a plan's `FromPc` and before its end, or a point inside a `RecoveryRequired`
     section. This check comes before the next two, so it holds whether or not a transfer
     started.
     - The journal records no steps after a plan, so a plan that ran to its end leaves its last
       step on its last primitive, past the point. The post-transfer completion is committed with
       the step that reached the end, and the journal stops updating the transfer's last
       post-transfer step there. When the newest transfer is complete and the point is no later
       than that step, the point is taken to be at the plan's end. A later point, such as a step
       back into the plan's entry, stands as it is. A resume record changes no step count, so it
       does not undo this.
     - The journal places a point only inside a plan, or on the step into a plan's entry, so
       only a section there can be found this way.
   - **No transfer-start marker**: a plain start, since nothing was erased.
   - **A transfer-start marker**: the restart order for the plan with the marker's stage. The
     marker alone is enough, so a lost erase response is an interrupted transfer. A stage the
     program does not declare ends in on-site intervention, and so does a plan that never allows
     a restart (`FlashRecovery::allows_restart`), since its program need not declare what a
     restart reads (ADR-245 item 6).

   A restart replays from the plan's entry, so it needs the VM state there.
   - That state is the newest one the journal holds (ADR-252: taken on the step into the entry).
     When the job started at the entry and so recorded none, it is the program's initial state.
   - It must decode, pass `Vm::check_state`, and stand at the plan's entry. Otherwise the job
     needs on-site intervention, so a version 1 state, which still decodes (ADR-245 item 7), is
     refused before anything is sent.
   - Its step count is set to one more than any record's in the journal (`restart::next_steps`),
     since the journal orders records by step count and refuses one that does not come after
     the last; `StepRef::steps` keeps counting across resumes, and the state with its new count
     is checked again. A plain start on an existing journal continues from the same count. A
     job's step limit (`JobLimits::max_steps`) then counts across its runs.
4. **The outcome lives in the classification.** `OnSiteInterventionRequired` is a variant of
   `RestartDecision`, with a reason. Wiring it into the job runner's result belongs to the
   restart entry, together with the resume limit and the voltage read.

## Consequences

- The journal format gains a record variant without a version change (ADR-244 item 1). A reader
  that predates it refuses such a journal as corrupt, which ends a restart in on-site
  intervention, the cautious outcome.
- A plan whose recovery-required point lies on a `Wait` or `Log` instruction sends a crash
  during that instruction to on-site intervention, although nothing was sent to the ECU.
- Say a job has two adjacent plans, the first completed and the crash in the second's pre-erase
  steps. The journal's newest VM state is then the second plan's entry, while its transfer is the
  first plan's. The classification refuses that pairing (`MissingEntryState`), so such a job
  ends in on-site intervention rather than a restart.
- A `RecoveryRequired` section outside every plan is not found: the runner journals nothing
  there, so a crash after a request in it was sent but before its response reads as a plain
  start.
- `RecoveryFacts`, the journal's part of the handover summary (ADR-244 item 4), gains
  `last_intent`, so a receiving device sees the intent too.

# ADR-276: A Redone Transfer Goes On from the Erase as a First Run

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/restart.rs`, `docs/ngr-agent.md`), ADR-273 item 4

## Context

ADR-229 item 2 step 4 orders a redone transfer as: the replay of the plan's steps from its entry
up to the erase (step 4b-2, ADR-273), a second check of the mutable conditions immediately
before the erase (step 4b-3), then the erase, the RequestDownload and the rest of the transfer
(step 4c). Until now the restart stopped after step 4b-3 and ended the job in
`OnSiteReason::RestartOrderUnavailable` (ADR-273 item 4), so a restart that called for a redo
never wrote anything.

Two questions are left open by the earlier decisions:

- how the redone transfer is journaled, given that the journal already holds the interrupted
  attempt, possibly with a RequestTransferExit marker and post-transfer progress (ADR-244);
- what happens when the plan's steps from its entry to its erase enter no programming session.
  The agent tracks no diagnostic session (ADR-273 consequences, and the review of step 4b-3), so
  in that case the erase is sent in the default session that step 2b-2 confirmed (ADR-265).

## Decision

1. **The replay's VM goes on from the erase as a first run.** When step 4b-3 passes, the VM the
   replay stopped at `erase_pc` (nothing at the erase journaled or sent) runs on with the
   replay's journaling and no stop point: the erase, the RequestDownload, the blocks,
   RequestTransferExit, the post-transfer steps and the rest of the program run as on a first
   run, and the job ends as that run would (completed, failed, cancelled, the step limit).
2. **The redo is a new attempt in the journal.** Its arrival at the erase commits a new
   transfer-start marker before the erase is sent, exactly as a first run's does. The journal's
   fold already starts a clean attempt on that marker: the earlier attempt's RequestTransferExit
   marker, its post-transfer progress and its `interrupted` flag go with it (ADR-229, ADR-244).
   An interruption of the redo is therefore classified as an interruption of the new attempt and
   takes the ordinary restart order; in particular the earlier exit marker no longer rules out an
   ECUReset in the next teardown. No new record type is needed.
3. **No session check before the erase.** The runner does not track or check the ECU's
   diagnostic session. A plan whose `[entry_pc, erase_pc)` re-establishes no programming session
   sends the erase in the confirmed default session. If the ECU accepts it there, the restart
   proceeds; if it does not, the erase's failure ends the job as a first-run step failure, with
   nothing erased. The transfer-start marker was committed before the erase, so a further
   restart, within the plan's resume limit, takes the restart order for that new attempt.
   Validation does not refuse such plans: whether an ECU accepts the erase outside a programming
   session is the ECU's behaviour, which the program cannot declare.
4. **The restart's ending before the erase goes.** `OnSiteReason::RestartOrderUnavailable` is no
   longer produced and is removed (superseding ADR-273 item 4). The restart order up to the erase
   is a separate function (`runner::restart_order`), so the tests can stop there and inspect the
   teardown, the confirmation and the state check (`OnSiteReason::StoppedBeforeErase`, which
   exists in test builds only).

## Alternatives rejected

- **A session check before the erase** (refuse, or end in on-site intervention, when the replayed
  range enters no programming session): the agent would need to track sessions, which it
  deliberately does not, and a static check over the range cannot see a session change made by
  a routine or an ECU that accepts the erase in its default session.
- **A dedicated "redo" record** before the transfer-start marker: the transfer-start marker
  already starts a clean attempt, and a new record type changes the journal format.

## Consequences

- A restart that redoes the transfer now erases and writes the ECU. Everything before the erase
  is unchanged: the resume count, the gates, the teardown, the default-session confirmation, the
  identity and state checks, step 4a, the replay and step 4b-3.
- A crash during the redo is bounded by the plan's resume limit, as any restart is.
- The restart end-to-end scenarios against `sim-ecu` can now complete a redone transfer.

# ADR-269: Only the Interrupted Pass's Completion Sends a Restart to Read-Back Verification

**Date:** 2026-10-10
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `docs/ngr-agent.md`), design 8.2.5, ADR-229 item 2 step 3, ADR-268 item 5

## Context

ADR-229 item 2 step 3 and design 8.2.5 send a restart straight to read-back verification when
the ECU reports the intended version and the journal records the procedure's post-transfer
steps as complete; ADR-268 item 5 restates the rule. The reasoning is that only a response or
the journal update was lost after the image was validated.

A program can step back into a plan's entry after that plan completed and run it again, for
example to write several logical blocks in a loop; the journal already counts such a step as
the start of a new pass (`restart::effective_point`). The completion recorded by the earlier
pass stays in the journal until the later pass commits its transfer-start marker. A restart
interrupted in the later pass before that marker therefore finds the completion recorded, and,
when the ECU already reports the intended version (the earlier pass may have written it), the
rule as worded skips the later pass. The reasoning behind the rule does not hold there: the
later pass's own steps never completed.

## Decision

1. **The completion counts only for the interrupted pass.** Read-back verification is chosen
   only when the post-transfer completion is journaled and the interruption point is no later
   than that pass's last post-transfer step. A step journaled after the completion (a step back
   into the plan) puts the interruption point later; the restart then treats the steps as not
   complete, so the intended version leads to redoing the transfer.
2. **The same reading applies wherever the restart relies on the completion.** The teardown's
   completed path (ADR-264, ADR-265) uses the same rule (`restart::teardown` calls
   `completed_pass_interrupted`). A completion an earlier pass left behind is not a
   RequestTransferExit with unfinished post-transfer steps either, since that pass's steps
   finished, so the teardown of a later pass follows the gates' decision.

## Consequences

- A program that runs a plan more than once never has a later pass skipped by a restart; at
  worst a pass whose image is already in place is written again, which the resume limit
  bounds.
- A single-pass program behaves exactly as before: its interruption point after the completion
  is the completion itself.
- The teardown of a later pass, before its erase, sends the ECUReset the gates allow instead of
  none. A replay that crashed with only its run start committed changes no fact, so the earlier
  pass still counts as the interrupted one there, for the teardown and the state check alike;
  the default-session confirmation and its passive retry cover an ECU the replay left in its
  programming session.

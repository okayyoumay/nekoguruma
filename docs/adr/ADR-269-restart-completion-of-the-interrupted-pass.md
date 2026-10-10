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
   completed path (ADR-264, ADR-265) is to use the same rule; until it does, a later pass's
   restart skips the ECUReset and the passive wait, and the default-session confirmation with
   its passive retry still runs.

## Consequences

- A program that runs a plan more than once never has a later pass skipped by a restart; at
  worst a pass whose image is already in place is written again, which the resume limit
  bounds.
- A single-pass program behaves exactly as before: its interruption point after the completion
  is the completion itself.
- The teardown still uses the completion alone until it is changed to the same rule.

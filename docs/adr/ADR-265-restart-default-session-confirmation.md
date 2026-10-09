# ADR-265: The Restart Confirms the Default Session, with One Passive Retry

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`), `sim-ecu` (`EcuConfig::startup_ms`, `docs/simulated-ecu.md`), design 8.2.5, ADR-229 item 2, ADR-264 item 5

## Context

ADR-229 item 2 step 2 ends the teardown with a confirmation that the ECU is back in its default
session, read from the active-session data identifier F186. In every case, the completed path
and the passive teardown included, the agent first waits the ECU startup time the procedure
declares, since a reset may have been accepted while the ECU still restarts, and then reads
within the declared confirmation window. On the completed path, a failed confirmation is
followed by the passive teardown and one more confirmation; when a confirmation cannot be
had, the job ends in on-site intervention. ADR-264 built the teardown and left the
confirmation to this step.

Points ADR-229 leaves open:

- **What one confirmation is.** A single read, or reads repeated within the window.
- **Which value confirms.** F186 reports the session by its number.
- **Which failures get the passive retry.** ADR-229 spells it out for the completed path. After
  an accepted reset the ECU's state is as unknown as on the completed path; after a passive
  teardown, the session has already been waited out.

## Decision

1. **One confirmation is a startup wait and a window of reads.** The agent sends nothing for the
   declared ECU startup time, then reads F186 until it confirms or the declared confirmation
   window, counted from the end of the startup wait, has passed, pausing the job's poll
   interval between reads. At least one read is made. A cancel ends the waits and the reads.
2. **Only a positive response naming the default session (01) confirms.** A negative response,
   no answer, a worker failure, another session or any other answer is a failed read, retried
   while the window lasts. Failed reads are logged.
3. **One passive retry when no passive teardown has run.** After an accepted reset or on the
   completed path, a failed confirmation is followed by the passive teardown of ADR-264 (the
   session timeout plus the margin, sending nothing) and one more confirmation. After a
   passive teardown, a failed confirmation is final: waiting the session out again would change
   nothing.
4. **A final failure needs someone on site.** It ends the job in
   `OnSiteReason::DefaultSessionNotConfirmed`, which carries the teardown that ran. On success
   the restart still ends in `RestartOrderUnavailable`, now carrying the confirmation as well,
   until step 3 exists.
5. **The simulator can stay silent after a reset.** `sim-ecu` gains `EcuConfig::startup_ms`:
   after an ECUReset the simulated ECU answers nothing for that long, so a slow restart can be
   tested against it.

## Consequences

- A restart now spends at least the startup time and one read after its teardown, and up to
  two startup waits, two windows and one passive teardown before it gives up.
- An ECU that reports a non-default session for the whole window is treated like one that does
  not answer: both get the passive retry when no passive teardown ran.
- ADR-229 lets a power-down time in the reset's response extend the startup wait. None is
  applied: the teardown accepts only a hard reset's positive response, which carries none, and
  the response to an ECUReset among the procedure's own post-transfer steps is not journaled,
  so a restart on the completed path does not know it.
- ADR-229 item 2 ended a failed confirmation after an accepted reset in on-site intervention at
  once; item 3 here adds the passive retry there too, as for the completed path.
- Nothing about the confirmation is journaled: a crash during it leads to another restart, which
  runs the gates, the teardown and the confirmation again.

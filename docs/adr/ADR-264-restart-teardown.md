# ADR-264: The Restart's Teardown Sends One Hard Reset or Waits Out the Session

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`), `sim-ecu` (`Fault`, `docs/simulated-ecu.md`), ADR-229 item 2, ADR-255 item 5, ADR-261 item 2

## Context

ADR-229 item 2 step 2 ends the download an interrupted ECU may still hold with a teardown:
an ECUReset once the identity and safety gates allow it and the journal does not rule it out,
otherwise a passive teardown that stops TesterPresent and waits out the session timeout and
the margin the procedure declares (`RecoveryTiming`). An ECU that refuses the reset, or whose
answer is lost, is torn down passively as well. On the completed path (the post-transfer
steps journaled as complete) no reset is sent and the default-session confirmation comes
first. ADR-261 built the gates; the restart then ended in `RestartOrderUnavailable` carrying
their decision, with nothing torn down.

Points the design and ADR-229 leave open:

- **Which reset.** ECUReset has several reset types. The procedure declares none for the
  teardown; its own resets are ordinary requests in its code.
- **What the passive teardown stops.** The agent runs no TesterPresent of its own: a job keeps
  a session alive only through requests its procedure sends.
- **What counts as a refusal and what as an unknown outcome.**

## Decision

1. **The order.** The teardown applies the journal's exclusions first, then the gates'
   decision:
   - the completed path: no reset and no wait (`Teardown::CompletedPath`); the confirmation of
     the next step comes first, as ADR-229 orders;
   - a RequestTransferExit intent journaled without the post-transfer steps complete: passive
     (`TransferExitJournaled`), whatever the gates said;
   - a gate that did not pass: passive, with the gate's reason;
   - otherwise one ECUReset.
2. **One hard reset, with its answer required.** The reset is ECUReset with the hard reset
   type, sent without suppressing its positive response, so that a refusal and a lost answer
   can be told from an acceptance. A hard reset is the type an ECU is most likely to support,
   and it ends a programming session on any ECU that implements the service. A procedure that
   needs another type can be given a declared reset later.
3. **Refused or unknown falls back to passive.** A negative response is `ResetRefused` with its
   response code. No answer, a worker failure, a request the agent's policy refuses, or an
   answer that is neither positive nor negative is `ResetOutcomeUnknown`, and is logged. Both
   take the passive teardown; the reset is not repeated.
4. **The passive teardown waits.** Since the agent runs no TesterPresent, there is nothing to
   stop: the passive teardown sends nothing further (after a refused or unknown reset, the reset
   was the last request) for the declared session timeout plus the margin,
   in steps of the job's poll interval, and a cancel ends the wait. A cancel before the reset
   is sent sends none; one that arrives while the reset is on its way does not hide an accepted
   reset, which is reported as `Teardown::Reset`. An accepted reset is not
   followed by a wait here: the ECU's startup time belongs to the confirmation.
5. **The outcome is carried.** `RestartOrderUnavailable` now carries the teardown that ran
   (`Teardown::Reset`, `Teardown::Passive(cause)` or `Teardown::CompletedPath`) instead of the
   gates' decision, until the confirmation (the next step) exists.
6. **A simulator fault for refusals.** `sim-ecu` gains `Fault::NegativeResponse { nrc }`,
   which refuses the next request that reaches it with that response code and changes nothing,
   so the refusal path is tested against the simulator as the other faults are.

## Consequences

- A restart now sends an ECUReset when everything allows it, the first request of the restart
  that changes the ECU. Under the maintainer's condition on ADR-261 no caller outside tests
  passes a target VIN yet, so outside tests the gates never allow it and the teardown is
  always passive.
- A passive teardown holds the job, its link and its locks for the declared session timeout
  plus the margin. Nothing but a cancel bounds that wait: the values come from the signed
  procedure, as the session timeout itself does.
- The reset type is fixed. An ECU that refuses a hard reset is torn down passively, which is
  slower but safe.
- The teardown is not journaled: a crash during it leads to another restart, which runs the
  gates and the teardown again and counts another resume.

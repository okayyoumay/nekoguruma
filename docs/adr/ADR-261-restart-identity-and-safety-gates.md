# ADR-261: The Restart's Identity and Safety Gates

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `src/journaling.rs`, `docs/ngr-agent.md`), ADR-255 item 5

## Context

ADR-229 item 2 step 2 orders what a restart does after its checks that need no ECU service and
before it tears down the download the ECU may still hold: it reads the VIN, then the ECU's
hardware identity, then the safety preconditions the procedure declares, and only when all of
them hold may the teardown send an ECUReset. A decoded VIN of another vehicle aborts the job;
every other outcome leaves only the passive teardown. ADR-255 item 5 ended a restart right after
step 1, with nothing sent to the ECU.

Three points are not settled by ADR-229 or the design:

- **The job's VIN.** The restart compares the VIN it reads with "the job's VIN". The journal
  records the hardware part number and the pre-erase software version (ADR-244 item 4,
  ADR-252), but no VIN: ADR-244 counts the target VIN among what the job itself names, next to
  the journal's facts. The runner's entry points take no VIN.
- **Which source a precondition is read from.** A precondition declares one source for the
  default session and one for the programming session (ADR-245). At step 2 the ECU's session is
  not known: after an interrupted transfer it may still be in its programming session, which is
  why a teardown is needed at all.
- **A failure to reach the worker.** ADR-229 names an ECU that does not answer, answers
  negatively or answers with a value that cannot be decoded. A worker that cannot be reached, or
  a refused request, is not named.

## Decision

1. **The job names its target VIN.** `JournalSetup` carries `vin: Option<String>`, the VIN
   the job targets, as the job's own data beside the journal's facts (ADR-244 item 4). The
   journal does not record it. A job that names none is never counted as identified: its
   restart takes the passive teardown without reading anything.
2. **The gates give a decision; the teardown acts on it.** `restart::check_gates` runs after
   `check_before_ecu` and returns a `TeardownGate`: `ResetAllowed`, or `PassiveOnly` with the
   first gate that did not hold (`PassiveReason`). The gates stop at the first one that does not
   hold, since a later one cannot make the decision better. Until the teardown is implemented,
   the restart ends in `OnSiteReason::RestartOrderUnavailable` carrying the decision.
3. **Only a decoded VIN of another vehicle aborts.** A VIN read that decodes to text other than
   the job's ends the job in `JobError::IdentityMismatch` (design 5.6 `Interrupted -> Failed`).
   No answer, a negative response, a value that does not decode, or a VIN source the procedure
   does not map gives `PassiveOnly(VinNotEstablished)`. At this step a hardware part number that
   differs from the journal's is not an abort but `PassiveOnly(HardwareIdentityDiffers)`:
   ADR-229 makes it an abort only in step 3, once the ECU is confirmed in its default session.
   The hardware part number is compared as the raw field bytes the journal recorded.
4. **A precondition is read from either session's source.** The default-session source is read
   first. When it gives no value and the procedure declares a different programming-session
   source, that one is read too. A value outside the declared range fails the gate at once.
   Both reads change nothing on the ECU, and a value the ECU gives is valid whichever session
   it was read in; a read in the wrong session only fails, which leaves the passive teardown.
   A precondition with no source, or one whose service source the agent's source table does
   not map, cannot be established and fails the gate.
5. **A failure to use the worker fails the gate.** A transport failure, a request the worker
   refuses or a lost response during a gate read counts as a read that gave no value, and is
   logged. This is how step 1 treats the supply voltage (ADR-255), and it fails safe: the
   gate leaves only the passive teardown, whose own confirmation fails if the worker stays
   unreachable.
6. **No VIN in errors or logs.** `IdentityMismatch` names the identity that differs, not its
   values, the gates log no VIN, and `JournalSetup`'s `Debug` output redacts it (design 5.5,
   16.2).

## Consequences

- ADR-255 item 5's "nothing sent to the ECU" no longer holds: a restart that passes step 1 now
  sends the gate reads (ReadDataByIdentifier through the declared sources) and nothing that
  changes the ECU. Its entry-state consequence still holds, since nothing is replayed.
- The teardown itself, the default-session confirmation and the exclusion of the reset after
  RequestTransferExit remain to be built on this decision.
- The promotion to the per-vehicle lock at the first matching VIN (ADR-229 item 2, ADR-256
  item 6) is not part of the gates; it needs the per-vehicle lock first.
- A first run does not yet compare the VIN with the job's: that is the VIN match of the
  execution preconditions (design 8.9.1), which come with the job policy.

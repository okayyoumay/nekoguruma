# ADR-263: The Restart Promotes to the Per-Vehicle Lock at Its VIN Match

**Date:** 2026-10-09
**Status:** Accepted (the step 3 fallback consequence settled by ADR-267)
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`), ADR-229 item 2, ADR-256 item 6, ADR-261, ADR-262

## Context

Design 8.8's two-stage locking and ADR-229 item 2 make a restart promote from its per-VCI lock
to the per-vehicle lock as soon as a VIN read first matches the job's VIN, before any further
ECU traffic. ADR-261 built the VIN read into the restart's gates (`restart::check_gates`) but
left the promotion out, since the per-vehicle lock did not exist; ADR-262 added it as
`JobGuards::take_vehicle`, with nothing calling it.

Three points are left open:

- **Where the promotion runs.** The gates read the VIN, then the hardware identity, then the
  safety preconditions. "Before any further ECU traffic" puts the promotion between the VIN
  read and the hardware read, inside the gates.
- **Where the guards are while it waits.** During a run the job's `JobGuards` sit in the
  runner's guard slot (a mutex shared with the job thread's link teardown), and `take_vehicle`
  needs them by `&mut` and may wait for another job for as long as that job runs.
- **What a promotion that fails does.** The lock can fail for reasons the job cannot fix (a lock
  file that is not a regular file, an I/O error), and it can be cancelled while it waits.

## Decision

1. **The gates call the promotion right after the VIN matches.** `check_gates` takes a
   promotion callback and calls it once, after the ECU's VIN read matches the journal's target
   VIN and before the hardware identity read. A VIN that is not established, a job without a
   target VIN and a VIN of another vehicle never call it. Its error ends the gates.
2. **The wait does not hold the guard slot.** The runner's callback takes the guards out of the
   slot, with the slot's mutex held only for that, calls `take_vehicle` with the job's poll
   interval and cancel flag, and puts the guards back. Putting them back is done by a value's
   `Drop`, so an unwind during the wait still returns them. Nothing else uses the slot while
   the job thread waits; keeping the mutex free keeps it that way should that change.
3. **The lock stays with the guards.** The promoted lock is held in the `JobGuards` and comes
   back with them from `resume_program_journaled`, so a job that runs again on those guards
   keeps it (ADR-262 item 4), and taking the same vehicle again returns at once.
4. **A cancel is a cancel; any other failure ends the job before ECU changes.** A cancel during
   the wait ends the job in `JobError::Cancelled`. Any other lock failure ends it in
   `JobError::VehicleLock`, which names no VIN and no bucket (design 16.2, ADR-262). At that
   point the restart has sent only the VIN read, so nothing that changes the ECU has been sent;
   the job does not go on to the hardware read without the lock. A guard slot found empty, which
   only a bug in the runner could cause, ends the job in `JobError::GuardsMissing` instead of a
   panic.

## Consequences

- A restart whose vehicle is held by another job (a job on another VCI, or one on a vehicle in
  the same bucket, ADR-262) waits with its link open and its VCI lock and slot held, sending
  nothing. If the ECU's session times out meanwhile, it does so as it would have during a
  passive teardown; the rest of the restart order confirms the session anyway.
- The resume count is committed in step 1, before the gates, so a restart that is cancelled or
  fails while it waits for the vehicle has used one resume.
- Until the journal's protection lands, no caller outside tests passes a target VIN (the
  maintainer's condition on ADR-261), so the promotion runs only in tests; it needs no change
  when callers start passing one.
- The step 3 fallback of ADR-229 (promotion at the step 3 VIN match when step 2 could not read
  the VIN) and the first run's VIN match among the execution preconditions (design 8.9.1) are
  not part of this decision; they can call the same promotion.

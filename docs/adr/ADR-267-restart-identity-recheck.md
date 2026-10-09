# ADR-267: The Restart Re-reads the Identity in the Default Session and Promotes There Too

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`), ADR-229 item 2 step 3, ADR-263, ADR-265 item 4

## Context

ADR-229 item 2 step 3 starts once the ECU is confirmed back in its default session (ADR-265).
The VIN and the hardware identity must be read again there, even when step 2 matched them: a
decoded value that differs aborts the job, and any other outcome ends it in
`OnSiteInterventionRequired`. The per-vehicle lock is taken at the first VIN match, in step 2
or, when step 2 could not read the VIN, in step 3 (ADR-229 item 2 step 1, design 8.8). ADR-263
built the promotion into step 2 and left the step 3 fallback open.

Points that ADR-229 leaves to the implementation:

- **How step 3 knows whether step 2 promoted.** Step 2 can fail to read the VIN, or read it and
  pass the promotion, and then fail a later gate; the restart goes on to the teardown either
  way.
- **What the on-site ending carries.** The technician needs to know what the restart has
  already done to the ECU, in particular whether an ECUReset was sent.
- **What counts as decoded.** The step 2 gates already treat a VIN that is not well-formed as
  not established rather than as a different vehicle.

## Decision

1. **Step 3 calls the same promotion on every VIN match.** It does not track whether step 2
   promoted. `JobGuards::take_vehicle` returns at once when the guards already hold that
   vehicle (ADR-262 item 4, ADR-263 item 3), so a second call after step 2's promotion costs
   nothing, and a restart whose step 2 could not read the VIN takes the lock here, before the
   hardware identity read. Both promotions use the journal's target VIN, so step 3 cannot lock
   another vehicle than step 2 did. A promotion failure or a cancel during its wait ends the
   job as in ADR-263 item 4.
2. **Decoded means what step 2 means.** A VIN is decoded when it is a well-formed VIN; a
   hardware identity when the declared field reads in full. A decoded value that differs ends
   the job in `JobError::IdentityMismatch`. Anything else (no target VIN or no source declared,
   no answer, a negative response, a malformed value, a worker failure) ends it in the new
   `OnSiteReason::IdentityNotEstablished`, naming the identity.
3. **The on-site ending carries the teardown.** `IdentityNotEstablished` carries the teardown
   that ran (`Teardown`), as `DefaultSessionNotConfirmed` does (ADR-265 item 4), so the
   technician can tell an accepted ECUReset from a passive teardown.
4. **The order is fixed.** The VIN is read first, the promotion follows its match, and the
   hardware identity is read only after the promotion; nothing else is sent in step 3a. A
   restart that passes it still ends in `RestartOrderUnavailable` until the ECU state check
   (the rest of step 3) exists.

## Consequences

- Until callers outside tests pass a target VIN (the maintainer's condition on ADR-261), every
  restart outside tests now ends in `IdentityNotEstablished` for the VIN after the teardown and
  the default-session confirmation, where it ended in `RestartOrderUnavailable` before. Nothing
  more is sent to the ECU than before.
- A restart whose step 2 could not read the VIN may wait for another job's vehicle lock after
  the confirmation, with its link open and nothing sent, as a step 2 promotion waits.
- The VIN and the hardware identity are read twice per restart. Both reads are
  ReadDataByIdentifier requests that change nothing on the ECU.

# ADR-261: The Restart's Identity and Safety Gates

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `src/journaling.rs`, `src/journal.rs`, `docs/ngr-agent.md`), ADR-244, ADR-255 item 5

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

1. **The job names its target VIN, and the journal records it at creation.** `JournalSetup`
   carries `vin: Option<Vin>`, the VIN the job targets. A first run writes it as the journal's
   first record (`Record::TargetVin`), in the same write that creates the file, so a journal
   exists with its target or not at all. A resume must name the same VIN, none included; one
   that does not ends in `OnSiteInterventionRequired(TargetVinDiffers)` before the
   classification, with nothing sent and no resume counted, since the job's own data changed
   between runs and the connected vehicle may be one the job never targeted. The resume input
   keeps its VIN, so a wrong journal (a reused job id, a wrong directory) is caught as well as a
   wrong caller. The gates then compare the ECU's VIN against the journal's, as they compare the
   hardware part number. A job that names none is never counted as identified: its restart takes
   the passive teardown without reading anything. `Vin`'s `Debug` output is redacted. The record
   is a consistency check between the job's runs, not tamper evidence; that comes with the
   journal's protection (design 5.5).
2. **The gates give a decision; the teardown acts on it.** `restart::check_gates` runs after
   `check_before_ecu` and returns a `TeardownGate`: `ResetAllowed`, or `PassiveOnly` with the
   first gate that did not hold (`PassiveReason`). `ResetAllowed` means the gates do not rule a
   reset out; the journal's own exclusions (no reset once RequestTransferExit was journaled,
   none on the completed path) belong to the teardown, which applies them on top. The gates stop
   at the first one that does not hold, since a later one cannot make the decision better. Until
   the teardown is implemented, the restart ends in `OnSiteReason::RestartOrderUnavailable`
   carrying the decision.
3. **Only a decoded VIN of another vehicle aborts.** A VIN read that decodes to a well-formed
   VIN other than the job's ends the job in `JobError::IdentityMismatch` (design 5.6
   `Interrupted -> Failed`). Well-formed means 17 characters, each a digit or an upper-case
   letter other than I, O and Q. Text that is not a well-formed VIN (a padded field, lower case,
   an empty answer) does not count as decoded: an abort is a verdict that the ECU belongs to
   another vehicle, and malformed text proves nothing of the kind. No answer, a negative
   response, a value that does not decode, or a VIN source the procedure or the agent's source
   table does not map gives `PassiveOnly(VinNotEstablished)`. A job VIN that is not well-formed
   counts as no job VIN. At this step a hardware part number that differs from the journal's is
   not an abort but `PassiveOnly(HardwareIdentityDiffers)`: ADR-229 makes it an abort only in
   step 3, once the ECU is confirmed in its default session. The hardware part number is
   compared as the raw field bytes the journal recorded.
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
   values, the gates log no VIN, and `Vin`'s `Debug` output redacts it (design 5.5, 16.2).

## Consequences

- ADR-255 item 5's "nothing sent to the ECU" no longer holds: a restart that passes step 1 now
  sends the gate reads (ReadDataByIdentifier through the declared sources) and nothing that
  changes the ECU. Its entry-state consequence still holds, since nothing is replayed.
- The teardown itself, the default-session confirmation and the exclusion of the reset after
  RequestTransferExit remain to be built on this decision.
- The promotion to the per-vehicle lock at the first matching VIN (ADR-229 item 2, ADR-256
  item 6) is not part of the gates; it needs the per-vehicle lock first.
- The journal holds the target VIN in the clear until the journal's protection (design 5.5)
  encrypts it. Design 5.5 already places VINs in the journal, under its retention and deletion
  rules, which is what ADR-256 item 6 found missing for a lock file's name.
- A journal written before the `TargetVin` record names no VIN: it resumes only with none, and
  a restart from it takes the passive teardown.
- `Journal::create` takes the target VIN. ADR-244's record set gains `TargetVin`, which must
  be the first record and appears at most once.
- The target VIN record shares the creation write with the header and is synced before the
  file has its name, so a crash cannot tear it. Storage damage that zeroes it while it is still
  the journal's last frame reads as a torn tail and is cut off (ADR-244 item 5), as the same
  damage would cut any other last frame. Such a journal has no records and names no VIN: it
  classifies as a plain start either way, a resume naming the job's VIN ends in
  `TargetVinDiffers`, and one naming none runs the program from its start as a first run would.
  The job's generation then has no VIN, so a later interrupted transfer gets only the passive
  teardown. A header flag could make this case corrupt instead, at the price of a format version
  bump and a two-version reader, for an ending that is already fail-safe; this is accepted.
- A placeholder that a bootloader may answer after an erase and that happens to be a
  well-formed VIN (for example seventeen zeros) still aborts the job. Nothing tells it apart
  from another vehicle's VIN; the job ends without a reset either way.
- An abort is not journaled as a terminal state: like the on-site endings, a later resume of the
  same job counts another resume and runs the gates again.
- When the ECU does not answer its ECU-sourced precondition reads, the gates can take up to two
  request timeouts per precondition. A start deadline, which a server's job instruction
  carries, would bound this.
- A first run does not yet compare the VIN with the job's: that is the VIN match of the
  execution preconditions (design 8.9.1), which come with the job policy.

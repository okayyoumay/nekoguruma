# ADR-268: The Job Names Its Intended Software Version, and the Journal Records It

**Date:** 2026-10-10
**Status:** Accepted (item 5's completion counts only for the interrupted pass since ADR-269)
**Affects:** `agent` (`src/journal.rs`, `src/runner.rs`, `src/restart.rs`, `tests/journal_crash.rs`, `docs/ngr-agent.md`), ADR-229 item 2 step 3, ADR-244 items 4 and 5, ADR-261 item 1

## Context

The ECU state check of ADR-229 item 2 step 3 compares the software version the ECU reports with
two others: the version read before the first erase, which the journal records (ADR-244,
ADR-252), and the version the job writes. Read-back verification after a restart needs the
latter as well. Nothing in the agent knows it: ADR-244 item 4 and design 8.2.5 count "the
version being written" among what the job itself names, next to the journal's facts, and the
runner's entry points take none. ADR-261 closed the same gap for the target VIN.

The version is a property of the image a job writes, not of the procedure: one flash procedure
writes many images, and the IR declares only the sources an identity is read from (ADR-244
item 7, ADR-245). It therefore cannot come from the program.

## Decision

1. **The job names it, and the journal records it at creation.** `JournalSetup` carries the
   intended software version as an optional value. A first run writes it as a new record
   (`Record::IntendedSoftwareVersion`) in the write that creates the journal, right after the
   target VIN record (first when the job names no VIN). `RecoveryFacts` carries it, so the
   checkpoint summary carries it unchanged with the other facts.
2. **The creation records form the journal's prefix.** The target VIN may only be the first
   record and the intended version only the first, or the second after a target VIN; each
   appears at most once. Anything else is refused on commit and makes the journal corrupt on
   read-back, as ADR-261 ruled for the VIN alone.
3. **A resume must name the same value.** A resume whose intended version differs from the
   journal's, a missing one on either side included, ends in
   `OnSiteReason::IntendedVersionDiffers` right after the target VIN check, before anything is
   sent and before a resume is counted: the job's own data changed between runs.
4. **Raw field bytes, compared bytewise.** The value is the software-version field as the
   procedure's source reads it, the same representation as the journal's pre-erase version, so
   the state check compares all three with one rule and no decoding rule can make two answers
   equal (ADR-252 item 5). A job that reflashes the version already installed names the same
   bytes as the pre-erase version. An empty value is refused (on creation, on commit and on
   read-back), since it would match an ECU that answers with an empty field.
5. **The state check that uses it** (the rest of ADR-229 item 2 step 3) reads the version with
   the procedure's retry limit; only a field read in full is decoded, the declared
   no-application response is conclusive, and anything else is retried, then ends in on-site
   intervention. Without an intended version the job can never recognise its own image: the
   pre-erase version or the no-application response redo the transfer, and any other version
   needs someone on site; read-back verification is never chosen. Until step 4 and read-back
   verification exist, the decision is carried in `RestartOrderUnavailable`.

## Consequences

- ADR-244 item 4's list of what the job names loses the version: it is a journal fact now.
- A journal with the new record is refused as corrupt by an older build, which ends the job in
  on-site intervention, as ADR-261's record did. The other way round, a journal written before
  this change holds no version, so a resume by a newer build that names one ends in
  `IntendedVersionDiffers`.
- A torn creation write that loses the version frame while it is the journal's last frame
  leaves a journal with the VIN and no version. A resume that names a version then ends in
  `IntendedVersionDiffers`; one that names none runs and can never reach read-back
  verification. Both are on the safe side.
- The job's instruction must give the version as the bytes the ECU answers for the field, or
  as text together with the field's encoding. The server's job instruction carries no software
  version yet; which form it takes is left to the server-side work.
- Unlike the VIN (ADR-261), a software version is not personal data, so no condition on
  passing it before the journal's protection lands applies.

# ADR-344: The Write-Job Journal Binds the Flash Artifact's Hash

**Date:** 2026-10-22
**Status:** Accepted
**Affects:** `agent` (`journal.rs`: `Record::ArtifactSha256`, `RecoveryFacts::artifact_sha256`,
`Journal::create`; `runner.rs`: the creation of a job's journal and the restart order;
`restart.rs`: `OnSiteReason::ArtifactDiffers`), design 8.2.5 and 11.3; ADR-229, ADR-244,
ADR-261, ADR-268, ADR-318, ADR-327, ADR-336

## Context

A flash job names its artifact by content hash, and the agent hashes the bytes at job check
before the first instruction (ADR-336 item 12): a file that does not match the job's hash is
refused. Within one process the host holds the bytes for the whole job, so every block of a run
comes from the same file.

A resume after a process restart (an agent crash, a power loss on the device) is a new process,
and the caller passes the job, its artifact included, again. Nothing compares that artifact
with the one the first run used. A resume handed another artifact, correctly named by its own
hash, passes job check and redoes the plan with other bytes, although the journal's facts (the
identity read, the pre-erase software version, the last confirmed block) describe a transfer of
the first file. The job's other data have such a check: the target VIN (ADR-261) and the
intended software version (ADR-268) are recorded when the journal is created and compared on
every resume. ADR-318 notes that a restart given another document goes undetected; ADR-327
closes that for a restart once the journal has reached a flash plan, through the hash its
combination or guard record holds. The artifact has no such binding.

The maintainer decided the shape (in the review of the agent's artifact): the journal records
the artifact's hash when it is created and every resume compares it with the job's; a mismatch is
refused before anything is sent, and once the journal has reached a flash plan it ends in on-site
intervention, as ADR-327 decides for another document. Left open: what "reached a flash plan"
means for the journal, how a journal that holds no hash is treated, and how the refusal before
the plan is reported.

## Decision

1. **The hash is part of the journal's creation prefix.** `Journal::create` takes the job's
   artifact hash and, when the job carries an artifact, writes `Record::ArtifactSha256` (the
   32-byte SHA-256 the job names, appended to `Record`) in the same write as the header and the
   target VIN and intended software version, after them, so the journal exists with it or not at
   all. The record folds into `RecoveryFacts::artifact_sha256` and is accepted only in that
   prefix, once. The journal format version does not change: the record is an appended variant,
   which an older agent reads as a corrupt journal and so asks for on-site intervention
   (`UnreadableJournal`), never a silent resume. A handover's checkpoint summary carries the fact
   with the others.
2. **Every resume compares it, before anything is sent.** In the restart order, after the
   terminal records (ADR-280, ADR-286, ADR-288), the target VIN and the intended software
   version (ADR-261, ADR-268) and the classification, the resume compares the recorded hash with
   the hash of the artifact it was given. The comparison applies only where the classification
   would go on with the job (a plain start, a resume from a checkpoint or a restart of the
   transfer): when it asks for on-site intervention, that decision and its reason stand, so
   another artifact never turns a job that needs a person into one a server may issue again. They differ when the two hashes differ, when the
   journal records one and the resume has no artifact, and when the resume has one and the
   journal records none. A journal written before this decision records none, so a resume of it
   given an artifact counts as differing. Nothing is sent and no resume is counted in either
   outcome below.
3. **Before a flash plan, a refusal; from the plan on, on-site intervention.** The journal has
   reached a flash plan when it records a transfer-start marker (`RecoveryFacts::transfer`) or
   its last completed step or its newest request intent (ADR-253) lies within a plan's
   boundaries, from its entry to its post-transfer end (`FlashRecovery`).
   - **Not reached:** the job ends in `JobError::ArtifactDiffersFromJournal`. Nothing
     irreversible was sent under the old artifact. The job is refused rather than run with the
     new bytes, so a job's data cannot change between runs. A server may issue a new job with the
     new artifact.
   - **Reached:** the job ends in on-site intervention (`OnSiteReason::ArtifactDiffers`), as
     ADR-327 decides for another document past the plan's entry. The erase may already have
     destroyed the application that the first artifact was to replace, and the journal's
     transfer facts belong to the first file.
4. **What it does not cover.** The comparison binds the artifact a resume is given to the one
   its journal was created with. Whether that artifact is the right one for the ECU stays with
   the job's signature and ADR-336's checks at job check. A job without a journal (a program
   that sends nothing it cannot repeat, ADR-335 item 1) records nothing and compares nothing.

## Alternatives rejected

- **On-site intervention for every mismatch, like the target VIN and the intended version
  (ADR-261, ADR-268).** Those are the job's own identity, and a change means the job is not the
  one that ran. The artifact can be replaced between runs for a reason that leaves the ECU
  untouched, such as a corrected file before the plan started. The maintainer chose the refusal
  there, which sends nothing and lets the server issue a new job.
- **Bump the journal format version.** Appended variants are how every record since the first
  format was added (ADR-253, ADR-272, ADR-280, ADR-335); an older agent already refuses a
  record it cannot decode, and a version bump would refuse the journals of jobs in flight too.
- **Compare before the classification.** A journal that the classification sends to on-site
  intervention (a request whose outcome is unknown, a recovery-required point already passed)
  would end in the refusal instead, which tells a server it may issue the job again while the
  ECU may need a person.
- **Accept a journal with no recorded hash as matching.** A journal written before this
  decision cannot show which file its transfer used. Treating it as matching would leave the gap
  this decision closes open for every job that started before the upgrade.
- **Compare the artifact's bytes against the journal's confirmed blocks instead.** The journal
  records block numbers, not bytes, and a check that reads ECU memory would send requests
  before the decision is made.

## Consequences

- A resume after a process restart with another artifact never redoes a plan with other bytes:
  before the plan it is refused, from the plan on it waits for a person. Design 8.2.5 and 11.3
  record it, and design 8.2.5's sentence that a restart reads the same bytes now says why.
- A job in flight when the agent is upgraded to this version has a journal without the hash. A
  resume of a flash job of that journal is refused before its plan and ends in on-site
  intervention past it. Such a job is rare (an agent upgrade while a write job is interrupted)
  and the outcome is the safe one.
- `RecoveryFacts` gains one field, and with it the checkpoint summary a handover carries.

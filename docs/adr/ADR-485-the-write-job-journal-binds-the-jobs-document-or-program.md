# ADR-485: The Write-Job Journal Binds the Job's Document or Program

**Date:** 2026-11-17
**Status:** Accepted
**Affects:** `agent` (`journal.rs`: `Record::JobContent`, `JobContent`, `JournalState::job_content`,
`Journal::create`; `journaling.rs`: `JobJournal::create`; `runner.rs`: `run_job`, the restart order;
`restart.rs`: `OnSiteReason::JobContentDiffers`; `job_report.rs`), design 8.2.5 and 8.2.6;
ADR-244, ADR-261, ADR-268, ADR-318, ADR-327, ADR-344

## Context

A restart is a new process that the caller hands the job again: its program, and for a document
job the document the program comes from. The journal compares what the job names against what
the first run recorded for the target VIN (ADR-261), the intended software version (ADR-268) and
the flash artifact (ADR-344), but nothing compared the program or the document. A restart given
a replaced one resumed the journal's progress with other steps, so the journal's step counters,
checkpoints and transfer facts no longer described the program that ran. ADR-318's Consequences
left this open, and ADR-327 closed it only for a restart whose journal had reached a flash plan,
through the hash its combination or guard record holds; before the plan a replacement went
undetected.

The maintainer confirmed the shape: the journal records what the job was created with and every
resume compares it with the job it is given, as it does for the artifact.

## Decision

1. **The record.** `Record::JobContent(JobContent)` is appended as the last variant of the
   journal's record type. `JobContent::Document(hash)` holds the SHA-256 of the job document's
   bytes (the hash the communication log already names the document by, ADR-347);
   `JobContent::Program(hash)` holds, for a job that is a program only, the SHA-256 of the
   program's postcard encoding. The kind is part of the value: a document and a program never
   match, even with equal bytes. A program that does not encode fails the job as a journal
   invariant before anything is created or compared, as a VM state that does not encode does.
2. **Written last in the creation prefix.** `Journal::create` takes the content as a required
   parameter and always writes the record, after the target VIN, the intended software version
   and the artifact hash when there are any, in the same write that makes the journal appear.
   It may appear only there and once, on commit and on read-back alike, as the artifact hash
   does (a record elsewhere is `Corrupt` on read-back). `FORMAT_VERSION` stays 1.
3. **Held on the state, not in the recovery facts.** `JournalState::job_content` carries it;
   `RecoveryFacts` does not, so the journal's summary and the checkpoint summary a handover
   sends hold no hash.
4. **The check.** `run_job` computes the content once, beside the artifact hash, and passes the
   same value to the creation and to the resume. A resume compares it right after the target
   VIN and the intended software version and before the classification: a journal with no such
   record, one of the other kind, or one with another hash ends in on-site intervention
   (`OnSiteReason::JobContentDiffers`). Nothing is sent, no resume is counted and no supply
   voltage is read. The check comes before the classification, so the artifact check, a
   classification that is on-site for another reason and the flash-plan rules never mask it;
   a later resume with the original job gets the original outcome back. A journal that holds
   only its creation prefix is refused the same way, although nothing was sent. A terminal
   record (a declared failure, an identity abort, a stopped run) is still reported first, an
   unreadable or busy journal is still reported as such, and the VIN and version mismatches keep
   their own reasons. The reports and logs name the reason only, never a hash.
5. **A torn record.** A creation that the file's atomic link makes whole cannot tear the
   record, but a frame zeroed while last reads back as a torn tail: the journal opens without
   the record, and every resume is then refused by item 4.

## Consequences

- A resume with no journal at all (the journal file is missing) is judged as a plain start from
  the given program, when that program passes the plain-start rule; the record cannot help,
  because there is no journal to hold it. A replaced program that the original job would have
  refused as a missing journal therefore runs. This is left as it was.
- A journal written before this decision has no record, so a restart of a job that was in
  flight on an upgrade is refused with on-site intervention. The same holds after an agent
  upgrade that changes the program's encoding for a program-only job. A journal with the record
  is unreadable to an older agent (an unknown record variant is `Corrupt`), so a downgrade with
  a job in flight ends in an unreadable journal. Both are on-site decisions, never a silent
  resume.
- A restart that is handed over to another device has nothing to compare: no code builds a
  journal from a summary, and the summary holds no hash (item 3). The handed-over restart's
  own gates stay those of ADR-229 and ADR-327.
- A document re-converted from the same source with other bytes is another document: the hash is
  over the bytes, not over the procedure and declarations.
- ADR-318's consequence that a replacement of the document or program goes undetected no longer
  holds, and ADR-327's remark that a replacement is not detected before the flash plan no longer
  holds either; the guard and combination records of ADR-325 and ADR-327 stay as they are.
- A program passed together with a document that is not its procedure is bound as the document;
  only a test builds such a job, since a document job derives its program from the document.
- A program-only job with another sources table or link configuration is not detected: those
  come from the caller, while a document job derives both from its document.
- A protected journal (an altered file with a valid checksum) is out of scope, as for every
  record (the header's protection is 0).

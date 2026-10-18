# ADR-293: A `RecoveryRequired` Section Keeps a Journal without a Flash Recovery Plan

**Date:** 2026-10-14
**Status:** Accepted (a job with several target combinations keeps a journal when any of them would, by ADR-325 item 7)
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`); ADR-252 item 7,
ADR-291 Consequences

## Context

ADR-291 writes an intent ahead of the first primitive of each pass through a section marked
`RecoveryRequired` outside the flash recovery plans, and refuses a resume while no transfer-start
marker follows that intent. That way a crash in the section, or after it, does not lead to a
plain start that would repeat the section (design 8.10.1).

The rule needs a journal. ADR-252 item 7 gives a journal only to a program with a flash recovery
plan, so a program with a `RecoveryRequired` section and no plan kept none. Its resume found no
journal and made a plain start, which ran the section again. ADR-291 (Consequences) left this
case open.

The item allowed two fixes: journal such a job, or refuse its resume. Refusing every resume would
also refuse one after a crash before the section was ever entered, when running the program again
repeats nothing that 8.10.1 protects.

## Decision

1. **A program keeps a journal when it has a flash recovery plan or a section marked
   `RecoveryRequired`** (`restart::keeps_journal`). Everything else about the journal is as for a
   program with a plan: it is created once the link is open and the policy allows the program,
   before anything is sent, and the resume opens it before the link.
2. **The classification treats such a program as it treats one with a plan.**
   - No journal at all is `OnSiteInterventionRequired(MissingJournal)`: the runner creates the
     journal before it sends anything, so a missing one cannot be told from one lost after the
     section was entered.
   - A section intent with no transfer-start marker after it refuses the resume (ADR-291
     item 2). A program without a plan never writes that marker, so once a run has entered the
     section, every later resume of the job is refused.
   - A journal without a section intent is a plain start. It runs the program again from its
     start on the same journal, which records the section's intent when the run reaches it.
3. **A program with neither keeps no journal**, as before (ADR-252 item 7).

## Consequences

- ADR-291's consequence on programs without a plan no longer holds. ADR-252 item 7 is amended:
  the condition is a plan or a `RecoveryRequired` section.
- A job of such a program that was interrupted anywhere after it entered the section ends in
  on-site intervention, nothing sent and no resume counted. A person decides whether the section
  may run again.
- Such a job writes the run start and the section's intents. The terminal records of ADR-280 and
  ADR-288 are written too, since they are written whenever a journal is kept.
- `run_program` (and so `ngr-agent run`) still keeps no journal; only `run_program_journaled` and
  `resume_program_journaled` do.

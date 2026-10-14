# ADR-290: The Restart's On-Site Endings after the Resume Count Stay Re-Resumable

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `agent` (`src/restart.rs`, `src/runner.rs` tests), `docs/system-architecture.md` 5.6;
ADR-229, ADR-255 item 2, ADR-261, ADR-286 Decision item 5

## Context

A restart counts its resume (ADR-229 item 2 step 1) before it reads anything from the ECU. Some of
its steps after that can end the job in `OnSiteInterventionRequired`:

- the gates' and step 3a's identity that cannot be established (`IdentityNotEstablished`);
- the default session not confirmed (`DefaultSessionNotConfirmed`);
- the software version not established, or not one the job knows (`SoftwareVersionNotEstablished`,
  `UnexpectedSoftwareVersion`);
- a precondition that does not hold at step 4a or 4b-3 (`PreconditionNotMet`,
  `PreconditionNotMetBeforeErase`);
- a continuation after a read-back verification that is not safe to repeat, or a missing plan-end
  state (`UnsafeContinuation`, `MissingPlanEndState`).

None of these is journaled. A later `resume_program_journaled` of the same job counts another
resume and runs the gates again, up to the plan's resume limit. Design 5.6 draws
`OnSiteInterventionRequired` as a final state, while design 8.2.5 bounds repeated resumes by the
resume limit. ADR-286 Decision item 5 journaled the identity abort and left open which of the two
rules governs these endings.

## Decision

1. **These endings end the run, not the job.** They stay unjournaled. A later resume of the job may
   try again: it counts another resume and runs the restart order from its start, gates included.
   The maintainer decided this.
2. **The resume limit bounds the repeats.** Each try counts a resume before it reads anything from
   the ECU, so once the count reaches the plan's `max_resumes`, the next resume ends in
   `ResumeLimitReached` with nothing sent (design 8.2.5, ADR-229).
3. **The other on-site endings are unchanged.** Those derived from the journal and the program, and
   those from the caller's input (ADR-286 Decision item 5), stay unjournaled and are decided again
   by every resume, before it counts a resume.

Why: the causes of these endings are mostly transient. A session that has not yet timed out, a
precondition such as the external supply or the engine state, or a version read that failed can
all hold on the next try. Making them final would ask for a person even where the next try needs
none. The resume limit already bounds the repeats, and each try runs the identity and safety gates
before it changes anything on the ECU.

## Consequences

- Design 5.6's state diagram gains an edge from `OnSiteInterventionRequired` back to
  `Interrupted` for these endings: a later resume of the job may try again within the resume
  limit. Every other `OnSiteInterventionRequired` stays final.
- Each such ending uses up one resume. A job whose cause does not go away ends in
  `ResumeLimitReached` after `max_resumes` tries.
- ADR-286 Decision item 5's open point is decided, and ADR-261's open consequence on the unjournaled
  endings is closed.
- A test shows a second resume after a `PreconditionNotMet` counting a resume, running the gates
  again and getting past step 4a, and a third ending at the resume limit with nothing sent.

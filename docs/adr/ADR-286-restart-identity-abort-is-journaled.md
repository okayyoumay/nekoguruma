# ADR-286: The Restart's Identity Abort Is Journaled as a Terminal Record

**Date:** 2026-10-13
**Status:** Accepted
**Affects:** `agent` (`src/journal.rs`, `src/restart.rs`, `src/runner.rs`, `docs/ngr-agent.md`),
`docs/system-architecture.md` 5.6 and 8.2.5; ADR-229, ADR-244, ADR-261, ADR-280

## Context

Design 5.6 ends a restart whose re-verification finds another vehicle's ECU in Failed: the VIN read
in the gates (ADR-261 item 3), or the VIN or hardware part number read in step 3a (ADR-229 item 2),
decodes to an identity other than the job's, and the job ends in `JobError::IdentityMismatch`. The journal kept no
record of that ending. A later `resume_program_journaled` of the same job therefore counted another
resume and ran the gates again, and once the VCI was on the job's own vehicle the job went on,
although the design makes it final. ADR-261 recorded this as an open consequence.

ADR-280 journals the other `Failed` edge of a write job, the procedure's own `diag.fail`, as
`Record::ProcedureFailed`, and every resume reads it before it does anything. The restart's abort is
the same kind of ending: final, decided by the agent, possibly with an erased ECU.

The on-site endings (`OnSiteInterventionRequired`) were left unjournaled as well. They are not all
alike: some are derived from the journal and the program alone, some from the caller's input, and
some from live reads after the resume count.

## Decision

1. **A new record, `Record::IdentityMismatch { identity }`,** appended at the end of the record enum,
   with no format-version change (the ADR-272 and ADR-280 precedent). `identity` is the
   `IdentityKind` that differed (VIN or hardware part number); the journal refuses a software
version, in a commit and when it reads the file back. It carries no step, since the abort
   happens at no VM instruction, and no VIN value.
2. **It folds into `RecoveryFacts::identity_mismatch` and is terminal.** No record may follow it or a
   procedure failure, so neither can follow the other. A journal with a record after it reads back
   as corrupt, as for ADR-280.
3. **The restart commits it where the abort is decided.** When the gates (step 2) or the identity
   check after the default session (step 3a) return `IdentityMismatch`, the runner commits the
   record under the writer lock and then ends the job with the same error; the link closes
   afterwards as it does for `ProcedureFailed`. If the commit fails, the job ends in the journal's
   error instead, so a caller never sees a terminal verdict the journal does not hold.
4. **Every resume reads it first.** It is checked next to the recorded procedure failure: before the
   link is opened, before the target VIN and intended version comparisons (ADR-261, ADR-268), and in
   `classify` before the transfer is looked at (`RestartDecision::Aborted`). The job ends in the
   same `IdentityMismatch` with nothing sent to the ECU and no resume counted. A journal another run
   holds (`InUse`, ADR-255 item 7) and a link not confirmed closed (ADR-258) are still reported
   first, as for ADR-280.
5. **On-site endings stay unjournaled.** Those derived from the journal and the program
   (recovery-required points and sections, unknown stage, missing or invalid entry state, a plan
   that allows no restart) are recomputed identically by every resume, and pinning one could keep a
   later agent version from a resume it may allow. Those from the caller's input (target VIN,
   intended version) must not let a wrong caller make a valid job final. Those reached after the
   resume count from live reads (identity or software version not established, unexpected version,
   preconditions, default session not confirmed, unsafe continuation, missing plan-end state) are
   final in design 5.6, while design 8.2.5 bounds repeated resumes by the resume limit; which of the
   two governs them is left to the maintainer.

## Consequences

- A restart that found another vehicle's ECU ends the job for good: no later resume counts a resume,
  opens the link or sends anything, whichever vehicle the VCI is on by then.
- A crash between the gates' verdict and the commit leaves no record. Nothing is sent in that window,
  and the next resume runs the gates again, as before this decision.
- The record holds `diag_ir::IdentityKind`, which the journal encodes by variant index, so that
  enum's variant order becomes part of the journal format; a test pins it.
- `RecoveryFacts` gains a field, so a recovery summary encoded by an earlier build does not decode;
  such summaries are not kept across builds yet (as noted for ADR-280).
- ADR-261's open consequence is closed for the identity abort and stays open for the on-site endings
  reached after the resume count, which wait on the maintainer's decision.

## Alternatives considered

- **One generic `Record::Aborted { cause }`.** Rejected: there is one abort cause today, and a second
  one gets its own record, as `ProcedureFailed` did.
- **One `RecoveryFacts::terminal` field holding either ending.** Rejected: it changes every reader of the
  recorded procedure failure for no change in behaviour.
- **Journal every on-site ending now.** Rejected: the input-derived ones would let a wrong caller end
  a valid job, the derived ones gain nothing, and for the live-read ones the design's two rules
  disagree, which is the maintainer's call.

# ADR-475: A Run's End Is Mapped to the Job's Reported State by Its Error and Its Journal's Standing

**Date:** 2026-11-14
**Status:** Accepted
**Affects:** design 5.6, `api/asyncapi.yaml`, `crates/agent` (`src/job_report.rs`, `docs/ngr-agent.md`), `crates/shared-proto`; annotates ADR-280, ADR-288; relies on ADR-255, ADR-325 item 12, ADR-335, ADR-344, ADR-348 items 6 and 8, ADR-375, ADR-392

## Context

A run ends in `Result<VmState, JobError>`. The shared protocol's `JobState` and `FailureCode`
(design 5.6, 5.2) were not tied to it: nothing said which error ends a job as failed, which
leaves it open for a resume, or which is a precondition failure, and a procedure's own failure
code had no field of its own, so it could be mistaken for one of the agent's. ADR-280 and
ADR-288 left the reported state of a procedure failure and of a stopped run open.

## Decision

1. **The standing is what the next resume would decide.** After the run the journal is read
   with `Journal::read`, which takes no lock, and judged by `restart::classify`, not by rules of
   its own. A plain start is clean. A decision the journal holds (a procedure failure, an
   identity abort, a run's stop: ADR-280, ADR-286, ADR-288) is decided. An on-site decision is
   on-site, with its reason. Anything else is resumable. Two cases are clean because nothing can
   have been sent: no journal on a first run (the run creates it before it sends anything), and
   a job ID that cannot name a journal on any run. A missing journal on a resume is
   `classify`'s: a plain start only for a program with no plan, no recovery-required section
   and no primitive unsafe to repeat (ADR-293, ADR-335 item 1), on-site otherwise. A journal
   that cannot be read back is on-site.
   - **On-site standing:** the job ends in `needsOnSiteRecovery` at once, whatever the error (a
     cancel and a transient error included), the detail the reason's name and plain numeric
     fields.
   - **Resumable standing:** a refusal that every resume would meet again (`Program`, `Vm`,
     `SeveralTargets`, `Undeclared`, `NoDeclarations`, `Sources`, `Refused`, `NoArtifact`,
     `ArtifactMismatch`, `SegmentOutsideArtifact`, `ArtifactDiffersFromJournal`,
     `CommLogMismatch`, `NoCommLogDir`, `CurrentThreadRuntime`, `GuardsMissing`) ends the job in
     `needsOnSiteRecovery`, since leaving it open would repeat it until the resume limit. Any
     other error (`Link`, `Host`, `IdentityRead`, `IdentityUnreadable`, `NoReprogrammingSlot`,
     `VehicleLock`, `LinkUnconfirmed`, `Journal`, `Panicked`, `Cancelled`, `Unanswerable`)
     leaves the outcome open: no terminal state is reported and the job is resumed as after a
     crash; the resume limits end it in `needsOnSiteRecovery` (ADR-335 item 6).
   - **Decided standing:** open; the resume reports what the journal holds.
2. **A first run's refusal before anything is sent is a precondition failure.** The static
   checks (`Program`, `SeveralTargets`, `Undeclared`, `NoDeclarations`, `Sources`, `Refused`,
   `NoArtifact`, `ArtifactMismatch`, `SegmentOutsideArtifact`) give `preconditionFailed` with
   `jobRefused`; a link that does not open gives `linkNotOpened`; a link left unconfirmed gives
   `deviceLocked`; an agent-side fault (`NoReprogrammingSlot`, `CurrentThreadRuntime`,
   `CommLogMismatch`, `NoCommLogDir`, `GuardsMissing`, `Panicked`, a journal fault) gives
   `internal`. On a resume the same causes are `failed` with the same codes. A panic and a
   journal fault can happen mid-run, so they are `failed` with `internal` on a first run too.
   These rules apply to a clean standing. `preconditionFailed`
   gains a `detail`. A variant sorts by its own type only: the mapping matches no worker kind,
   API or vendor code (design principle 6).
3. **Terminal whatever the standing.** The runner journals these, so a resume would end the same
   way: the procedure's own failure (`failed`, code `procedure`), an identity mismatch
   (`vinMismatch` for the VIN, `variantMismatch` for the hardware part number; any other
   identity, which no run ends on today, `other`), a step limit (`failed`, `procedureError`)
   and an on-site ending (`needsOnSiteRecovery`). A VM error is the same while the standing is
   clean or decided; on a resumable journal it is a pre-run check's refusal (item 1).
4. **The procedure's code travels apart.** `failed` and `jobState` gain an optional
   `procedureCode` (1 to 128 bytes of UTF-8; the byte count is the rule, which a schema's
   character-counting `maxLength` only approximates), set with `procedure` only; `detail` is the
   procedure's detail cut to 1024 bytes. A procedure can never produce an agent code.
5. **Interruptions with a clean standing.** A failed exchange (`Host`, `IdentityRead`,
   `IdentityUnreadable`, `Link` on a resume, `VehicleLock`) is `failed` with `exchangeFailed`,
   which asserts neither a fault of the vehicle nor of the diagnostic system: only `internal`
   asserts the latter, and design 8.11 and ADR-375 leave the attribution of an exchange to the
   communication log. `ArtifactDiffersFromJournal` is `jobRefused` and `Unanswerable` is `other`.
6. **Another run's job reports nothing.** `Journal(InUse)` and `Journal(AlreadyExists)` mean
   another run owns the job (ADR-255), so the outcome is none.
7. **A detail is `LogSafe::log_text` cut to 1024 bytes**, never `Display` or `log_external`
   (ADR-348 items 6 and 8, design 16.2); `log_text` carries no operating-system text (the I/O
   error of a journal or a guard's lock file shows its kind only), no document text and no job
   ID, and an empty detail is left out of the event. An on-site reason is written by hand as its name and
   plain numeric fields, never `Debug`: the strings it holds and the voltage the VCI read stay
   out, as do the nested teardown outcomes.
8. **The agent module and the wire.** `agent::job_report::outcome` takes the result, whether
   the run was the job's first and the journal's standing (`JournalStanding::read`, which
   takes the same flag) and returns terminal, open or none.
   `shared-proto` gains the codes `procedure`, `procedureError`, `jobRefused`, `linkNotOpened`,
   `exchangeFailed` and `internal`, and the typed state converts to the `jobState` event. No
   database column is added: the server does not parse or store `jobState` yet.

## Consequences

- A runtime that shuts down before a job's thread starts ends the job as cancelled, which the
  result alone cannot tell from a user's cancel: the reporter must send nothing terminal
  while the agent shuts down.
- The intake must resume open jobs and report `running` only once the first step begins; it
  must record what it reports so that no later resume reports again; the server must store the
  state, code, procedure code and detail as vehicle data, outside the audit log. The
  communication log's incomplete flag has to travel with the result. These are later work.
- `Host` on a first run with a clean standing is a `failed` with `exchangeFailed`, since a
  primitive failed mid-run: the first-run rule covers only what precedes the first request.

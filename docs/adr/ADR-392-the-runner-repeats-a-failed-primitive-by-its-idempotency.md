# ADR-392: The Runner Repeats a Failed Primitive by Its Idempotency

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `agent` runner (`runner.rs` `run_vm`, `JobLimits::max_primitive_repeats`),
restart (`restart.rs` repeat decision, plain-procedure classification), journal (`journal.rs`
`Record::RequestNotSent`), host and communication log (`origin: repeat`); design 5.6, 8.2.5,
8.11; `crates/agent/docs/ngr-agent.md`; ADR-233 item 2; ADR-335 item 3; ADR-389

## Context

Design 8.2.5 and ADR-233 item 2 give the runner the repeat policy of a diagnostic primitive by
its section's idempotency, but the runner ended the job on every host error. ADR-335 journals a
request marker before a primitive not safe to repeat and confirms it with the checkpoint after,
and decides only what a restart with lost VM state does. ADR-389 lets the host say whether a
failed request may have reached the ECU. The IR has no state query, so a `CheckState`
primitive has nothing to check its state with.

## Decision

1. **One decision, in one place.** When a diagnostic primitive fails in the host, one function
   decides whether to send it again, to end the run as before, or to end it in on-site
   intervention. Nothing else decides it.
2. **A lost answer of a primitive safe to repeat is sent again.** Outside every flash plan, a
   failure that may have reached the ECU and left the link open (no response, a primitive that
   ended with an error event, lost events) is sent again in place when the primitive is safe to
   repeat (ADR-335's definition), at most `JobLimits::max_primitive_repeats` times per arrival
   (default 2), with the poll interval of a wait between sends, during which a cancel is taken
   as during a wait. This holds inside an uninterruptible or recovery-required section too, whose
   held cancel stays held. When the bound is reached, the run ends as before.
3. **One not safe to repeat ends as a resume would.** The same failure of an `Unsafe`
   primitive, of a `CheckState` one, or of one no section covers ends the run with the verdict
   the restart's classification gives on the journal as it now stands: in on-site intervention
   with that classification's reason when it refuses a resume, and as before (the run ends and
   a resume follows that classification) when it does not, for example after a completed flash
   transfer, whose restart order is ADR-229's. Without a journal, which no resume reads, the
   run ends in on-site intervention for the unconfirmed request. `CheckState` counts as
   `Unsafe` until an ADR adds a state query.
4. **A request not sent is sent again whatever its idempotency, when the cause passes.** A
   request refused before it was sent because an earlier primitive held the link or the queue
   was full is sent again within the same bound. No second request marker is journaled.
5. **A marker whose request was not sent is withdrawn.** A run that ends with a journaled
   request marker whose request was provably not sent (a refusal before the start, or a cancel
   taken after the marker and before the send) journals `RequestNotSent` first. A restart treats
   a withdrawn marker as no marker. A failure that may have reached the ECU never withdraws one.
6. **Everything else ends the run as before.** A failure of the worker or the link, any failure
   inside a flash plan (whose restart order is ADR-229's), and a failure of a step that sends
   nothing to the ECU end the run as they did. Continuing within the run after reopening the
   link is separate work.
7. **A resend is logged as a repeat.** Every attempt after the first of one arrival at the
   primitive has `origin: repeat` in the communication log, also when the first was refused
   before it was sent.
8. **An older agent refuses the new record.** `RequestNotSent` is appended to the journal's
   record set as ADR-244 allows; an agent that does not know it reads the journal as
   unreadable and ends in on-site intervention, as for any record it cannot read (ADR-344).

The maintainer chose `CheckState` as `Unsafe` for now, two repeats with the poll interval, and
the rest of this decision (2026-10-30).

## Consequences

- A read that loses its answer once no longer ends the job, and an `Unsafe` request whose answer
  is lost ends on site at once instead of in a resume that would reach the same verdict.
- Repeats do not advance the VM's step count, so the repeat bound, not the step limit, bounds
  them.
- A run that ends with an unsent request journals one record more, so a later resume does not
  end in on-site intervention for a request that never went out.
- A `CheckState` routine outside a flash plan ends on site when its answer is lost, until a
  state query exists (backlog).
- A request marker withdrawn at the first primitive of a recovery-required section leaves the
  section's intent behind, so a resume still ends in on-site intervention for the section,
  although nothing in it was sent. That errs toward the safe side.
- A worker crash or a lost VCI still ends the run, and the VM state is resumed only through the
  journal (backlog).

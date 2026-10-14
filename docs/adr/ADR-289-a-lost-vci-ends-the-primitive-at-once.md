# ADR-289: A Lost VCI Ends the Pending Primitive at Once, as `HostError::VciLost`

**Date:** 2026-10-13
**Status:** Accepted (the consequence on a VCI lost between two primitives closed by ADR-294: the next request ends in `VciLost` too)
**Affects:** `agent` (`src/host.rs`), `docs/system-architecture.md` 8.2.4; ADR-235 item 3, design 5.6 and 8.2.5

## Context

When the worker meets a lost device, it reports the loss for the whole link and then cancels the
link's primitives. The event sequence is `PDU_ERR_EVT_LOST_COMM_TO_VCI`, then `PDU_COPST_CANCELLED`
for each primitive, then the link offline and the module not available (`j2534-0404-service`,
`handle_channel_hard_error`). The lost-communication event carries no primitive handle, since it
belongs to no single primitive.

The agent's host took only events bound to its own primitive (ADR-235 item 3). It skipped the
handle-less event, so the cancellation that followed ended the send-receive as
`HostError::NoResponse`: a lost VCI looked like an ECU that stayed silent. The resume model needs
the two told apart (design 5.6 and 8.2.5): a silent ECU and a lost VCI are different
interruptions, and a person reading the job's error needs to know which one happened.

## Decision

1. **`PDU_ERR_EVT_LOST_COMM_TO_VCI` with no primitive handle is a VCI-level event that ends the
   pending primitive.** The host returns `HostError::VciLost` as soon as the event arrives. It
   does not wait for the cancellation that follows, since nothing more can reach the ECU through
   the link.
2. **The same event bound to the pending primitive counts the same.** One bound to another
   primitive (an earlier one) is not this request's and is skipped, as every other event of
   another primitive is.
3. **The outcome is unknown, not "no response".** The request may or may not have reached the ECU
   before the device was lost. `VciLost` therefore says nothing about the ECU's answer, unlike
   `NoResponse`.
4. **The job's handling does not change.** A host error ends the run as an interrupted step
   (`JobError::Host`), so the journal and the restart order treat a lost VCI as they treated it
   before. A lost VCI is one of design 5.6's interruptions, and the restart goes on once the VCI
   is back. What changes is the error the job reports.

## Consequences

- ADR-235 item 3's note that a lost VCI is not told apart no longer holds.
- A lost VCI no longer waits for the worker's cancellation, so the job ends at once on the event.
- When the device is lost between two primitives, the host still sees no event for a pending
  primitive. The next request then fails as a call on a link that is offline (`HostError::Rpc`
  with `FailedPrecondition`), as before. Mapping that case to `VciLost` too needs the worker to
  say why the link is offline, which this decision does not change.

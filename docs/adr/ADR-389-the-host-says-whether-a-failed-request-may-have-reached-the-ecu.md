# ADR-389: The Host Says Whether a Failed Request May Have Reached the ECU

**Date:** 2026-10-30
**Status:** Accepted
**Affects:** `agent` host (`host.rs` `HostError::send_state`, `SendState`, `HostError::VciLost`,
`sent_end`, `Sent`), communication log (design 8.11 `sent`); `docs/rpc-api-guide.md` "Reading a
Worker's Answer"; ADR-289; ADR-294 item 1; ADR-350 item 2

## Context

Before the job runner repeats a diagnostic primitive (design 8.2.5, ADR-233 items 1 and 2), it
must know whether a failed send-receive's request may have reached the ECU: a request that was
certainly not sent can be sent again whatever its idempotency, and one that may have arrived
cannot.

The host's errors said this for some failures only. A `StartComPrimitive` that timed out was
`HostError::WorkerUnresponsive`, which names the worker's silence but not the request's fate.
`HostError::VciLost` covered two cases with opposite answers: a loss reported while the
primitive was pending (ADR-289), when the request may have arrived, and a start the worker
refused on a link it had taken offline (ADR-294), when nothing was sent. Only the communication
log told them apart, from a flag of its own about whether the primitive had started, and the log
is written only when it is open. The runner sees nothing but the error.

## Decision

1. **One judgement on the error.** `HostError::send_state()` says `NotSent` or
   `MayHaveReached`. It says `NotSent` only when a refusal before the start proves that nothing
   was sent:
   - the agent's own refusals before the start (its request policy, the request's preparation,
     a flash transfer it cannot build, an input not established);
   - an earlier primitive still holding the link (`LinkHeld`);
   - a start the worker refused naming a reason (ADR-350 item 2);
   - a start refused after the VCI was lost (ADR-294).

   Everything else may have reached the ECU, a timed-out start included: the worker answered
   neither a refusal nor a success. So do a failed event subscription after the start, a lost
   VCI while the primitive was pending, a primitive that ended with an error event, lost events,
   and no response.
2. **`VciLost` carries its case.** `HostError::VciLost { refused_after_loss }` says which of the
   two cases it is, as the log's `vci_lost` end already did. This annotates ADR-294 item 1: the
   variant stays, with the field.
3. **The log derives its not-sent cases from the same judgement.** The communication log's
   end of an exchange that failed before the ECU answered is `not_sent` exactly when
   `send_state()` is `NotSent`, and its own record of whether the primitive started is gone. The
   name of the failed call already shows where a failure arose: only the start call fails as
   `StartComPrimitive`.

## Consequences

- The runner can repeat a primitive whose request was not sent, whatever its idempotency, and
  must treat every other failed request as possibly received.
- The judgement errs one way: a failure it cannot prove before the start reads as possibly
  received. A transmit error that in fact left nothing on the bus is one of them.
- A new error variant must be judged where it is added: the judgement matches every variant by
  name.
- A timed-out start can leave a primitive running in the worker with no handle the host knows,
  so the next request does not cancel it, and its late answer is read as an item of no
  primitive. That is unchanged here (backlog).

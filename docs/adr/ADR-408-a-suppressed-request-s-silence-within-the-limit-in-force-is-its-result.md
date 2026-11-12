# ADR-408: A Suppressed Request's Silence Within the Limit in Force Is Its Result

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `diag-ir` (`suppresses_positive_response`, `dry_run`, `DiagHost` documentation),
`agent` (`src/host.rs`: `exchange`, `Sent::vci_ended`, `Sent::silence_is_result`, `sent_end`),
`sim-ecu` (`Fault::ResponsePending`), `crates/diag-frontend/docs/starlark-subset.md` 3.4 and 6,
`crates/agent/docs/ngr-agent.md`, `crates/sim-ecu/docs/simulated-ecu.md`, design 8.2.4 and 8.11;
supersedes a consequence of ADR-235 item 3, extends ADR-328 item 2, applies ADR-397

## Context

ADR-235 item 3 maps every primitive that ends without a final response to
`HostError::NoResponse`, and its Consequences record that a request whose positive response is
suppressed therefore fails although it succeeded. ISO 14229-1:2026 7.7.3.2 says that with the
suppress bit set the server sends no positive response, still sends every negative response, and
owes a final response after a response pending (0x78). The client therefore cannot tell
"accepted" from "never arrived" except by waiting for the link's limit and seeing no negative
response.

The workers offer two ways to "expect no response": zero receive cycles (ISO 22900-2:2022
8.2.6.2.2) and the `SUPPRESS_POS_RESP` transmit flag (D.2.1, Annex I.2.1). Both lose or change
the negative response, the flag has no J2534 equivalent, and design 8.11 already leaves it unset
(design principle 6: the workers absorb their APIs' differences, and these two would differ). The
log already names a suppressed request (`request.suppressed`) and classifies its silence.

## Decision

1. **The host judges the silence, on the evidence it has.** One function in `diag-ir`,
   `suppresses_positive_response`, says whether a request asks for its positive response to be
   suppressed (bit 7 of the sub-function of a UDS service that has one). The host's result, the
   log's `request.suppressed` and the dry run all call it (ADR-397).
2. **A suppressed request succeeds only when the VCI ended the primitive by its receive timeout
   with no pending seen and no final response.** The result is `Ok` with empty bytes
   (`Sent::silence_is_result`, using `Sent::vci_ended`, which the entry's `vci_ended` shares).
   The wait is the link's limit in force as the VCI or worker runs it; the agent's own budget
   (ADR-235 item 6) is unchanged, and its passing stays `NoResponse`, as does a short
   positive-form frame the worker took as the end, an error event, lost events and a lost VCI.
2a. **Two further conditions.** No error event may be bound to the primitive: an error read, then
   the receive timeout, then the agent's deadline passing is not silence. A request longer than
   one frame (more than `CAN_SINGLE_FRAME_BYTES`, the 6 bytes a classic CAN frame surely carries
   whatever the addressing; the host does not know the format) needs a confirmed send as well, a
   transmit indication bound to the primitive or a transmit-done time in `Sent`. An expired
   flow control is reported as a receive timeout too (ISO 22900-2:2022 D.4.2; the J2534 worker
   does the same), so without that confirmation nothing was delivered and the result stays
   `NoResponse`.
3. **A negative response is returned as for any request. A pending seen means a final response
   is owed**, so silence after it stays `NoResponse`. On a link whose VCI absorbs pendings the
   host cannot see one, so silence after an absorbed pending counts as success; the log's reading
   H4 records it (design 8.11).
4. **Nothing changes in the workers or the interface**: `num_receive_cycles` stays 1 and the flag
   stays unset, so both workers deliver the negative response.
5. **The procedure sees empty bytes**: `ok` true, `nrc` 0, `raw` empty, and a field read stops
   the run. The dry run answers a suppressed request with no bytes unless the script gives an
   answer, so the conversion refuses a procedure that reads fields the ECU never sends (extends
   ADR-328 item 2).
6. **The entry keeps `outcome: no_response` with `request.suppressed`**, judged by design 8.11's
   rows; `detail.host_error` is absent, since the primitive did not fail.
7. **`sim-ecu`'s `Fault::ResponsePending` applies to a suppressed request**: after the pending
   messages the ECU sends the final response whatever the bit says (7.7.3.2), so the pending
   cases run end to end.

## Consequences

- ADR-235's consequence is superseded. ADR-392 no longer repeats a suppressed request that stayed
  silent, since its step is `Ok`.
- A procedure author who needs confirmation must not set the bit.
- On an absorbing link a pending followed by silence passes as success (documented limitation).
- TesterPresent stays refused to procedures (ADR-396), and no functionally addressed procedure
  target exists, so `3E 80` and the functional suppression of 7.7.3.3 are not among the requests
  this decision reaches; the functional case is to be decided with functional targets.
- `presence` keeps its own rule for a silent window (ADR-396): a collecting window that saw only
  a pending is a legitimate silent outcome there, while for one suppressed request it is not.

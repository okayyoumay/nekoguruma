# ADR-350: Worker Error Events Carry Their Extra Information, and Refusals Name Their Reason

**Date:** 2026-10-24
**Status:** Accepted (Decision item 1's agent use of a known code superseded by ADR-351)
**Affects:** `vci-service-interface` (`service.proto`: `EventItem`, `ErrorData`, `ErrorDetail`, `RefusalReason`;
`start_refusal`, `protocol_cannot_carry`), `iso22900-service` (event items, `StartComPrimitive`),
`j2534-0404-service` (`StartComPrimitive`, `resolve_send_recv_tx`), `iso22900-mock`,
`docs/rpc-api-guide.md`; design 8.11, ADR-105, ADR-346

## Context

The communication log (design 8.11, ADR-346) needs two things from the worker interface that it
did not carry:

- An error event's extra error information. ISO 22900-2:2022 D.4.2 gives each error item an
  additional error code (`PDU_ERROR_DATA.ExtraErrorInfoId`) whose meaning the library's MDF gives:
  a frame error that names the response's frame, a receive error that names a missing
  acknowledge, a protocol error that names a request too long for the protocol. Design 8.11
  "Link error" classifies those cases more precisely than `link_error`. `EventItem.error_data`
  was the bare `PDUErrorEvent`.
- Why a worker refused a request. Design 8.11 counts a request as `not_sent` only on a definite
  refusal, and names the worker's own reasons: invalid parameters, a full queue, a request the
  protocol cannot carry. A refused `StartComPrimitive` came back as a plain `INVALID_ARGUMENT`
  for the worker's own checks, whatever the reason, and with the library's `PDUError` in
  `ErrorDetail` (ADR-105) for a native refusal. The agent could not tell a refusal that shows the
  request was not sent from a failed start, nor a too-long request from invalid parameters.

## Decision

1. **Extra error information.** `EventItem.error_data` becomes a message, `ErrorData`, with
   `error_event` (the `PDUErrorEvent`) and `optional uint32 extra_error_info`: the error item's
   additional error code as the library gives it. The extra information belongs to the error, so
   it is part of the error's own value rather than a field of `EventItem` that only one variant
   uses. The variant moves to tag 10 and its old tag 7, the bare `PDUErrorEvent`, is reserved:
   this breaks the wire format for error events, which is accepted because no release has
   shipped. `PDUErrorEvent` itself stays an enum, since `ErrorDetail` and other messages use it.
   The field is `optional`, so a worker with no such value leaves it absent and a library's 0
   stays 0. The D-PDU worker copies `ExtraErrorInfoId`; the J2534 worker leaves it absent, since
   SAE J2534-1 has no such value. The codes are vendor-defined, so the interface passes them on
   without interpreting them; the agent uses one only when it knows the library's code.
2. **Refusal reason.** `ErrorDetail` gains `optional RefusalReason refusal`, with the reasons
   `INVALID_PARAMETERS`, `QUEUE_FULL` and `PROTOCOL_CANNOT_CARRY`. A worker sets it on a refused
   `StartComPrimitive` only, and only when the refusal shows the request was not sent. Its
   absence says nothing about whether it was sent, so the agent then keeps the send `unknown`.
3. **Which refusals name a reason.** Both workers pass every error of `StartComPrimitive` through
   one helper, `vci_service_interface::start_refusal`:
   - an `INVALID_ARGUMENT` the worker returned from its own checks, which had no `ErrorDetail`,
     now carries one with `PDU_ERR_INVALID_PARAMETERS` and the reason `INVALID_PARAMETERS`;
   - a library's `PDU_ERR_INVALID_PARAMETERS` is `INVALID_PARAMETERS`, and its
     `PDU_ERR_TX_QUEUE_FULL` is `QUEUE_FULL` (the J2534 worker maps `ERR_BUFFER_FULL` to the
     latter);
   - a refusal that already names its reason keeps it, and every other error is returned as it
     was.

   The J2534 worker's own transmit-size checks (the SAE J2534-1 message size range of the link's
   protocol, and the single-frame limit of a functionally addressed ISO 15765-2 request) refuse
   with `PROTOCOL_CANNOT_CARRY` (`vci_service_interface::protocol_cannot_carry`), with
   `PDU_ERR_INVALID_PARAMETERS` as the `PDUError`, which is how the worker reported them before.
4. **What the helper does not do.** It reads no other RPC, and it does not judge a library's
   other codes: a code that does not show the request was not sent leaves the reason absent.

## Consequences

- Every reader of `EventItem.error_data` reads `ErrorData.error_event` instead of the bare enum,
  and a peer built before this change does not read the new error events.

- A refused `StartComPrimitive` now always carries an `ErrorDetail` when its status is
  `INVALID_ARGUMENT`; ADR-105's plain status for request-shape failures stays for every other
  RPC.
- The D-PDU worker has no transmit-size check of its own, so a too-long request on a D-PDU link
  is refused by the library with whatever code it uses, or reported later as a protocol error
  whose extra error information names it, which item 1 now passes on.
- The J2534 worker's transmit queue is unbounded, so it never refuses a start for a full queue
  itself; `QUEUE_FULL` comes only from a library.
- The agent's host maps the extra error information and the refusal reason to the log's
  outcomes in a later change (design 8.11 "Link error", `not_sent`).

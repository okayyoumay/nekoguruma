# ADR-427: The D-PDU Mock Can Queue Its Indications Unbound

**Date:** 2026-11-07
**Status:** Accepted (Consequences on the agent's D-PDU link settled by ADR-428)
**Affects:** `crates/iso22900-mock` (`SimEcuCommand::IndicationBinding`, `ecu_events`,
`send_tester_present`, `make_event_item`, `PDUDestroyItem`), `crates/iso22900-mock/docs/testing-guide.md`,
`crates/iso22900-service/tests/t2_indication_unbound.rs`, `crates/ngr-hw-checks` guarded handle
(`note_indication` unit tests); ADR-299, ADR-356, ADR-423

## Context

ADR-356 has the guarded handle take a link's transmit done and the response's start from the
indication items (`CP_TransmitIndEnable`, `CP_StartMsgIndEnable`, ISO 22900-2:2022 B.5.1). It
accepts an indication with no ComPrimitive (`hCop` undefined) when the CAN identifier in its
extra info header is the link's. That shape is the J2534 worker's (ADR-051, ADR-151). A D-PDU
library attaches extra info (10.1.4.11.4, D.2.1) only to the items of a ComPrimitive whose
TxFlag sets ENABLE_EXTRA_INFO, and an item with `hCop` undefined has no ComPrimitive, so a
conforming D-PDU library gives it no header and `note_indication` ignores it. The mock queued
its indications bound to the ComPrimitive only, so the handle's unbound path was covered by its
unit tests and never end to end through `iso22900-service`.

The backlog asked whether the handle needs a rule for the API's own tester present (ADR-423),
whose unbound transmit indication on the request identifier `note_indication` would take as the
exchange's transmit done if it carried a header. B.5.1 ties the transmit indication to a
transmit the protocol completed and does not say whether an API-generated message counts.

The API sends its tester present only on a COMM_STARTED link (8.2.6.3.2 e), B.5.1). The handling
is on by B.5.1's default for the UDS protocols and the guard leaves it as is, but no check sends
the `CoptStartcomm` the guard grants, so no guarded link is COMM_STARTED. The tester present goes
at once when the link starts and then every `CP_TesterPresentTime`, and 8.1.3 Table 1 holds a
physical transmit until the previous physical request's final answer (a response pending
excepted), so a conforming library lands it before a request or after its response, never
between. One sent before the request is overridden by the handle's "latest unbound indication
before the response" rule. With address mode 1 it goes out on the functional identifier, which
the handle's identifier check does not match.

## Decision

1. **The mock has a binding control, as a test double.** `SimEcuCommand::IndicationBinding {
   bound, request_id, response_id }` (JSON `{"indication_binding":{"bound":false,
   "request_id":2016,"response_id":2024}}`) makes the indication items unbound in the J2534
   worker's shape: `hCop` is `PDU_HANDLE_UNDEF`, the tag is 0, and `pExtraInfo` has four header
   bytes, the CAN identifier big-endian (`request_id` for a transmit indication, `response_id`
   for a start-of-message one). Both identifiers are required with `bound` false. The header
   comes from the control, not from a library. The default is bound, as before.
   `__mock_reset` resets the binding; `Detach` does not (like `response_timestamps`). A
   ComPrimitive takes the binding when it is started, like the indication ComParams. The mock
   knows no CAN identifiers (ADR-299), so the control supplies them. `PDUDestroyItem` frees the
   extra info and its header bytes with the result item.
2. **The mock's tester present raises an unbound transmit indication under that control.** When
   the ECU's own tester present (ADR-423 item 5) is sent, the link's active `CP_TransmitIndEnable`
   is on, the active tester-present address mode is 0 and the binding is unbound, the mock
   queues an unbound transmit indication with `request_id`, at the send's time. Otherwise it
   queues none: a bound mode has no identifier to give it, and a functional send goes out on an
   identifier the control does not supply.
3. **The handle keeps its rule.** `note_indication` takes the latest unbound indication on the
   request identifier before the response. "The first after the send" would be wrong for a
   tester present followed by the request, an order 8.1.3 Table 1 allows. The handle's unit
   tests show the request identifier (taken), the functional one (not taken) and an unbound
   indication with no header (ignored) next to a bound indication, and the integration test
   fails under a "first after send" rule.
4. **Related ADRs are annotated.** ADR-356's Consequences on the mock and on the tester present,
   and ADR-423 item 5, point here.

## Consequences

- `iso22900-service`'s `t2_indication_unbound` test shows the `hCop`-undefined and `pExtraInfo`
  path through the D-PDU worker and the handle taking its times from it, with the mock's
  response items carrying no times, and a later exchange keeping its own, newest transmit done
  after the start of communication's and the mock's tester present's indications.
- On D-PDU the residual is moot: a tester-present indication from a conforming library has no
  header and is never matched. A mid-exchange tester present that shortens a measured latency
  exists only for the J2534 shape, where the worker's own tester present gets a TxDone with a
  header (ADR-419 serializes it per Table 1). As in ADR-356 it can shorten a latency but never
  makes an on-time response late.
- The agent's end-to-end coverage of the unbound path is still open. Design 8.11 `sent_evidence`
  takes an unbound transmit done only when the API sends no tester present on that identifier,
  and the agent's tests through the mock use the default, bound mode.
- The agent's D-PDU link sends no TxFlag, so on a real library its response items carry no
  header; settled by ADR-428: the D-PDU worker sets ENABLE_EXTRA_INFO on every ComPrimitive.

# ADR-427: The D-PDU Mock Can Queue Its Indications Unbound

**Date:** 2026-11-07
**Status:** Accepted
**Affects:** `crates/iso22900-mock` (`SimEcuCommand::IndicationBinding`, `ecu_events`,
`send_tester_present`, `make_event_item`, `PDUDestroyItem`), `crates/iso22900-mock/docs/testing-guide.md`,
`crates/iso22900-service/tests/t2_indication_unbound.rs`, `crates/ngr-hw-checks` guarded handle
(`note_indication` unit tests); ADR-299, ADR-356, ADR-423

## Context

ADR-356 has the guarded handle take a link's transmit done and the response's start from the
indication items (`CP_TransmitIndEnable`, `CP_StartMsgIndEnable`, ISO 22900-2:2022 B.5.1). It
accepts an indication with no ComPrimitive (`hCop` undefined) when the CAN identifier in its
extra info header is the link's. The mock queued its indications bound to the ComPrimitive only,
so that path was covered by the handle's unit tests and never end to end on a D-PDU API.

B.5.1 ties the transmit indication to a transmit the protocol completed and does not exclude a
message the API generates itself. The API's own tester present (ADR-423) can therefore raise an
unbound transmit indication on the request identifier, which `note_indication` would take as the
exchange's transmit done. The backlog asked whether the handle needs a rule for it.

The API sends its tester present only on a COMM_STARTED link (8.2.6.3.2 e), B.5.1), and the
guard never turns the handling on or starts communication, so no guarded hardware check meets it
today. Even on a started link, 8.1.3 Table 1 holds a physical transmit until the previous
physical request's final answer (a response pending excepted), so a conforming library cannot
send the tester present between a request and its response. One sent before the request is
overridden by the handle's "latest unbound indication before the response" rule. With address
mode 1 it goes out on the functional identifier, which the handle's identifier check does not
match.

## Decision

1. **The mock has a binding control.** `SimEcuCommand::IndicationBinding { bound, request_id,
   response_id }` (JSON `{"indication_binding":{"bound":false,"request_id":2016,
   "response_id":2024}}`) makes the indication items unbound: `hCop` is `PDU_HANDLE_UNDEF` and
   `pExtraInfo` has four header bytes, the CAN identifier big-endian (`request_id` for a
   transmit indication, `response_id` for a start-of-message one). The default is bound, as
   before. `__mock_reset` resets it; `Detach` does not (like `response_timestamps`). The mock
   knows no CAN identifiers (ADR-299), so the control supplies them. `PDUDestroyItem` frees the
   extra info and its header bytes with the result item.
2. **The mock's tester present raises an unbound transmit indication.** When the ECU's own
   tester present (ADR-423 item 5) is sent, the link's active `CP_TransmitIndEnable` is on, the
   active tester-present address mode is 0 and the binding is unbound, the mock queues an
   unbound transmit indication with `request_id`, at the send's time. Otherwise it queues none: a
   bound mode has no identifier to give it, and a functional send goes out on an identifier the
   control does not supply. This is the literal reading of B.5.1.
3. **The handle keeps its rule.** `note_indication` takes the latest unbound indication on the
   request identifier before the response. "The first after the send" would be wrong for a
   tester present followed by the request, an order 8.1.3 Table 1 allows. The handle's unit
   tests show both the request identifier (taken) and the functional one (not taken) next to a
   bound indication.
4. **Related ADRs are annotated.** ADR-356's Consequences on the mock and on the tester present,
   and ADR-423 item 5, point here.

## Consequences

- `iso22900-service`'s `t2_indication_unbound` test shows the handle taking its times from
  unbound items end to end on a D-PDU API, with the mock's response items carrying no times, and
  a later exchange keeping its own transmit done after the mock's tester present has raised its
  indication.
- The agent's end-to-end coverage of the unbound path is still open. Design 8.11 `sent_evidence`
  takes an unbound transmit done only when the API sends no tester present on that identifier,
  and the agent's tests through the mock use the default, bound mode.
- The residual is a non-conforming library that sends a tester present between a request and
  its response. As in ADR-356 it can shorten a measured latency but never makes an on-time
  response late.

# ADR-423: The D-PDU Mock Keeps a Started Link's Tester-Present

**Date:** 2026-11-05
**Status:** Accepted (Decision item 5 amended by ADR-427: a send can raise an unbound transmit indication)
**Affects:** `crates/iso22900-mock` (`PDUSetComParam`, `PDUGetComParam`, `PDUGetStatus`,
`PDUStartComPrimitive`, `PDUConstruct`, the delivery thread, `NGR_SIM_ECU_COMMANDS`),
`crates/iso22900-mock/docs/`, `crates/iso22900-service/tests/grpc_mock.rs`; ADR-299, ADR-418,
ADR-419, ADR-420

## Context

The agent's keep-alive (ADR-418, ADR-420) sets the tester-present ComParams of ISO 22900-2:2022
Annex B on a link, sends the first session control as a `CoptStartcomm` primitive with a request,
and relies on the worker to send the tester-present while the link is COMM_STARTED and to report
a missing or negative answer. The J2534 worker does this against `sim-vci` (ADR-419). The D-PDU
worker leaves it to the vendor library, and `iso22900-mock` (ADR-299) offered nothing to test it
against: it resolved only two tester-present ComParams, so the agent's other six were refused and
it opened the link without the keep-alive; `PDUSetComParam` stored nothing; only a send-receive
reached the attached ECU, so a `CoptStartcomm` got the synthetic completion; and the link's state
was always reported as finished or online. The end-to-end test of the keep-alive through the
D-PDU worker (the second part of this work) needs a mock that behaves as a library which keeps
to the standard here.

## Decision

1. **The eight tester-present ComParams resolve.** `PDUGetObjectId` knows `CP_TesterPresentHandling`,
   `Message`, `ReqRsp`, `ExpPosResp`, `ExpNegResp`, `SendType`, `Time` and `AddrMode` (Annex B.5.1; the six
   it lacked take ids 3236 to 3241), so the agent's link opens with the keep-alive.
2. **Working and active values.** `PDUSetComParam` of one of them writes a working copy (8.4.16):
   a number (`UNUM32`) or a byte field (`ParamActLen` bytes of `pDataArray`, at most 12), and
   refuses a value of the wrong data type or a longer field. The working copy starts at the B.5.1
   defaults of UDS on CAN: handling on, message `3E 80`, no response expected, functional
   addressing, 2 s. The active copy is taken from the working one when a `CoptStartcomm` or a
   `CoptUpdateparam` finishes (8.2.6.1), so setting a ComParam alone changes nothing on the
   wire. `PDUGetComParam` reads a numeric one back from the working copy; a byte field still
   reads as the number 0, because `PDUDestroyItem` frees a ComParam's data as a number. The
   copies are cleared by `PDUDestroyComLogicalLink`, `PDUConstruct` and `__mock_reset`.
3. **Link state.** The mock keeps ONLINE or COMM_STARTED. A `CoptStartcomm` makes the link
   COMM_STARTED when it finishes, whatever the ECU answered. That is the mock's choice, as the
   J2534 worker's is: ISO 22900-2:2022 8.2.6.3.2 e) and 8.4.17.2.1 tie the state to a successful
   StartComm, but libraries differ here, which is why the agent asks the link after a refused
   session control (ADR-420 item 3) instead of assuming. `CoptStopcomm`, `PDUDisconnect`,
   `PDUDestroyComLogicalLink` and a reset make it ONLINE, and drop the state change of every
   ComPrimitive still waiting or running, so a `CoptStartcomm` that finishes after the link
   went away does not start it again. `PDUGetStatus` of a link (no ComPrimitive, 8.4.6) reports
   the state; module status and ComPrimitive status are as before (the marker of item 6 is not a
   ComPrimitive and never makes either read executing).
4. **A `CoptStartcomm` with a request is sent as a send-receive** (ADR-418 item 3, 8.2.6.3.2):
   its events and their control data (cycles) are those of ADR-299 items 2 and 3, and a request
   the ECU does not answer ends in the receive timeout and a finished status, with the link
   started. A `CoptUpdateparam` and a `CoptStopcomm` keep the synthetic completion; all three
   change the link state when that item is delivered. A second `CoptStartcomm` on a started link
   is a send-receive and leaves it started.
5. **The tester-present sends.** While the link is COMM_STARTED and the active handling is on, the
   delivery thread sends the active message to the attached ECU (`SimEcu::exchange`, physical
   addressing for address mode 0, functional otherwise): at once when the link starts or a
   `CoptUpdateparam` changes the active values (the note on the tester-present ComParams in B.5.1),
   then a period after each send. Send type 1 is sent as type 0, every period, and a period below
   1 ms is sent every millisecond. With no ECU attached a send is skipped without an error, and
   resumes when one is attached. Switching the handling off, a `CoptStopcomm`, a disconnect, a
   destroy and a reset stop it.
   Amended by ADR-427: with the indication binding unbound, the active transmit indication on
   and address mode 0, a send also raises an unbound transmit indication.
6. **Serialization (8.1.3 Table 1, as ADR-419 for the J2534 worker).** A tester-present goes only
   when no ComPrimitive runs or waits; a ComPrimitive that waits goes first and the
   tester-present stays due. This holds every tester-present behind the ComPrimitives, which is
   stricter than 8.1.3 Table 1 (a functional tester-present with no response may go at once
   behind a physical request): a simplification of the mock. A send that expects a response (`ReqRsp` 1) puts a marker into the
   schedule of the running ComPrimitive, due when the ECU's final answer is (response-pending
   messages included), and the next ComPrimitive begins only after it. A send that expects none
   holds nothing. The ECU's answer to a tester-present is never delivered as a result item.
7. **Judging and the error item.** With `ReqRsp` 0 an answer is never an error. With 1 the answer is
   positive when it starts with `ExpPosResp`; with that field empty it is not enabled (B.5.1
   compares only the bytes the field holds), so any answer is positive unless it starts with a
   non-empty `ExpNegResp`. A missing answer and the expected negative response are an error, as
   is any other answer when `ExpPosResp` is set. The marker, when due, queues one
   `PDU_IT_ERROR` item `PDU_ERR_EVT_TESTER_PRESENT_ERROR` for the link (hCop
   `PDU_HANDLE_UNDEF`) per unanswered or unexpectedly answered send, and sending goes on. It
   queues nothing when the answer was positive or the tester-present was switched off or
   the link left COMM_STARTED meanwhile. The additional error codes are the mock's own, as
   `MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP` is (ADR-351, D.4.2 leaves them to the vendor's MDF):
   `MOCK_XTRA_ERR_TP_NO_RSP` (0x102) and `MOCK_XTRA_ERR_TP_NEG_RSP` (0x103). The error item comes
   after the finished status of the `CoptStartcomm` that started the link.
8. **Faults from the start.** `NGR_SIM_ECU_COMMANDS`, a JSON array of the commands
   `__mock_sim_ecu_control` takes, is applied by `PDUConstruct` after the ECU of
   `NGR_SIM_ECU_CONFIG` is attached, when no ECU is attached yet. A worker process under test
   cannot call the back door, so this arms a fault such as the loss of the first tester-present
   answer (`{"inject_fault":{"fault":"drop_response","service":62}}`). A command that does not
   parse or cannot apply fails `PDUConstruct` and leaves no ECU attached, so a retried construct
   does not succeed without the faults. A variable that is set but blank names no commands.

## Consequences

- Every agent link through the mock now opens with the keep-alive (its `link_opened` says the
  library accepted the ComParams), and a `CoptStartcomm` with a request starts a tester-present
  with the defaults of item 2 when no ComParam was set: tests that count events or the ECU's
  requests see the tester-present traffic. The tests of the workspace that run against the mock
  needed no change.
- The mock's extra error codes grow by two; an MDF that names them is the test's own.
- ADR-299 is extended, not superseded: its statements that the mock keeps no ComParams and
  answers only a send-receive from the ECU hold except for the tester-present ComParams and
  the `CoptStartcomm` of this ADR.
- The mock does not model the bus: address mode 3 and 4 are sent as functional, the
  tester-present's own timing against P3 (`CP_P3Func`, `CP_P3Phys`) is not modelled, and one
  link is simulated.
- The end-to-end test of the agent's keep-alive through the D-PDU worker builds on this.

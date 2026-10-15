# ADR-299: iso22900-mock Answers ComPrimitives from sim-ecu

**Date:** 2026-10-15
**Status:** Accepted
**Affects:** `crates/iso22900-mock` (`PDUStartComPrimitive`, event items, `__mock_sim_ecu_control`,
`NGR_SIM_ECU_CONFIG`), `crates/iso22900-service/tests/grpc_mock.rs`

## Context

`sim-vci` passes the J2534 worker's requests to `sim-ecu` and returns its responses (ADR-238), so
the J2534 path can be run against a simulated ECU. The D-PDU worker had no such path:
`iso22900-mock` answered every ComPrimitive with one result item carrying no data, whatever the
request. The M2 end-to-end test has to run one definition through both workers with the same
results, and the D-PDU side needs an ECU behind the mock for that.

The mock's other tests depend on the synthetic answer, so the ECU cannot replace it outright.
The mock also keeps no ComParams, so it has no P2 time to wait for and no setting for the D-PDU
API's own response-pending handling.

## Decision

1. **Opt-in ECU.** The mock answers from `sim-ecu` only when an ECU is attached. Tests attach
   one through the back door `__mock_sim_ecu_control`, a JSON command (`attach` with optional
   `sim_ecu::EcuConfig` fields set over `sim-vci`'s default configuration, `detach`, `inject_fault` with an optional `service` or request prefix,
   as `sim-vci`'s control commands take them). A process that loads the mock as a vendor
   library attaches one at `PDUConstruct` when `NGR_SIM_ECU_CONFIG` names a configuration file,
   the variable `sim-vci` reads; a file that cannot be read or parsed fails `PDUConstruct`.
   Without an ECU, and for any ComPrimitive that is not `PDU_COPT_SENDRECV` or has no request
   bytes, the synthetic result item stays as it was.
2. **Event sequence.** A send-and-receive ComPrimitive's request goes to
   `SimEcu::exchange` with physical addressing. Its events follow ISO 22900-2 (2022)'s
   description of a non-cyclic send-and-receive ComPrimitive (8.2.6): the
   `PDU_COPST_EXECUTING` status item, a result item for each response-pending message, a
   result item with the response, then the `PDU_COPST_FINISHED` status item. When the ECU
   sends no response, a `PDU_ERR_EVT_RX_TIMEOUT` error item takes the response's place.
3. **Send and receive cycles.** The ComPrimitive's control data decides what is sent and
   received: no send cycles, nothing reaches the ECU; no receive cycles, the request reaches
   the ECU and nothing is received. Any other combination, periodic sends and several or
   cyclic receives included, runs as one send and one response. Without control data the mock
   sends once and receives once.
4. **No negative-response handling.** Response-pending messages reach the client as result
   items, as they do when the D-PDU API's handling of NRC 0x78 is switched off. The mock keeps
   no ComParams, so it cannot follow the client's setting; passing the messages on lets the
   worker and its callers see them, as `sim-vci` does.
5. **Timing and order.** A link runs its non-cyclic ComPrimitives one after another
   (8.2.6.5). While an ECU is attached, and while other ComPrimitives still wait or run after
   it is detached, a ComPrimitive therefore waits its turn: it begins when the one before it
   has finished, and only then does its request reach the ECU, so the ECU's state and timers
   (a session timeout, a reset's start-up time) run from when it begins. Its events are due at
   the delays `sim-ecu` gives them, counted from when it begins. One delivery thread begins
   each ComPrimitive in turn, queues each event when due and notifies the callback with the
   mock's state unlocked. The receive timeout is reported right after the last message the ECU
   sent, since there is no P2 to wait for. `PDUConstruct` and a reset discard waiting
   ComPrimitives and scheduled events; `PDUDestruct` discards them and waits for the delivery
   thread to end, so no callback runs after it returns and the caller may free the API tag and
   unload the library. `PDUGetStatus` reports a waiting ComPrimitive as idle and one with
   events still to come as executing.

## Consequences

- The D-PDU worker can be tested against the same simulated ECU as the J2534 worker, through the
  same fault model.
- A receive timeout comes without the delay a real VCI would take, so a test cannot observe P2
  timing through the mock.
- `PDUCancelComPrimitive` still does nothing: a ComPrimitive cancelled while its delayed events
  are pending still delivers them and finishes, and one cancelled while it waits its turn still
  reaches the ECU when it begins and changes the ECU's state. No test needs cancellation against
  the ECU yet.
- Functional addressing and the ECU state file of ADR-241 are not supported by the mock.

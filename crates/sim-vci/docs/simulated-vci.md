# Simulated VCI (`sim-vci`)

`sim-vci` is a cdylib exporting the J2534 v04.04 PassThru API. A worker loads it like a vendor
library, so the agent -> worker -> VCI path runs in CI without hardware (design 13.4). Behind the
API sits one simulated ECU from `sim-ecu` (`crates/sim-ecu/docs/simulated-ecu.md`). Clause numbers
below refer to SAE J2534-1 (v04.04).

## Device and channels

- `PassThruOpen` returns device ID 1. On the first open of the process it creates the ECU with the
  `sim_ecu::EcuConfig` read from the JSON file named by `NGR_SIM_ECU_CONFIG`, or with a built-in
  configuration (VIN `NGRSIMECU00000001`, part number `NGR-SIM-ECU`, software version `1.0.0`)
  when the variable is unset. An unreadable or invalid file makes the open fail with `ERR_FAILED`.
- The ECU lives as long as the process: `PassThruClose` and a new `PassThruOpen` keep its
  session and flash state, as a vehicle keeps its state while the tester disconnects. A process
  restart starts a fresh ECU.
- `PassThruConnect` accepts `ISO15765` with 11-bit CAN IDs only; any other protocol, or the
  29-bit ID flag, gets `ERR_NOT_SUPPORTED`. Channel IDs count up from 1. `PassThruClose` drops
  every channel together with its filters and unread responses; `PassThruDisconnect` drops one.
- All 14 J2534 v04.04 functions are exported, so `j2534-0404-service` loads the library like a
  vendor's. `PassThruReadVersion` reports firmware `NGR-SIM 1.0`, DLL `sim-vci <crate version>`
  and API `04.04` on an open device. `PassThruGetLastError` always reports a fixed text: the
  simulator keeps no error descriptions. `PassThruSetProgrammingVoltage` accepts any request on
  an open device, since no pins are simulated. Periodic messages are not simulated:
  `PassThruStartPeriodicMsg` returns `ERR_NOT_SUPPORTED`, and `PassThruStopPeriodicMsg`
  `ERR_INVALID_MSG_ID`.

## Filters

ISO 15765 channels follow clause 7.2.9 and Appendix A for flow-control filters:

- Only `FLOW_CONTROL_FILTER` is accepted; `PASS_FILTER` and `BLOCK_FILTER` get
  `ERR_INVALID_FILTER_ID`.
- The mask, pattern and flow-control messages must have the same size and TxFlags, and the mask
  must select the whole 4-byte CAN ID (`ERR_INVALID_MSG` otherwise). Extended addressing is not
  simulated (`ERR_NOT_SUPPORTED`).
- A pattern or flow-control ID already used by another filter of the channel gets
  `ERR_NOT_UNIQUE`; one filter may use the same ID for both. A channel takes ten filters, the
  minimum the clause asks for (`ERR_EXCEEDED_LIMIT` beyond).
- A response enters the receive queue only if a filter's pattern ID is the response's CAN ID.
  The ECU still answers on the bus without one, so a later filter does not bring back a missed
  response.
- A request longer than a single frame (more than 7 payload bytes) needs a filter whose
  flow-control ID is the request's CAN ID; without one it is not sent and the write returns
  `ERR_NO_FLOW_CONTROL`. Single frames need no filter.

## Messages

Messages carry the 4-byte CAN ID followed by the UDS A_Data (clause 8.3). The ECU uses the legacy
ISO 15765-4 identifiers of the first ECU:

| CAN ID | Meaning |
|---|---|
| `7E0` | physically addressed request |
| `7DF` | functionally addressed request |
| `7E8` | response |

`PassThruWriteMsgs` hands each request to `SimEcu::exchange` at once and queues the response, if
any, on the same channel; requests to any other CAN ID are sent but nobody answers. A message whose
protocol ID differs from the channel's gets `ERR_MSG_PROTOCOL_ID`, and one without at least one
byte after the CAN ID gets `ERR_INVALID_MSG`; `*pNumMsgs` then reports the messages sent before it.
The write timeout is ignored, since sending takes no time.

A response becomes readable after the delay `exchange()` reports (`EcuConfig::response_delay_ms`
plus any injected `Fault::DelayResponse`). Responses are read in the order they become readable,
so a delayed response can be overtaken by a later one. `PassThruReadMsgs` follows clause 7.2.5:
with a zero timeout it returns at once; otherwise it waits until the requested number of messages
is read or the timeout passes. Reading nothing gives `ERR_BUFFER_EMPTY`; reading fewer than
requested with a non-zero timeout gives `ERR_TIMEOUT`. A response the ECU does not send (dropped,
suppressed, silent ECU) never appears, nor does one still being delayed when the ECU power-cycles
(power loss, ECU reset, reconnection); responses already on the bus stay readable.

Simplifications:

- No TxDone indications or loopback messages are generated, and segmentation itself (flow
  control frames, separation time) is not simulated: a request or response of any length moves
  as one message.
- `RxStatus` is always zero. The timestamp is the moment the response appeared on the bus
  (after its delay), in microseconds since the library was first used.

## Fault injection

The faults of `sim_ecu::Fault` act on the ECU behind the VCI as described in `sim-ecu`'s document;
`sim-vci` applies the resulting delay or missing response on the read side. Today they can be
armed only from `sim-vci`'s own unit tests.

## Tests

`sim-vci`'s unit tests call the exports directly. `j2534-0404-service`'s
`tests/sim_vci_end_to_end.rs` loads the built cdylib through the real service, launched by
`worker-host` with the platform's `unsigned long` width (8 bytes on Linux x86_64), and reads the
VIN over an ISO 15765 link.

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
  every channel together with its unread responses; `PassThruDisconnect` drops one.

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

- No flow-control filter is needed before responses are queued, and no TxDone indications or
  loopback messages are generated.
- `RxStatus` is always zero. The timestamp is the moment the response appeared on the bus
  (after its delay), in microseconds since the library was first used.

## Fault injection

The faults of `sim_ecu::Fault` act on the ECU behind the VCI as described in `sim-ecu`'s document;
`sim-vci` applies the resulting delay or missing response on the read side. Today they can be
armed only from `sim-vci`'s own unit tests.

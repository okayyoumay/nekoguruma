# Simulated VCI (`sim-vci`)

`sim-vci` is a cdylib exporting the J2534 v04.04 PassThru API. A worker loads it like a vendor
library, so the agent -> worker -> VCI path runs in CI without hardware (design 13.4). Behind the
API sits one simulated ECU from `sim-ecu` (`crates/sim-ecu/docs/simulated-ecu.md`). Clause numbers
below refer to SAE J2534-1 (v04.04).

## Device and channels

- `PassThruOpen` returns device ID 1, and a new ID only after a lost device was closed (see
  "Control"). On the first open of the process it creates the ECU with the
  `sim_ecu::EcuConfig` read from the JSON file named by `NGR_SIM_ECU_CONFIG`, or with a built-in
  configuration (VIN `NGRSIMECU00000001`, part number `NGR-SIM-ECU`, software version `1.0.0`)
  when the variable is unset. An unreadable or invalid file makes the open fail with `ERR_FAILED`.
- The ECU lives as long as the process: `PassThruClose` and a new `PassThruOpen` keep its
  session and flash state, as a vehicle keeps its state while the tester disconnects. A process
  restart starts a fresh ECU.
- `PassThruConnect` accepts `ISO15765` with 11-bit CAN IDs only; any other protocol, or the
  29-bit ID flag, gets `ERR_NOT_SUPPORTED`. Channel IDs count up from 1. `PassThruClose` drops
  every channel together with its filters and unread responses; `PassThruDisconnect` drops one.
- All 14 J2534 v04.04 functions are exported (with the control function `NgrSimVciControl`,
  "Control" below), so `j2534-0404-service` loads the library like a vendor's on the targets
  where the exports' calling convention matches; the end-to-end test runs on the host target. On Windows x86 the exports still use the C calling convention, not the
  stdcall a vendor DLL uses there, so the service cannot load that build yet. `PassThruReadVersion` reports firmware `NGR-SIM 1.0`, DLL `sim-vci <crate version>`
  and API `04.04` on an open device. `PassThruGetLastError` always reports a fixed text: the
  simulator keeps no error descriptions. `PassThruSetProgrammingVoltage` drives nothing but keeps
  track of the pins by the rules of clause 7.2.11: 5 to 20 V on one of pins 0, 6, 9 and 11 to 14
  at a time (switch it off before using another pin), pin 15 shorted to ground only
  (`ERR_PIN_INVALID` otherwise), and `ERR_FAILED` for a voltage outside those values, for which
  the clause names no code. Closing the device switches every pin off. Periodic messages are not
  simulated:
  `PassThruStartPeriodicMsg` returns `ERR_NOT_SUPPORTED`, and `PassThruStopPeriodicMsg`
  `ERR_INVALID_MSG_ID`.
- `PassThruIoctl` implements `CLEAR_MSG_FILTERS` (removes the channel's filters) and
  `CLEAR_RX_BUFFER` (drops the responses already received; ones still being delayed arrive
  later) on a channel. Every other IOCTL is accepted and does nothing.

## Filters

ISO 15765 channels follow clause 7.2.9 and Appendix A for flow-control filters (ADR-234):

- Only `FLOW_CONTROL_FILTER` is accepted; `PASS_FILTER` and `BLOCK_FILTER` get
  `ERR_INVALID_FILTER_ID`.
- The mask, pattern and flow-control messages must have the same size and TxFlags, and the mask
  must select the whole 4-byte CAN ID (`ERR_INVALID_MSG` otherwise). The channel uses 11-bit IDs
  and normal addressing, so a filter with the 29-bit or extended-address TxFlag, or an ID above
  `7FF`, gets `ERR_INVALID_MSG`; so does a written message with either flag.
- A pattern or flow-control ID already used by another filter of the channel, in either role,
  gets `ERR_NOT_UNIQUE`; one filter may use the same ID for both. A channel takes ten filters, the
  minimum the clause asks for (`ERR_EXCEEDED_LIMIT` beyond).
- A response enters the receive queue only if a filter's pattern ID is the response's CAN ID.
  A segmented response (more than 7 bytes after the CAN ID) also needs that filter's flow-control
  ID to be `7E0`, where the device would send flow control to the ECU; a filter whose pattern and
  flow-control IDs are the same receives single frames only.
- Filters judge a response when it appears on the bus (after its delay), as a device filters
  what it receives: a filter started while a response is delayed lets it in, one stopped before
  it appears keeps it out, and a response kept out stays lost when a filter is started later.
- A request longer than a single frame (more than 7 payload bytes) needs a filter whose
  flow-control ID is the request's CAN ID and whose pattern ID is the partner answering there
  (`7E8` for `7E0`; any other ID for an address nobody simulates), since the partner's flow
  control arrives on the pattern ID. A filter with the same pattern and flow-control ID serves
  functional single frames only, and a request to the functional ID `7DF` is never segmented,
  whatever the filters (ADR-055). Without such a filter the request is not sent and
  the write returns `ERR_NO_FLOW_CONTROL`. Single frames need no filter.

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
`sim-vci` applies the resulting delay or missing response on the read side. Tests arm them, and
unplug the VCI, with control commands (ADR-238).

## Control

A control command is a JSON object tagged by `command`:

| Command | Effect |
|---|---|
| `{"command": "inject_fault", "fault": F}` | `SimEcu::inject(F)`; `F` is a `sim_ecu::Fault` in snake case: `"power_loss"`, `"drop_response"`, `"bus_error"`, `{"delay_response": {"ms": 500}}`, `{"corrupt_block": {"block": 3}}` |
| `{"command": "reconnect_ecu"}` | `SimEcu::reconnect`, which ends a power loss |
| `{"command": "disconnect_vci"}` | unplugs the VCI |
| `{"command": "connect_vci"}` | plugs it back in |

Unknown commands and unknown fields, in the command or in the fault, are rejected. A command that needs the ECU creates it first,
as the first `PassThruOpen` does. There are two ways to send one:

- `NgrSimVciControl(const char *command)` applies one command in the calling process and returns
  `STATUS_NOERROR`, `ERR_NULL_PARAMETER`, or `ERR_FAILED` for a command it cannot parse or an
  ECU configuration it cannot read. It does not look at the control directory, whose files wait
  for the next J2534 call.
- `NGR_SIM_VCI_CONTROL_DIR` names a directory, read once when the library is first called. For
  a test driving a worker process that loaded the library. At the start of every call into the
  library except `PassThruGetLastError`, and every 20 ms while `PassThruReadMsgs` waits,
  `sim-vci` applies the `*.json` files there in file-name order. It claims each file by renaming
  it to `*.applying` before reading it, so a command is applied at most once (a file it cannot
  claim is tried again at the next call), and deletes it once applied; a file it cannot read,
  parse or apply is renamed to `*.rejected`. Write each file as UTF-8 without a byte-order mark,
  under another name (such as `*.tmp`), and rename it when complete, so it is never read
  half-written. Name the files so that their order is the order to apply them in (`001.json`,
  `002.json`, ...).

If the device is open when the VCI is unplugged, the device is lost: every function except
`PassThruGetLastError` returns `ERR_DEVICE_NOT_CONNECTED`, before checking its arguments, and a
waiting `PassThruReadMsgs` returns with it at once, reporting the messages it had already read.
Following clause 6.10.1, the device stays lost after the VCI is plugged back in, until
`PassThruClose` on that device, which releases it (with its channels and unread responses) and
still reports the error. The next `PassThruOpen` returns a new device ID. With no device open,
an unplugged VCI only makes `PassThruOpen` fail with `ERR_DEVICE_NOT_CONNECTED`; the other calls
fail as they do for any closed device, and the open succeeds once the VCI is back. The ECU keeps
its state throughout.

A VCI crash, as opposed to a disconnect, is not simulated.

## Tests

`sim-vci`'s unit tests call the exports directly. `j2534-0404-service`'s
`tests/sim_vci_end_to_end.rs` loads the built cdylib through the real service, launched by
`worker-host` with the platform's `unsigned long` width (8 bytes on Linux x86_64), and reads the
VIN over an ISO 15765 link. Its `tests/sim_vci_control.rs` runs agent jobs against the same setup
and sends control commands through `NGR_SIM_VCI_CONTROL_DIR`: an ECU power loss and
reconnection, a VCI disconnect between jobs, and one while a link is open, after which
`GetVersion` reports the lost device until the next job closes and reopens it.

# Simulated VCI (`sim-vci`)

`sim-vci` is a cdylib exporting the J2534 v04.04 PassThru API. A worker loads it like a vendor
library, so the agent -> worker -> VCI path runs in CI without hardware (design 13.4). Behind the
API sits one simulated ECU from `sim-ecu` (`crates/sim-ecu/docs/simulated-ecu.md`). Clause numbers
below refer to SAE J2534-1 (v04.04).

`sim-vci` does not build on `j2534-0404-mock`, the J2534 test double for the worker's own tests,
and the two stay separate crates (ADR-260). They differ where it matters:

- **`unsigned long` width.** The mock exports the `j2534-0404-sys` binding types, always 32-bit.
  `sim-vci` uses the native `c_ulong` on 64-bit Linux, as the counterpart of
  `NGR_J2534_LONG_SIZE=8` (`docs/worker-crates.md`, "`unsigned long` width").
- **Responses.** The mock answers itself, for example with a loopback echo or per-protocol
  fixed replies. `sim-vci` hands ISO-TP requests to `sim-ecu`.
- **Test control.** The mock is driven per test through its `__mock_*` exports and reset.
  `sim-vci` holds one process-wide ECU, driven by control commands.
- **CI cost and dependencies.** `sim-vci` is cross-checked for every worker target, the mock is
  built for host tests only. Building one on the other would cross-build the whole mock, and
  would make the worker's test double depend on the vehicle simulator.

## Device and channels

- `PassThruOpen` returns device ID 1, and a new ID only after a lost device was closed (see
  "Control"). On the first open of the process it creates the ECU with the
  `sim_ecu::EcuConfig` read from the JSON file named by `NGR_SIM_ECU_CONFIG`, or with a built-in
  configuration (VIN `NGRSIMECU00000001`, part number `NGR-SIM-ECU`, software version `1.0.0`)
  when the variable is unset. An unreadable or invalid file makes the open fail with `ERR_FAILED`.
  The ECU's timers run on real time, so a non-default session ends after tS3_Server (5 s unless
  the configuration sets `s3_server_ms`) without a request, as on a vehicle.
  After an ECUReset it also answers nothing for `startup_ms` when the configuration sets it
  (`crates/sim-ecu/docs/simulated-ecu.md`); the requests in that time get no response.
- The ECU lives as long as the process: `PassThruClose` and a new `PassThruOpen` keep its
  flash state, and its session too if the device is opened again within tS3_Server, as a vehicle
  keeps its state while the tester disconnects. A process restart starts a fresh ECU, unless
  `NGR_SIM_ECU_STATE` names a state file ("ECU state across processes" below).
- `PassThruConnect` accepts `ISO15765` with 11-bit CAN IDs only; any other protocol, or the
  29-bit ID flag, gets `ERR_NOT_SUPPORTED`. Channel IDs count up from 1. `PassThruClose` drops
  every channel together with its filters and unread responses; `PassThruDisconnect` drops one.
- All 14 J2534 v04.04 functions are exported (with the control function `NgrSimVciControl`,
  "Control" below), so `j2534-0404-service` loads the library like a vendor's. The end-to-end
  test (`tests/sim_vci_end_to_end.rs`) runs on the host target, and `scripts/abi-roundtrip.sh`
  (`LAUNCH=1`) runs it against the service and library built for each worker target (Linux ARM
  under qemu-user; the Windows targets on a Windows runner, win-x86 under WOW64), with the `unsigned long` width the ABI table gives (design 7.1.2). Every export uses the platform's J2534 calling convention
  (`extern "system"`: stdcall on Windows x86, the standard C convention elsewhere, design
  7.1.2) under its plain name. `scripts/abi-roundtrip.sh` checks the names on each build it is
  given, including the Windows release builds on `main`, where a decorated stdcall name
  (`_Name@N`) would fail it. `PassThruReadVersion` reports firmware
  `NGR-SIM 1.0`, DLL `sim-vci <crate version>` and API `04.04` on an open device. `PassThruGetLastError` always reports a fixed text: the
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
  later) on a channel, and `READ_VBATT` on the device: it takes the device ID
  (`ERR_INVALID_DEVICE_ID` for any other, `ERR_NULL_PARAMETER` for a null output) and writes
  the battery voltage on pin 16 in millivolts, rounded to a tenth of a volt (clause 7.3.3). The
  voltage is 12.0 V until a test sets another with `set_battery_voltage` ("Control" below),
  and stays at that value for the life of the process: closing and opening the device, an ECU
  power loss or a VCI disconnect leave it alone, so a test that simulates low supply sets it
  itself.
  Every other IOCTL is accepted and does nothing.

## ECU state across processes

When `NGR_SIM_ECU_STATE` names a file, the ECU outlives the process that loaded the library, as
a vehicle outlives a crashed worker (ADR-241):

- The first time the process needs the ECU, it continues from the file if the file exists;
  `NGR_SIM_ECU_CONFIG` is then not read, since the file holds the configuration as it stands.
  Otherwise the ECU starts from the configuration, and the file is written.
- The whole ECU is kept (`sim_ecu::EcuSnapshot`): session, security, flash phase, download and
  image, software versions, DTCs, armed faults and the power-cycle count. Its timers keep
  running while no process has the library loaded: the file records the wall-clock time of the
  write, and the time since then counts against tS3_Server and the security delay.
- The file is rewritten after every change to the ECU (each request it handles, each control
  command that touches it), through a temporary file renamed over it. If it cannot be written,
  the call returns `ERR_FAILED`, and a request's response is not delivered; the change itself
  has taken effect in this process, so a control file whose change cannot be written is renamed
  `*.rejected` although it was applied. If the state of a new ECU cannot be written, the ECU is
  not created, and every call that needs it fails until it can be. Only a file that does not
  exist starts a fresh ECU: a path that cannot be read for any other reason, a file of another
  format version, or one whose state contradicts itself (`SimEcu::restore` refuses it) makes the
  call that first needs the ECU (`PassThruOpen` or a control command) fail with `ERR_FAILED`.
- Replacing the file is retried briefly when Windows reports it in use.
- The VCI side is not kept: channels, filters, unread responses, the battery voltage and the
  device ID start fresh in the new process.
- One process at a time may use a state file.

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
`sim-vci` applies the resulting delay or missing response on the read side. The response pending
messages of `ResponsePending` are queued on the channel like responses, each at its own delay,
ahead of the final response. Tests arm them, and
unplug the VCI, with control commands (ADR-238).

## Control

A control command is a JSON object tagged by `command`:

| Command | Effect |
|---|---|
| `{"command": "inject_fault", "fault": F}` | `SimEcu::inject(F)`; `F` is a `sim_ecu::Fault` in snake case: `"power_loss"`, `"drop_response"`, `"bus_error"`, `{"delay_response": {"ms": 500}}`, `{"corrupt_block": {"block": 3}}`, `{"response_pending": {"count": 2, "interval_ms": 300}}`, `{"negative_response": {"nrc": "conditions_not_correct"}}` (the response code by its snake-case name, not its number) |
| `{"command": "reconnect_ecu"}` | `SimEcu::reconnect`, which ends a power loss |
| `{"command": "disconnect_vci"}` | unplugs the VCI |
| `{"command": "connect_vci"}` | plugs it back in |
| `{"command": "set_battery_voltage", "millivolts": N}` | sets the battery voltage `READ_VBATT` reports (default 12000) |

Unknown commands and unknown fields, in the command or in the fault, are rejected. A command that needs the ECU creates it first,
as the first `PassThruOpen` does. There are two ways to send one:

- `NgrSimVciControl(const char *command)` (the same calling convention as the J2534 exports) applies one command in the calling process and returns
  `STATUS_NOERROR`, `ERR_NULL_PARAMETER`, or `ERR_FAILED` for a command it cannot parse or an
  ECU configuration it cannot read. It does not look at the control directory, whose files wait
  for the next J2534 call.
- `NGR_SIM_VCI_CONTROL_DIR` names a directory, read once when the library is first called. For
  a test driving a worker process that loaded the library. At the start of every J2534 call
  except `PassThruGetLastError`, and every 20 ms while `PassThruReadMsgs` waits,
  `sim-vci` applies the `*.json` files there in file-name order. It claims each file by renaming
  it to `*.applying` before reading it, so a command is applied at most once (a file it cannot
  claim is tried again at the next call, and the files after it wait until it is applied), and
  deletes it once applied; a file it cannot read, parse or apply is renamed to `*.rejected`. Write each file as UTF-8 without a byte-order mark,
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
answer as they do with no device open (a device or channel ID gets `ERR_INVALID_DEVICE_ID` or
`ERR_INVALID_CHANNEL_ID`, while an IOCTL the simulator ignores and `PassThruGetLastError` still
succeed), and the open succeeds once the VCI is back. The ECU keeps
its state throughout.

A VCI crash, as opposed to a disconnect, is not simulated.

## Tests

`sim-vci`'s unit tests call the exports directly. `j2534-0404-service`'s
`tests/sim_vci_end_to_end.rs` loads the built cdylib through the real service, launched by
`worker-host` with the platform's `unsigned long` width (8 bytes on Linux x86_64), and reads the
VIN over an ISO 15765 link. Its `tests/sim_vci_control.rs` runs agent jobs against the same setup
and sends control commands through `NGR_SIM_VCI_CONTROL_DIR`: an ECU power loss and
reconnection, the battery voltage through `PDU_IOCTL_READ_VBATT` before and after
`set_battery_voltage`, a VCI disconnect between jobs, and one while a link is open, after which
`GetVersion` fails, also after the VCI is back, until the test closes the link (the service may
answer that itself once its own polling has seen the loss); the next job then opens the device
again and reads the VIN. Since the service may answer for a lost module itself (ADR-131),
`tests/sim_vci_library.rs` loads the cdylib into its own process through the `j2534-0404`
wrapper and checks the device-loss rules without the service: lost after the unplug, still
lost after the replug, released by the close, and a new device ID on the next open. `tests/sim_vci_restart.rs`
runs each step in its own process with a state file: one starts a download and exits without
closing anything, the next reads the running transfer and the programming session (DIDs FD00
and F186), and one started after tS3_Server reads the interrupted transfer in the default
session. `tests/sim_vci_response_pending.rs` runs agent jobs through the worker
while `sim-ecu` answers with response pending. In a chain whose gaps are longer than P2 and
shorter than P2*, the job still gets the final response: the worker restarts its timer with P2*
on each 0x78, and the agent host skips the 0x78s the worker passes on. A chain that outlasts the
link's 0x78 completion timeout ends the job with `NoResponse`.

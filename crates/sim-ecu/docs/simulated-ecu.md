# Simulated ECU (`sim-ecu`)

`sim-ecu` answers UDS requests the way a server described by ISO 14229-1 (2026 edition) would, so
that the agent, the worker services and `sim-vci` can be tested without a vehicle (design 13.4).
Clause numbers below refer to that edition. It ships in the SDK, behind `sim-vci` (ADR-506).

The standard leaves many choices to the vehicle manufacturer or system supplier. This document
lists the ones the simulator makes, so that tests and IR programs written against it know what to
expect. Anything not listed follows the clause of the service.

## Entry point

`SimEcu::request(message)` handles a physically addressed request; `request_with(addressing,
message)` takes the addressing mode. `message` is the A_Data starting with the SID. The result is a
`SimResponse`: a positive response (response SID included), a negative response (`7F SID NRC`
via `to_bytes()`), or no response. `exchange(addressing, message)` returns the same response
together with the delay before it is sent (`EcuConfig::response_delay_ms` plus any injected
delay); the VCI side (`sim-vci` for the J2534 worker, `iso22900-mock` for the D-PDU worker,
ADR-299, and `ngr-stand-in-ecu` on a SocketCAN interface) applies it. Its `reached` says whether the request reached the ECU at all: not while
the ECU is off or starting up after a reset, nor when a bus error or `drop_at_block` loses it.
`sim-vci` gives such a request no TxDone when it is segmented, since no flow control would come
(ADR-352).

The ECU's own timers (the session timeout and the security delay, below) run on a `Clock`.
`SimEcu::new` uses real time (`SystemClock`); `SimEcu::with_clock(config, clock)` takes another,
such as a `ManualClock` that moves only when a test calls `advance` (design 13.4, clock
substitution). Timers are evaluated when the next request arrives, or when a test calls
`check_timers()` before reading the state directly.

General behaviour (clause 7.7):

- Checks run in the order of each service's NRC evaluation figure; the first failing check decides
  the NRC.
- The suppressPosRspMsgIndicationBit suppresses positive responses only.
- For functionally addressed requests, NRCs 11, 7F, 12, 7E and 31 are not sent (clause 7.7.1).
- A supported SubFunction is available in every session its service is, so NRC 7E is not produced.
- Every service completes within the request, so busy (NRC 21) is never produced, and response
  pending (NRC 78) only when a test injects it (`Fault::ResponsePending`, below). Response delays
  are reported by `exchange()` and applied by the VCI side.

## Services and sessions

| SID | Service | Sessions | Supported SubFunctions / parameters |
|---|---|---|---|
| 10 | DiagnosticSessionControl | all | 01 default, 02 programming, 03 extended |
| 11 | ECUReset | all | 01 hard, 02 key off/on, 03 soft (all behave as a power cycle) |
| 14 | ClearDiagnosticInformation | all | group FFFFFF or one stored DTC; no MemorySelection |
| 19 | ReadDTCInformation | all | 01, 02, 0A |
| 22 | ReadDataByIdentifier | all | up to 8 DIDs per request (DIDs below) |
| 27 | SecurityAccess | extended, programming | 01 requestSeed, 02 sendKey |
| 2E | WriteDataByIdentifier | all (VIN: extended, programming) | F190 and the configured DIDs that declare a write (below) |
| 31 | RoutineControl | all (routines: programming) | 01 startRoutine on FF00, FF01 |
| 34 | RequestDownload | programming | no compression or encryption |
| 36 | TransferData | programming | |
| 37 | RequestTransferExit | programming | no parameter record |
| 3E | TesterPresent | all | 00 |

Any other SID gets NRC 11.

The DiagnosticSessionControl response reports tP2_Server_Max and tP2*_Server_Max
(`EcuConfig::p2_server_max_ms` and `p2_star_server_max_ms`; 50 ms and 5000 ms by default, the
values ISO 14229-2 (2021) clause 9.2, Table 4, recommends), P2 in 1 ms units and P2* in 10 ms
units, rounded down and capped at 0xFFFF. They are reported only: when the ECU responds is set by
`response_delay_ms` and `Fault::ResponsePending`, so a test can also make it break the timing it
reports.

A non-default session times out after tS3_Server without a request (`EcuConfig::s3_server_ms`,
5000 ms by default, the value ISO 14229-2 (2021) clause 9.5, Table 5, gives). Following Table 6
of that clause (ADR-239), the timer restarts with every request
that reaches the ECU, whatever the service and whether it is supported, and runs from when the
response goes out (after its delay), also when that response is then lost (`Fault::DropResponse`),
or from the request when no response is sent; when responses overtake each other, it runs from
the last one to go out. A request lost on the bus does not restart it.
Timers keep running while the ECU is powered off or silent. When it expires, the ECU falls back to the default session as
for a change to it: security is relocked and a running download is interrupted. The default
session has no timer. An ECU reset or `reconnect()` also ends a non-default session. `hold_session()` stops the timer of a non-default session that has not timed out until the
next request reaches the ECU, which is handled in that session and restarts the timer as usual;
the hold is kept in the state file, so a test harness can hold the session between the runs of
two processes. A power cycle ends a hold.

Every DiagnosticSessionControl request relocks security, including a restart of the active
session. A running download is interrupted (see below) by any session other than programming,
and also by a restart of the programming session when the download was opened (or resumed)
with `EcuConfig::require_security_access` set, since it then depends on the security access the
relock takes away. A download opened with the flag unset keeps running across a restart of the
programming session, with its blockSequenceCounter and block numbering continuing.
A rejected DiagnosticSessionControl request changes nothing.

Services 2E, 31, 34, 36 and 37 need gateway authentication when `EcuConfig::require_gateway_auth`
is set; NRC 34 is returned until `SimEcu::gateway_authenticated` is set (there is no
Authentication service in the simulator; `sim-vci`'s `set_gateway_authentication` control
command sets it). The check is a service-level one at the position of the general behaviour
figure (clause 7.7.2), so it comes before each service's own length and range checks. Authentication is the gateway's state, not the ECU's, so a power cycle, ECU reset
or `reconnect()` of the simulated ECU leaves it as it is.

## Data identifiers

| DID | Content |
|---|---|
| F186 | active session (diagnosticSessionType value) |
| F187 | `EcuConfig::part_number`, always `PART_NUMBER_LENGTH` (11) bytes: padded with spaces when shorter, cut when longer |
| F189 | the running software version (`SimEcu::running_sw_version`): `EcuConfig::sw_version` at power-up, then the version of the last verified download (below), always `SW_VERSION_LENGTH` (5) bytes, padded or cut as F187 is |
| F190 | `EcuConfig::vin`; writable as 17 ASCII alphanumeric bytes, behind security access |
| FD00 | flash state: phase code (1 byte), block number (4 bytes), bytes received of the current download (4 bytes), big endian |
| FD01 | `SimEcu::power_cycles` (ECU resets, power losses, reconnections) as 4 bytes, big endian, saturating |
| FD04 | a fixed battery voltage, `BATTERY_VOLTAGE_RAW` (0x007D), as 2 bytes, big endian; the example ECU definition reads it in 0.1 V units as 12.5 V |
| FD05 | a fixed boot time, `BOOT_TIME_RAW` (0x002D), as 2 bytes, big endian; the example ECU definition reads it in 10 ms units as 450 ms |
| FD06 | an external supply connected, as 1 byte (1), in every session, for a definition that reads the external-supply precondition from the ECU |
| FD07 | the ignition on, as 1 byte (1), in every session, for the ignition precondition in the same way |

`EcuConfig::validate` refuses a part number, software version or `downloaded_sw_version` that is not ASCII or is longer than its width, and a VIN that is not exactly 17 ASCII bytes (F190 is a fixed 17-byte row); `sim-vci` and `iso22900-mock` call it when they load or attach a configuration. A `SimEcu` built directly from a longer value cuts it, so the answer keeps its length. A configured `sw_version` or `downloaded_sw_version` shorter than 5 bytes is read back space-padded, so a job's intended software version, which the agent compares as the field's raw bytes (ADR-268), must include the padding.

### Configured data identifiers

`EcuConfig::dids` adds DIDs beyond the built-in ones above (F186, F187, F189, F190, FD00, FD01 and
FD04 to FD07, which a configuration cannot replace). Each entry has `did`, `value` (its record;
its length is the DID's length) and, when the DID is writable, `write` with an optional `session`
(`default`, `programming` or `extended`; absent means every session) and an optional
`security_level` (only 1 exists; absent means no security check). Without `write` the DID is
read-only. `EcuConfig::security_key_mask` sets the key mask (below).

`did` and `security_key_mask` take a JSON number or a string of hex digits (either case) after a
lowercase `0x` prefix; `value` takes an array of bytes or a string of an even number of hex digits
with no prefix and no separators.
Both forms mean the same, and a malformed string is a parse error. The hex strings are for human-readable
formats; the state file and the snapshot hold numbers and byte arrays.

```json
{"security_key_mask": "0x3C3C3C3C",
 "dids": [{"did": "0x1200", "value": "010002"},
          {"did": "0x1201", "value": "000000",
           "write": {"session": "extended", "security_level": 1}}]}
```

```json
{"security_key_mask": 1010580540,
 "dids": [{"did": 4608, "value": [1, 0, 2]},
          {"did": 4609, "value": [0, 0, 0],
           "write": {"session": "extended", "security_level": 1}}]}
```

`EcuConfig::validate` also refuses a configured DID that is built-in, declared twice, has an empty
value or one longer than a response can carry (4092 bytes), has a `security_level` other than 1, or
gives a security level without naming a non-default session (an absent session means every
session, the default one included, where SecurityAccess is not available).
`SimEcu` built from an unvalidated configuration does not fail: a built-in DID wins and the first
of two equal DIDs wins.

A write to a configured DID is checked in this order: gateway authentication (NRC 34, all
writes), a request shorter than 4 bytes (NRC 13), then a DID that is not configured or has no
`write`, or a session other than the one `write` names (NRC 31), a record whose length differs from
the value's (NRC 13), and an unlocked level the DID needs (NRC 33). On success the record replaces
`value` and the response echoes the DID; an armed `Fault::DropResponse` applies the write and loses the answer, and
`Fault::NegativeResponse` refuses the request with no effect. A functionally addressed request that is refused with NRC 31 gets no response.

Security for a configured DID is per DID (ADR-482): one with a `security_level` always needs the
level unlocked, whatever `require_security_access` says, and one without never checks. The flag
gates only the VIN write, eraseMemory and the download.

The example ECU definition (`crates/diag-frontend/examples/example-ecu`) also declares FD02 and
FD03, which the simulator does not answer (RequestOutOfRange).

FD00 phase codes: 00 idle, 01 erased, 02 transferring (next block), 03 transfer complete,
04 verified, 05 interrupted (last stored block). The received byte count is zero when no
download is open. It is the state check a resume starts with (design 8.2.5): the resume address
is the download start plus the received count.

## Security access

One level (01/02). The seed is 4 bytes, deterministic per `SimEcu` instance and never zero; the
expected key is `seed XOR 5A5A5A5A` by default; `EcuConfig::security_key_mask` replaces the mask (`EcuConfig::key_for_seed`, also on `SimEcu`). The handling follows the state chart
in Annex I with a fresh seed for every requestSeed (no static seed). Any SecurityAccess request
other than a successful requestSeed discards the pending seed (Annex I, transition 9): a seed
answers exactly one sendKey, and a sendKey without a seed gets NRC 24.
The third false key in a row returns NRC 36 and starts the delay timer; requestSeed then returns
NRC 37 until the delay has run (`EcuConfig::security_delay_ms`, 10000 ms by default; clause 9.4.1
leaves the length to the vehicle manufacturer) or a test calls `SimEcu::expire_security_delay()`.
`security_delay_active()` reports whether it is running. A power cycle or ECU reset clears the
false-attempt counter and starts a delay that is still running again for its full length (after an
ECU reset, from when its response goes out; the reset itself is applied at once, so a request
arriving before a delayed reset response already sees the reset state); one that has already run
out stays over. Clause 9.4.1 also asks a server that supports the delay to start it at power-up
when an earlier SecurityAccess failed on a single false key; the simulator does not. With
`EcuConfig::require_security_access` unset, the VIN write, eraseMemory and the download do not check the lock state; a configured DID with a `security_level` still does (ADR-482).

## Routines and download

- `FF00` eraseMemory erases the whole simulated flash and discards any download progress. Its
  option record is accepted and ignored. Refused with NRC 22 while a download runs.
- `FF01` checkProgrammingDependencies is the integrity check after RequestTransferExit. Its
  routineStatusRecord is one byte: 00 image correct (state becomes verified), 01 image incorrect
  (`EcuConfig::fail_checksum`).
- A download that passes the check installs `EcuConfig::downloaded_sw_version` (when set): F189
  keeps reporting the old version until the next power cycle (ECU reset, power loss or
  `reconnect()`) and the new one from then on. An erase or a failed check before that power
  cycle withdraws the install. This lets a test read back the version after an interruption
  between the transfer and its verification, as the state check of design 8.2.5 does: a
  restart before the check still reports the old version, one after it the new one.
- RequestDownload accepts addresses in `FLASH_START .. FLASH_START + FLASH_SIZE` and needs an
  erased flash; otherwise NRC 22. The positive response reports maxNumberOfBlockLength
  `MAX_BLOCK_LENGTH` (whole TransferData request).
- TransferData repeating the previous blockSequenceCounter with the same data is answered again
  without storing the data twice, also after the last block. The same counter with different data
  gets NRC 73.

## Interruption and resume

`EcuConfig::drop_at_block = Some(n)` makes the ECU drop block `n` (counted from 1 over the whole
download) and answer nothing until `reconnect()`. It fires once: the field is cleared when it does. `reconnect()`, ECUReset and the
session changes described above turn a running download into the interrupted state, keeping the
data already stored.

A download resumes with a RequestDownload whose address and size cover exactly the part not yet
received; the blockSequenceCounter starts again at 1. Any other RequestDownload in the interrupted
state gets NRC 22, and the client must erase and start over. A download interrupted after its last
block but before RequestTransferExit has nothing left to send; RequestTransferExit closes it
directly.

`SimEcu::snapshot()` records the whole ECU as an `EcuSnapshot` (serde), and
`SimEcu::restore(snapshot, clock, elapsed)` rebuilds it on another clock, as it was `elapsed`
after the snapshot: running timers have that much less to go, and one that has run out takes
effect at once. It refuses (`InvalidSnapshot`) a snapshot whose download, image and block
numbers contradict each other or exceed the flash window, which the ECU itself never produces. The snapshot holds the configuration, so a value written to a configured DID and the key mask survive with it. `sim-vci` uses this to keep the ECU across worker processes (ADR-241). The `persist` module holds the files that start an ECU, shared by `sim-vci` and `ngr-stand-in-ecu`: `read_ecu_config` reads and checks a JSON `EcuConfig`, `default_ecu_config` is the configuration used when none is given, and `load_ecu_state` and `save_ecu_state` read and write the state file (postcard, a version number first, the snapshot, and the wall-clock time of the write, which counts against the ECU's timers when the next process reads it). A state file is written through a temporary file renamed over it, and one of another version is refused.

## Fault injection

`SimEcu::inject(fault)` arms one of the faults design 13.4 asks for. Each fires once.

| `Fault` | Effect |
|---|---|
| `DelayResponse { ms }` | The response to the next request goes out `ms` later; added to `response_delay_ms` in `exchange()` |
| `DropResponse` | The next request is handled and its state changes apply, but its response is lost |
| `BusError` | The next request is lost on the bus: neither handled nor answered |
| `PowerLoss` | Immediate: session, security and a running transfer are lost as in a power cycle (flash progress is kept), and the ECU answers nothing until `reconnect()` |
| `CorruptBlock { block }` | Writing TransferData block `block` (numbered as for `drop_at_block`) fails: NRC 72 (clause 14.4), nothing is stored and the blockSequenceCounter does not advance, so the client may send the block again |
| `NegativeResponse { nrc }` | The next request that reaches the ECU is refused with `nrc` and has no effect: it is not dispatched, so no state changes (an ECU reset does not happen, a session does not switch). Sent whatever the addressing and the suppress bit; tS3_Server restarts as for any request that reaches the ECU |
| `TruncateResponse { len }` | The next positive response is cut to its first `len` bytes (at least its service identifier): the request is handled as usual, and the response no longer answers it (ADR-359). A request with no positive response leaves the fault armed; a `len` at or above the response's length uses the fault up without cutting; with `DropResponse` armed too, the cut response is lost |
| `RepeatResponse { interval_ms }` | The next final response goes out twice, the second copy `interval_ms` after the first (ADR-400). A request with no response leaves the fault armed; with `DropResponse` armed too, both copies are lost and the fault stays armed |
| `ResponsePending { count, interval_ms }` | The next request the ECU answers gets `count` response pending messages (`7F SID 78`, Annex A.1) before its response: the first when the response was due, the others `interval_ms` apart, and the response `interval_ms` after the last. `exchange()` lists them in `Exchange::pending` with their delays; tS3_Server restarts when the final response goes out. A request without a response leaves the fault armed (a request with the suppress bit is not one, see below); with `DropResponse` the messages still go out and only the final response is lost. `count` is capped at `MAX_RESPONSE_PENDING` (1000); zero uses the fault up without sending any |

`ResponsePending` simplifies what a server does around NRC 78 (Annex A.1, clause 7.7.3):

- A request with the suppress bit set that a `ResponsePending` with a count above zero fires on is
  handled as if the bit were clear: after a response pending message a final response is owed
  whatever the bit says (clause 7.7.3.2), so the positive response follows the last message
  (the bit is cleared in the sub-function it echoes). Only the services the simulator gives a
  sub-function read the bit (DiagnosticSessionControl, ECUReset, SecurityAccess,
  TesterPresent, ReadDTCInformation, RoutineControl). A request that has no response for another
  reason leaves the fault armed.
- The chain can precede any final response, also a negative one a server would have given at
  once (NRC 11, 12 or 13), although NRC 78 says the request was accepted.
- During the chain the ECU answers other requests at once, and the request's own effect (an ECU
  reset, for one) applies at the start of the chain, not at its end.
  The exception is an ECU reset with `EcuConfig::startup_ms` set: its startup runs from when
  the final response goes out, so the requests during the chain and the startup are all lost.

After an ECUReset the ECU can start up before it answers again (`EcuConfig::startup_ms`, unset
by default: it answers at once). The reset itself is answered as usual; for `startup_ms` from
when its response goes out (also when that response is lost) the ECU answers nothing, and a
request in that time is lost as on a bus error, without firing an armed fault. The time is
measured on the ECU's clock, like the other timers, and a snapshot keeps the time left.
`reconnect()` and `Fault::PowerLoss` start no startup (a power loss stays silent until
`reconnect()`, which also ends a startup in progress).

`SimEcu::inject_for_request(prefix, fault)` arms a fault for one kind of request: it waits,
passing other requests by, until a request that starts with `prefix` reaches the ECU (`22 F1 86`
for a read of DID F186, or just the service identifier), and then fires on that request as it
would have fired on the next one after `inject`. `inject_for_service(sid, fault)` is the same with
the one-byte prefix `sid`. Faults waiting for the same request are armed together, in injection
order and ahead of the faults `inject` armed; a `PowerLoss` takes effect when that request
arrives, so it gets no answer. A request lost on the way (the ECU silent or starting up, or a
bus error) does not count. Once armed, a fault behaves as after `inject`: one that does not fire
on that request (a second fault of the same kind, a `ResponsePending` on a request without a
response) stays armed for the next request of any kind. A request a `BusError` armed by `inject`
loses never reaches the ECU, so the faults waiting for it wait on; a `BusError` aimed at a
request loses that request. This lets a test hit a request it cannot time, such as a client's
ECUReset or session read that follows a run of other requests. `faults_for_request()` lists the
faults still waiting, with their prefix and the number of matching requests left before each fires.
`inject_for_nth_request(prefix, n, fault)` waits for the `n`-th request that starts with `prefix`
instead of the first (`inject_for_request` is `n` = 1): each armed entry counts the matching
requests that reach the ECU on its own, so an earlier match is answered as it would be without
this entry (it may still get another armed fault). Requests that do not match or are lost on the
way, including to a `BusError` or `PowerLoss` aimed at that very request, do not count, and the
fault is spent when it fires. The count is part of the snapshot, so it carries over a restart that
continues from a saved state.

While the ECU is silent (after `PowerLoss` or `drop_at_block`), requests do not reach it and the
armed faults stay armed. `armed_faults()` lists the faults that have not fired yet.
`power_cycles()` counts power cycles (power loss, ECU reset, `reconnect()`); the VCI side uses it
to discard responses it was still delaying when the ECU lost power or reset.
`Fault` serializes in snake case (`"power_loss"`, `{"delay_response": {"ms": 500}}`), the form
`sim-vci`'s control commands take. A response code is named the same way, not by its number:
`{"negative_response": {"nrc": "conditions_not_correct"}}`.

## Stand-in ECU on SocketCAN (`ngr-stand-in-ecu`)

`ngr-stand-in-ecu` (`crates/ngr-stand-in-ecu`) serves a `SimEcu` on a Linux SocketCAN interface:
the stand-in ECU of the T1 hardware checks (design 13.5, ADR-275). It uses the kernel's ISO-TP
sockets (`CAN_ISOTP`, the `can-isotp` module), so the kernel segments messages and runs the flow
control of ISO 15765-2 (2024) clauses 9 and 10 on classical CAN, and the process reads and writes
whole UDS messages. It opens two sockets on the interface: physical requests (receive on the
request ID, send on the response ID) and functional requests (receive on the functional ID only).
A request goes to `SimEcu::exchange` with its addressing; the messages the exchange returns
(response pending messages, the response, repeated responses) all go out on the physical socket,
each once its delay from its request has passed, and a response that is `NoResponse` is not sent.
Requests keep being taken while messages wait, so a delayed response, a response pending chain or
a repeated response holds up no other request. Responses use the physical
socket for both addressings because functional addressing is single-frame only (ISO 15765-2
(2024) 8.3.2.4): a segmented response is a physical transfer, and the tester sends its flow
control to the request ID. The kernel cannot restrict the functional socket to single frames, so
a segmented functional request is accepted. A transfer the kernel reports as failed (the tester
stops mid-request or sends bad flow control, or none for a response) is logged to stderr and
dropped; the server goes on. A request longer than the read buffer is logged and dropped. The sockets use normal addressing and no padding. It builds on every platform, but off
Linux the binary exits with an error saying it is Linux only.

```text
ngr-stand-in-ecu --interface <ifname> [--request-id <id>] [--response-id <id>]
                 [--functional-id <id>] [--extended] [--config <file>] [--state <file>]
```

| Flag | Meaning |
|---|---|
| `--interface <ifname>` | The CAN interface (required), for example `vcan0` |
| `--request-id`, `--response-id`, `--functional-id` | The physical request, response and functional request CAN IDs; default `0x7E0`, `0x7E8` and `0x7DF` (the IDs `sim-vci` uses). Decimal or `0x` hexadecimal; the three must differ |
| `--extended` | The three IDs are 29-bit IDs (the kernel sees them with the extended-frame flag); an ID too wide for the selected width is refused. Default: 11-bit |
| `--config <file>` | A JSON `EcuConfig` (the format of `sim-vci`'s `NGR_SIM_ECU_CONFIG`): the ECU's identity, response delay, `drop_at_block`, DIDs and so on. Without it the built-in configuration is used |
| `--state <file>` | The state file (the format of `sim-vci`'s `NGR_SIM_ECU_STATE`, ADR-241) |

Faults come from the configuration only: the process has no runtime control channel. With
`--state`, the process continues from the file if it exists (and then reads no `--config`), and
writes it at start, after every exchange before the response goes out (a write that fails ends the
process without sending the response), and when SIGTERM or SIGINT stops it. A new process
counts as the ECU's power-up: its session and security state are gone and an ECU that went
silent (`drop_at_block`, `Fault::PowerLoss`) answers again, with its flash progress kept, so a
restart with the same file continues an interrupted download as described under "Interruption
and resume". The process prints `ngr-stand-in-ecu: serving on <ifname>` once its sockets are
open.

On a virtual interface, without hardware:

```sh
sudo modprobe vcan can-isotp
sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
echo '{"vin":"NGRSIMECU00000001","part_number":"NGR-SIM-ECU","sw_version":"1.0.0",
       "response_delay_ms":0,"drop_at_block":2,"require_security_access":false,
       "require_gateway_auth":false,"fail_checksum":false}' > ecu.json
cargo run -p ngr-stand-in-ecu -- --interface vcan0 --config ecu.json --state ecu.state
# a UDS client on vcan0 now reaches the ECU at request ID 0x7E0, response ID 0x7E8
```

`crates/ngr-stand-in-ecu/tests/vcan.rs` does this from a client ISO-TP socket. It runs only with
`NGR_TEST_VCAN=1`, and then fails, rather than skips, when `vcan0` or `can-isotp` is missing.

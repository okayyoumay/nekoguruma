# Simulated ECU (`sim-ecu`)

`sim-ecu` answers UDS requests the way a server described by ISO 14229-1 (2026 edition) would, so
that the agent, the worker services and `sim-vci` can be tested without a vehicle (design 13.4).
Clause numbers below refer to that edition.

The standard leaves many choices to the vehicle manufacturer or system supplier. This document
lists the ones the simulator makes, so that tests and IR programs written against it know what to
expect. Anything not listed follows the clause of the service.

## Entry point

`SimEcu::request(message)` handles a physically addressed request; `request_with(addressing,
message)` takes the addressing mode. `message` is the A_Data starting with the SID. The result is a
`SimResponse`: a positive response (response SID included), a negative response (`7F SID NRC`
via `to_bytes()`), or no response. `exchange(addressing, message)` returns the same response
together with the delay before it is sent (`EcuConfig::response_delay_ms` plus any injected
delay); the VCI side (`sim-vci`) applies it.

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
| 2E | WriteDataByIdentifier | all (VIN: extended, programming) | F190 only |
| 31 | RoutineControl | all (routines: programming) | 01 startRoutine on FF00, FF01 |
| 34 | RequestDownload | programming | no compression or encryption |
| 36 | TransferData | programming | |
| 37 | RequestTransferExit | programming | no parameter record |
| 3E | TesterPresent | all | 00 |

Any other SID gets NRC 11.

A non-default session times out after tS3_Server without a request (`EcuConfig::s3_server_ms`,
5000 ms by default, the value ISO 14229-2 (2021) clause 9.5, Table 5, gives). Following Table 6
of that clause (ADR-239), the timer restarts with every request
that reaches the ECU, whatever the service and whether it is supported, and runs from when the
response goes out (after its delay), also when that response is then lost (`Fault::DropResponse`),
or from the request when no response is sent; when responses overtake each other, it runs from
the last one to go out. A request lost on the bus does not restart it.
Timers keep running while the ECU is powered off or silent. When it expires, the ECU falls back to the default session as
for a change to it: security is relocked and a running download is interrupted. The default
session has no timer. An ECU reset or `reconnect()` also ends a non-default session.

Every DiagnosticSessionControl request relocks security, including a restart of the active
session. A running download is interrupted (see below) by any session other than programming,
and also by a restart of the programming session when the download was opened (or resumed)
with `EcuConfig::require_security_access` set, since it then depends on the security access the
relock takes away. A download opened with the flag unset keeps running across a restart of the
programming session, with its blockSequenceCounter and block numbering continuing.
A rejected DiagnosticSessionControl request changes nothing.

Services 2E, 31, 34, 36 and 37 need gateway authentication when `EcuConfig::require_gateway_auth`
is set; NRC 34 is returned until `SimEcu::gateway_authenticated` is set (there is no
Authentication service in the simulator). The check is a service-level one at the position of
the general behaviour figure (clause 7.7.2), so it comes before each service's own length and
range checks. Authentication is the gateway's state, not the ECU's, so a power cycle, ECU reset
or `reconnect()` of the simulated ECU leaves it as it is.

## Data identifiers

| DID | Content |
|---|---|
| F186 | active session (diagnosticSessionType value) |
| F187 | `EcuConfig::part_number` |
| F189 | the running software version (`SimEcu::running_sw_version`): `EcuConfig::sw_version` at power-up, then the version of the last verified download (below) |
| F190 | `EcuConfig::vin`; writable as 17 ASCII alphanumeric bytes, behind security access |
| FD00 | flash state: phase code (1 byte), block number (4 bytes), bytes received of the current download (4 bytes), big endian |

FD00 phase codes: 00 idle, 01 erased, 02 transferring (next block), 03 transfer complete,
04 verified, 05 interrupted (last stored block). The received byte count is zero when no
download is open. It is the state check a resume starts with (design 8.2.5): the resume address
is the download start plus the received count.

## Security access

One level (01/02). The seed is 4 bytes, deterministic per `SimEcu` instance and never zero; the
expected key is `seed XOR 5A5A5A5A` (`SimEcu::key_for_seed`). The handling follows the state chart
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
`EcuConfig::require_security_access` unset, secured operations do not check the lock state.

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
numbers contradict each other or exceed the flash window, which the ECU itself never produces. `sim-vci` uses this to keep the ECU across worker processes (ADR-241).

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
| `ResponsePending { count, interval_ms }` | The next request the ECU answers gets `count` response pending messages (`7F SID 78`, Annex A.1) before its response: the first when the response was due, the others `interval_ms` apart, and the response `interval_ms` after the last. `exchange()` lists them in `Exchange::pending` with their delays; tS3_Server restarts when the final response goes out. A request without a response leaves the fault armed; with `DropResponse` the messages still go out and only the final response is lost. `count` is capped at `MAX_RESPONSE_PENDING` (1000); zero uses the fault up without sending any |

`ResponsePending` simplifies what a server does around NRC 78 (Annex A.1, clause 7.7.3):

- A request with the suppress bit set gets no chain: a server that sends NRC 78 must then send a
  final response even for such a request, a sequence the simulator cannot produce.
- The chain can precede any final response, also a negative one a server would have given at
  once (NRC 11, 12 or 13), although NRC 78 says the request was accepted.
- During the chain the ECU answers other requests at once, and the request's own effect (an ECU
  reset, for one) applies at the start of the chain, not at its end.

While the ECU is silent (after `PowerLoss` or `drop_at_block`), requests do not reach it and the
armed faults stay armed. `armed_faults()` lists the faults that have not fired yet.
`power_cycles()` counts power cycles (power loss, ECU reset, `reconnect()`); the VCI side uses it
to discard responses it was still delaying when the ECU lost power or reset.
`Fault` serializes in snake case (`"power_loss"`, `{"delay_response": {"ms": 500}}`), the form
`sim-vci`'s control commands take.

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
via `to_bytes()`), or no response.

General behaviour (clause 7.7):

- Checks run in the order of each service's NRC evaluation figure; the first failing check decides
  the NRC.
- The suppressPosRspMsgIndicationBit suppresses positive responses only.
- For functionally addressed requests, NRCs 11, 7F, 12, 7E and 31 are not sent (clause 7.7.1).
- A supported SubFunction is available in every session its service is, so NRC 7E is not produced.
- Response pending (NRC 78) and busy (NRC 21) are not produced: every service completes within the
  request. Response delays belong to the VCI side.

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

Any other SID gets NRC 11. The session timeout (S3) is not simulated: a non-default session lasts
until a session change, an ECU reset or `reconnect()`.

Every DiagnosticSessionControl request relocks security, including a restart of the active
session. A running download is interrupted (see below) by any session other than programming,
and also by a restart of the programming session when `EcuConfig::require_security_access` is
set, since the download was opened behind the security access that the relock takes away.
Without security access, a restart of the programming session leaves the download running.
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
| F189 | `EcuConfig::sw_version` |
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
NRC 37 until `SimEcu::expire_security_delay()` is called (the simulator has no clock). A power
cycle or ECU reset clears the false-attempt counter but not an active delay. With
`EcuConfig::require_security_access` unset, secured operations do not check the lock state.

## Routines and download

- `FF00` eraseMemory erases the whole simulated flash and discards any download progress. Its
  option record is accepted and ignored. Refused with NRC 22 while a download runs.
- `FF01` checkProgrammingDependencies is the integrity check after RequestTransferExit. Its
  routineStatusRecord is one byte: 00 image correct (state becomes verified), 01 image incorrect
  (`EcuConfig::fail_checksum`).
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

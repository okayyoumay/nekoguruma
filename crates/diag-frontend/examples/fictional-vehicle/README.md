# Fictional Vehicle: Nekoguruma Motors "Mikeneko"

The diagnostic communication of a vehicle that does not exist, for simulation tests (design 13.4).
Its manufacturer, model, part numbers, identifiers, data, DTC meanings and seed/key algorithms are
invented; nothing here describes a real vehicle. The choices follow what is common practice and
what the standards leave to the vehicle manufacturer, so that a test against it exercises the same
shapes a real vehicle has: a gateway in front of the ECUs, optional ECUs, variants, ECU-specific
data, routines behind security access and reprogrammable ECUs.

Standards the choices follow (clause numbers refer to these editions; the text paraphrases them):

- ISO 14229-1 (2026): the services (the clauses in section 4's table), negative response codes
  (Annex A.1), data identifier and routine identifier ranges (Annex C.1, Annex F) and DTC status
  bits (Annex D.2)
- ISO 14229-2 (2021): session layer timings (clause 9.2, Table 4; clause 9.5, Table 5)
- ISO 15765-2 (2024): segmentation and flow control over CAN (clause 9, with the block size and
  minimum separation time of 9.6.5.3 and 9.6.5.4) and normal addressing (10.3.2)
- ISO 22901-1 (2008) 7.3.10: the vehicle information model the vehicle definition (section 9)
  follows (ADR-326)

Each ECU is one ECU definition directory in the CSV format of `../../docs/csv-tables.md`.
`identify.star` is one read procedure that every ECU declares the services for. `sim/` holds a
`sim-ecu` configuration (`EcuConfig`) for each ECU of one example vehicle, and
`crates/diag-frontend/tests/fictional_vehicle.rs` converts every directory, checks the vehicle-wide
rules below and runs the identification against `sim-ecu`.

## 1. Vehicle Identification

| Information | Values |
|---|---|
| OEM | `Nekoguruma Motors` |
| Vehicle model | `Mikeneko` |
| Model year | `2027`, `2028` |
| Vehicle type | `MK-M` (manual transmission, no transmission controller), `MK-A` (six-speed automatic) |

The VIN is 17 characters. Positions:

| Positions | Content | Values |
|---|---|---|
| 1-3 | Manufacturer code (invented) | `NGR` |
| 4-5 | Model | `MK` (Mikeneko) |
| 6 | Body | `S` (sedan) |
| 7 | Transmission | `M` manual, `A` automatic |
| 8 | Engine | `1` (1.5 l petrol) |
| 9 | Check character | `0` (not computed) |
| 10 | Model year | `V` 2027, `W` 2028 |
| 11 | Plant | `K` |
| 12-17 | Serial number | digits |

The example vehicle of `sim/` is `NGRMKSA10VK000123`: a 2027 automatic.

## 2. Network

```text
  OBD connector (diagnostic CAN)
            |
     +------+------+
     |  gateway    |  CGW 0x710 / 0x718
     +--+---+---+--+
        |   |   |
        |   |   +-- body CAN ........ BCM 0x740 / 0x748, IPC 0x720 / 0x728
        |   +------ chassis CAN ..... ABS 0x760 / 0x768, EPS 0x730 / 0x738
        +---------- powertrain CAN .. ECM 0x7E0 / 0x7E8, TCM 0x7E1 / 0x7E9 (MK-A only)
```

Every bus runs at 500 kbit/s with 11-bit identifiers and normal addressing (ISO 15765-2 (2024)
10.3.2). The
gateway routes the diagnostic identifiers below between the diagnostic CAN and the internal buses
unchanged, so a tester sees one bus; routing adds no protocol of its own. All ECUs listen on the
functional identifier `0x7DF`. Flow control (ISO 15765-2 (2024) 9.6.5.3, 9.6.5.4): block size 0
(no further flow control frames), STmin 0 (`CP_BlockSize`, `CP_StMin`).

| Role | Directory | Variants | Request | Response | Bus | Optional | Reached through |
|---|---|---|---|---|---|---|---|
| `gateway` | `cgw` | `cgwV1` | `0x710` | `0x718` | diagnostic | no | (direct) |
| `engine` | `ecm` | `ecmV1`, `ecmV2` | `0x7E0` | `0x7E8` | powertrain | no | `gateway` |
| `transmission` | `tcm` | `tcmAt6V1` | `0x7E1` | `0x7E9` | powertrain | yes (MK-A only) | `gateway` |
| `brakes` | `abs` | `absEscV1` | `0x760` | `0x768` | chassis | no | `gateway` |
| `steering` | `eps` | `epsV1` | `0x730` | `0x738` | chassis | no | `gateway` |
| `body` | `bcm` | `bcmStandard`, `bcmPremium` | `0x740` | `0x748` | body | no | `gateway` |
| `cluster` | `ipc` | `ipcV1` | `0x720` | `0x728` | body | no | `gateway` |

The engine and transmission controllers use the identifier pairs that legacy OBD addressing gives
the first two ECUs, so the engine controller is reached on the identifiers `sim-vci` simulates.

Variant identification (design 8.9.1, `variant_patterns.csv`): the engine controller's variants
are told apart by the first two characters of the software version (`1.` and `2.`), the body
controller's by the spare part number (`MK-BCM-0001` standard grade, `MK-BCM-0002` premium grade
with keyless entry), and each single-variant ECU matches on its own spare part number.

## 3. Timing and Sessions

| Parameter | Value | Source |
|---|---|---|
| P2server_max | 50 ms | ISO 14229-2 (2021) 9.2, Table 4, recommended value |
| P2*server_max | 5000 ms | the same |
| S3server | 5000 ms | ISO 14229-2 (2021) 9.5, Table 5 |
| P2* of eraseMemory and checkProgrammingDependencies | 10000 ms | this vehicle |

| Session | Value | ECUs |
|---|---|---|
| default | `0x01` | all |
| programming | `0x02` | ECM, TCM |
| extended | `0x03` | all |

A non-default session falls back to the default session after S3server without a request; the
tester keeps it with TesterPresent (`3E 00`). Any session change relocks security access.

## 4. Services

Every ECU supports these services. A service not listed gets NRC `0x11`.

| SID | Service (ISO 14229-1 (2026)) | Sub-functions / parameters | Session | Security | Behind a locked gateway |
|---|---|---|---|---|---|
| `0x10` | DiagnosticSessionControl (9.2) | `01`, `03`; `02` on ECM and TCM | all | none | allowed |
| `0x11` | ECUReset (9.3) | `01` hard, `02` key off/on, `03` soft | all | none | allowed |
| `0x14` | ClearDiagnosticInformation (11.2) | group `FFFFFF` or one DTC | all | none | allowed |
| `0x19` | ReadDTCInformation (11.3) | `02` by status mask, `0A` supported DTCs | all | none | allowed |
| `0x22` | ReadDataByIdentifier (10.2) | up to 8 DIDs per request | all | none | allowed |
| `0x27` | SecurityAccess (9.4) | `01` requestSeed, `02` sendKey | extended, programming | - | allowed |
| `0x2E` | WriteDataByIdentifier (10.7) | writable DIDs (section 6) | extended | per DID | refused |
| `0x31` | RoutineControl (13.2) | per routine (section 6) | per routine | per routine | refused |
| `0x34` | RequestDownload (14.2) | ECM, TCM | programming | level 1 | refused |
| `0x36` | TransferData (14.4) | ECM, TCM | programming | level 1 | refused |
| `0x37` | RequestTransferExit (14.5) | ECM, TCM | programming | level 1 | refused |
| `0x3E` | TesterPresent (9.7) | `00` | all | none | allowed |

Negative responses (ISO 14229-1 (2026) Annex A.1) every ECU may give: `0x12`, `0x13`, `0x22`,
`0x24`, `0x31`, `0x33` and `0x7F`; for SecurityAccess also `0x35`, `0x36` and `0x37`; behind the
gateway also `0x34` (section 5). Routines add the condition codes listed with them (`0x83` engine
running, `0x88` vehicle moving).

## 5. Gateway Lock

The gateway protects the ECUs behind it against changes. While it is locked, a request to an ECU
behind it for a service marked "refused" in section 4 is answered with NRC `0x34`
(authenticationRequired); reads, DTC reads and clears, session changes, resets, SecurityAccess and
TesterPresent pass. This is the behaviour `sim-ecu` gives with `require_gateway_auth` set.

To unlock it, the tester opens the extended session on the gateway and passes its security access
(level 1, section 7). The gateway stays unlocked while it stays in the extended session: a session
change, an ECU reset of the gateway or S3server without a request locks it again. A tester keeps it
open as an access target (ADR-325), with TesterPresent or by reading DID `F186`, which also shows a
relock. DID `0100` reports the lock state.

## 6. Data, Routines and DTCs

### 6.1 Identification DIDs (every ECU)

| DID | Name | Format |
|---|---|---|
| `F186` | activeSession | 1 byte, the session value of section 3 |
| `F187` | sparePartNumber | 11 ASCII characters, `MK-<ECU>-<4 digits>` |
| `F189` | softwareVersion | 5 ASCII characters, `n.n.n` |
| `F18C` | ecuSerialNumber | 10 ASCII characters |
| `F190` | vin | 17 ASCII characters; writable on ECM and BCM (extended session, level 1) |
| `F197` | systemName | 8 ASCII characters, padded with spaces |

`F187`, `F189` and `F190` are each ECU's identity values (`identity.csv`). ECM and TCM also report
`FD00` flashState (phase 1 byte, block number 4 bytes, bytes received 4 bytes, as `sim-ecu`
reports it), the state check a resumed download starts with (design 8.2.5).

The DIDs below are in the vehicle manufacturer range (ISO 14229-1 (2026) Annex C.1), all big
endian. "Raw" gives the conversion from the transmitted value.

### 6.2 Gateway (CGW)

| DID | Name | Format |
|---|---|---|
| `0100` | gatewayLockState | 1 byte: 0 locked, 1 unlocked |
| `0101` | busStatus | bits 0, 1, 2: powertrain, chassis, body bus off |

| DTC | Meaning |
|---|---|
| `U0100-00` | No messages received from the engine controller |
| `U0101-00` | No messages received from the transmission controller |
| `U0121-00` | No messages received from the brake controller |
| `U0131-00` | No messages received from the steering controller |
| `U0140-00` | No messages received from the body controller |
| `U0155-00` | No messages received from the instrument cluster |
| `U1001-00` | Powertrain CAN bus off |

### 6.3 Engine (ECM)

| DID | Name | Format |
|---|---|---|
| `1000` | engineSpeed | uint16, raw / 4 rpm |
| `1001` | coolantTemperature | uint8, raw - 40 °C |
| `1002` | intakeAirTemperature | uint8, raw - 40 °C |
| `1003` | vehicleSpeed | uint8, km/h |
| `1004` | throttlePosition | uint8, raw × 100 / 255 % |
| `1005` | batteryVoltage | uint16, raw / 100 V |
| `1006` | engineRunning | uint8: 0 no, 1 yes |
| `1010` | fuelRailPressure | uint16, raw × 10 kPa; `ecmV2` only |

| Routine | Name | Sub-functions | Session | Security | Result |
|---|---|---|---|---|---|
| `0201` | throttleAdaptationReset | start | extended | level 1 | 1 byte: 0 completed, else failed; NRC `0x83` while the engine runs |
| `0202` | idleSpeedLearn | start, results | extended | none | results: 0 not started, 1 running, 2 completed, 3 aborted; NRC `0x88` while moving |
| `FF00` | eraseMemory | start | programming | level 1 | |
| `FF01` | checkProgrammingDependencies | start | programming | none | 1 byte: 0 image correct, else incorrect |

| DTC | Meaning |
|---|---|
| `P0117-00` | Coolant temperature sensor signal below its range |
| `P0122-00` | Throttle position sensor signal below its range |
| `P0300-00` | Misfire detected on several cylinders |
| `P0301-00` | Misfire detected on cylinder 1 |
| `P0562-00` | Supply voltage low |
| `U0101-00` | No messages received from the transmission controller |

### 6.4 Transmission (TCM, MK-A only)

| DID | Name | Format |
|---|---|---|
| `2000` | selectorPosition | uint8: 0 P, 1 R, 2 N, 3 D, 4 M |
| `2001` | currentGear | uint8: 0 neutral, 1 to 6, `FF` reverse |
| `2002` | fluidTemperature | uint8, raw - 40 °C |
| `2003` | inputShaftSpeed | uint16, rpm |
| `2004` | outputShaftSpeed | uint16, rpm |

| Routine | Name | Sub-functions | Session | Security | Result |
|---|---|---|---|---|---|
| `0301` | shiftAdaptationReset | start | extended | level 1 | 1 byte: 0 completed, else failed; NRC `0x83` while the engine runs |
| `FF00`, `FF01` | as on the ECM | | | | |

| DTC | Meaning |
|---|---|
| `P0711-00` | Transmission fluid temperature signal implausible |
| `P0715-00` | Input shaft speed signal missing |
| `P0720-00` | Output shaft speed signal missing |
| `U0100-00` | No messages received from the engine controller |

### 6.5 Brakes (ABS)

| DID | Name | Format |
|---|---|---|
| `3000` | wheelSpeeds | 4 × uint16 (front left, front right, rear left, rear right), raw / 100 km/h |
| `3001` | brakePressure | uint16, raw / 10 bar |
| `3002` | yawRate | int16, raw / 100 °/s |
| `3003` | lateralAcceleration | int16, raw / 100 m/s² |
| `3004` | brakePedalPressed | uint8: 0 no, 1 yes |

| Routine | Name | Sub-functions | Session | Security | Parameters and result |
|---|---|---|---|---|---|
| `0401` | pumpMotorTest | start, stop, results | extended | level 1 | start: duration in s (1 byte); results: state as `0202`; NRC `0x88` while moving |
| `0402` | brakeBleeding | start, stop, results | extended | level 1 | start: circuit 1 to 4 (1 byte); results: state as `0202`; NRC `0x88` while moving |

| DTC | Meaning |
|---|---|
| `C1011-00` | Front left wheel speed sensor signal missing |
| `C1012-00` | Front right wheel speed sensor signal missing |
| `C1013-00` | Rear left wheel speed sensor signal missing |
| `C1014-00` | Rear right wheel speed sensor signal missing |
| `C1020-00` | Hydraulic pump motor circuit fault |

### 6.6 Steering (EPS)

| DID | Name | Format |
|---|---|---|
| `4000` | steeringAngle | int16, raw / 10 ° |
| `4001` | steeringTorque | int16, raw / 100 N·m |
| `4002` | motorCurrent | uint8, A |
| `4003` | angleSensorCalibrated | uint8: 0 no, 1 yes |

| Routine | Name | Sub-functions | Session | Security | Result |
|---|---|---|---|---|---|
| `0501` | steeringAngleCalibration | start, results | extended | level 1 | results: state as `0202`; NRC `0x88` while moving |

| DTC | Meaning |
|---|---|
| `C1301-00` | Steering angle sensor not calibrated |
| `C1302-00` | Steering torque signal implausible |
| `C1310-00` | Assist motor overcurrent |

### 6.7 Body (BCM)

| DID | Name | Format |
|---|---|---|
| `5000` | doorStatus | bits 0 to 4: driver, passenger, rear left, rear right door, tailgate open |
| `5001` | exteriorLamps | bits 0 to 4: low beam, high beam, left turn, right turn, position lamps |
| `5002` | ignitionState | uint8: 0 off, 1 accessory, 2 on, 3 cranking |
| `5010` | registeredKeys | uint8; `bcmPremium` only |
| `5100` | bodyConfiguration | 3 × uint8 (0 off, 1 on): automatic headlamps, daytime running lamps, automatic door lock; writable (extended session, level 1) |

| Routine | Name | Sub-functions | Session | Security | Parameters |
|---|---|---|---|---|---|
| `0601` | exteriorLampTest | start, stop | extended | none | start: lamp number (1 byte) |

| DTC | Meaning |
|---|---|
| `B1201-00` | Driver door switch signal implausible |
| `B1310-00` | Left low beam circuit open |
| `B1311-00` | Right low beam circuit open |
| `B1402-00` | Keyless entry antenna circuit fault; `bcmPremium` only |

### 6.8 Instrument Cluster (IPC)

| DID | Name | Format |
|---|---|---|
| `6000` | odometer | uint32, raw / 10 km |
| `6001` | fuelLevel | uint8, raw / 2 % |
| `6002` | serviceDistanceRemaining | int16, raw × 10 km; negative when the service is overdue |
| `6003` | displayLanguage | uint8: 0 English, 1 Japanese; writable (extended session, no security) |

| Routine | Name | Sub-functions | Session | Security |
|---|---|---|---|---|
| `0701` | serviceReminderReset | start | extended | none |

| DTC | Meaning |
|---|---|
| `B1601-00` | Fuel level sensor signal out of range |
| `B1610-00` | Display backlight failure |

### 6.9 DTC Format

DTCs are 3 bytes: the two-byte code (the letter in the top two bits, `P` 00, `C` 01, `B` 10,
`U` 11, then the four digits) and a failure type byte, so `P0301-00` is `030100` and `C1011-00`
is `501100`. The powertrain controllers and the gateway use codes of the generic ranges with
meanings of this vehicle's own; the chassis and body ECUs use the manufacturer ranges (`C1`, `B1`).
The status byte has the eight bits of ISO 14229-1 (2026) Annex D.2; every ECU supports all of them
(availability mask `FF`). No ECU declares snapshot or extended data.

## 7. Security Access

One level per ECU, level 1 (requestSeed `01`, sendKey `02`), in the extended and programming
sessions. The seed is 4 bytes, never zero, and new for every requestSeed; the key is the seed XOR
the ECU's mask, big endian:

| ECU | Mask |
|---|---|
| CGW | `4E47524D` |
| ECM | `5A5A5A5A` |
| TCM | `3C3C3C3C` |
| ABS | `A5A5A5A5` |
| EPS | `0F0F0F0F` |
| BCM | `C3C3C3C3` |
| IPC | `96969696` |

The third false key in a row returns NRC `0x36` and starts a 10 s delay, during which requestSeed
returns NRC `0x37` (ISO 14229-1 (2026) 9.4.1 leaves the length to the manufacturer). A power cycle
or ECU reset clears the counter. The ECM's mask is the one `sim-ecu` uses.

## 8. Reprogramming (ECM, TCM)

| | ECM | TCM |
|---|---|---|
| Flash session id | 1 | 1 |
| Application segment | `00000000`, 256 KiB | `00000000`, 128 KiB |
| Block payload | 256 bytes (maxNumberOfBlockLength 258) | the same |
| RequestDownload formats | dataFormatIdentifier `00`, addressAndLengthFormatIdentifier `44` | the same |
| Steps | programming session, security access, eraseMemory, RequestDownload, TransferData, RequestTransferExit, checkProgrammingDependencies, ECUReset | the same |
| Recovery | session timeout 5000 ms, teardown margin 1000 ms, startup 2000 ms, confirmation window 3000 ms, 3 version reads, NRC `0x22` means no valid application | the same |

Preconditions (design 8.9.1): supply voltage 12.0 to 15.0 V, external supply connected, ignition
on, engine stopped, vehicle stationary. The ECM reads the voltage (DID `1005`, raw 1200 to 1500),
the engine state (`1006`) and the speed (`1003`) itself; the TCM takes them all from the agent's
inputs. The gateway is unlocked first (section 5).

## 9. Vehicle Definition

The vehicle definition of ADR-326 (design 8.8.2), which has no supply format yet:

- Identification values: section 1. Matching rules: VIN positions 4-5 `MK` give the model,
  position 7 the vehicle type (`M` `MK-M`, `A` `MK-A`) and position 10 the model year (`V` 2027,
  `W` 2028), all read from the engine controller's `vin` field (`vin.read`).
- ECU entries: the roles of section 2, each with its one ECU definition, `transmission`
  optional, every entry but `gateway` reached through `gateway`.

## 10. Simulation

`sim/<ecu>.json` configures `sim-ecu` as each ECU of `NGRMKSA10VK000123` (a 2027 MK-A, premium
grade body controller), with a few stored DTCs; the ECUs behind the gateway set
`require_gateway_auth`. `sim-ecu` answers what all of them have in common: the sessions, ECUReset,
the DTC services, `F186`, `F187`, `F189`, `F190`, `FD00`, security access with the ECM's mask,
and the reprogramming of section 8 with the routines `FF00` and `FF01`. It does not answer `F18C`, `F197`, the ECU-specific DIDs and routines
of section 6, or the other ECUs' masks, and `sim-vci` simulates one ECU on the engine
controller's identifiers; `identify.star` and the vehicle test therefore use only the common part.

# ComParam Protocol Support Matrix

This document describes which D-PDU API ComParams (`CP_*`) are accepted by
`j2534-0404-service` for each J2534 protocol, and notes where required (`S`)
params have incomplete hardware support.

## Legend

| Symbol | Meaning |
|---|---|
| ✓ | Supported — forwarded to J2534 hardware via `SET_CONFIG` |
| S | Supported at service level — stored but not forwarded to hardware |
| U | `PDU_PC_UNIQUE_ID` class — rejected by `SetComParam` / `GetComParam` (`INVALID_ARGUMENT`); managed exclusively via `SetUniqueRespIdTable` / `GetUniqueRespIdTable` (ISO 22900-2 §9.3.3.6) |
| ✗ | Not supported — `SetComParam` / `GetComParam` returns `INVALID_ARGUMENT` |
| — | Not applicable to this protocol |

**Support classification (per ISO 22900-3):**
- **(S)** Mandatory for D-PDU API compliance on this protocol
- **(O)** Optional — implementation may choose to support or reject
- **(T)** Tester-side only (ECU does not implement)
- **(E)** ECU-side only (tester does not implement)

---

## Protocol Families

| J2534 Protocol | J2534 ID | D-PDU Bus Types |
|---|---:|---|
| CAN | 0x05 | ISO_11898_2_DWCAN, ISO_11898_3_DWFTCAN, ISO_11992_1_DWCAN, SAE_J1939_11_DWCAN, SAE_J2411_SWCAN |
| ISO15765 | 0x06 | ISO_11898_2_DWCAN (transport layer / UDS over CAN) |
| ISO9141 | 0x03 | ISO_9141_2_UART, SAE_J1708_UART |
| ISO14230 | 0x04 | ISO_14230_1_UART |
| J1850VPW | 0x01 | SAE_J1850_VPW |
| J1850PWM | 0x02 | SAE_J1850_PWM |
| SCI_A_ENGINE | 0x07 | SAE_J2610_UART (engine ECU) |
| SCI_A_TRANS | 0x08 | SAE_J2610_UART (transmission ECU) |
| SCI_B_ENGINE | 0x09 | SAE_J2610_UART (engine ECU, variant B) |
| SCI_B_TRANS | 0x0A | SAE_J2610_UART (transmission ECU, variant B) |

> **Note:** CAN and ISO15765 share the same allowlist. The only difference is
> that ISO15765-specific flow-control params (ISO15765_BS, ISO15765_STMIN,
> BS_TX, STMIN_TX, ISO15765_WFT_MAX) are sent to hardware on ISO15765 channels
> and silently ignored by many adapters on plain CAN channels.

---

## Physical Layer ComParams (BUSTYPE class)

Source: ISO 22900-3, physical layer ComParam table.

| D-PDU ComParam | D-PDU Requirement | CAN / ISO15765 | ISO9141 / ISO14230 | J1850VPW | J1850PWM | SCI |
|---|---|---|---|---|---|---|
| **CP_Baudrate** | S (all) | ✓ `DATA_RATE` | ✓ `DATA_RATE` | ✓ `DATA_RATE` | ✓ `DATA_RATE` | ✓ `DATA_RATE` |
| **CP_BitSamplePoint** | S,T (CAN) | ✓ `BIT_SAMPLE_POINT`¹ | — | — | — | — |
| **CP_BitSamplePoint_Ecu** | S,E (CAN) | S service-only | — | — | — | — |
| CP_CanBaudrateRecord | O,T (CAN) | S Bytefield | — | — | — | — |
| CP_K_L_LineInit | O (KWP) | — | S service-only | — | — | — |
| CP_K_LinePullup | O (KWP) | — | S service-only | — | — | — |
| CP_ListenOnly | O (CAN) | S service-only | — | — | — | — |
| **CP_NetworkLine** | S (J1850PWM) | — | — | — | ✓ `NETWORK_LINE` | — |
| **CP_SamplesPerBit** | S,T (CAN) | S service-only ⚠️ | — | — | — | — |
| **CP_SamplesPerBit_Ecu** | S,E (CAN) | S service-only | — | — | — | — |
| **CP_SyncJumpWidth** | S,T (CAN) | ✓ `SYNC_JUMP_WIDTH`¹ | — | — | — | — |
| **CP_SyncJumpWidth_Ecu** | S,E (CAN) | S service-only | — | — | — | — |
| CP_TerminationType | O,T (CAN subset) | S service-only | — | — | — | — |
| CP_TerminationType_Ecu | O,E (SAE J2411) | S service-only | — | — | — | — |
| **CP_UartConfig** | S (UART) | — | ✓ `DATA_BITS`+`PARITY` ⚠️ | — | — | S service-only ⚠️ |
| CP_Parity | O (UART) | — | ✓ `PARITY` ² | — | — | S service-only |
| CP_CANFDBaudrate | O,T (CAN FD) | S service-only ³ | — | — | — | — |
| CP_CANFDBitSamplePoint | O,T (CAN FD) | S service-only | — | — | — | — |
| CP_CANFDSyncJumpWidth | O,T (CAN FD) | S service-only | — | — | — | — |
| CP_J1850IFRCtrl | O,T (J1850) | — | — | S service-only | S service-only | — |
| CP_Node_Address | O (alias CP_TesterSourceAddress) ⁴ | S service-only | S service-only | S service-only | ✓ `NODE_ADDRESS` | — |

### Physical Layer Notes

**¹ CP_BitSamplePoint / CP_SyncJumpWidth (CAN vs. ISO15765):** CAN and
ISO15765 share the same `SetComParam`/`GetComParam` allowlist (see the note
above the Protocol Families table), so both accept these params without
error. However, the hardware-forwarding gate (`ComParamId::to_j2534_config_id`,
ADR-027/ADR-028) only forwards `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` to
`PassThruIoctl(SET_CONFIG)` when the channel's J2534 protocol ID is literally
`CAN` (0x05). On an `ISO15765` (0x06) channel these two params are accepted
and stored in the Working ComParam set like any other service-level param,
but are silently not forwarded to hardware.

**⚠️ CP_SamplesPerBit (S,T — CAN):** Required by ISO 22900-3 but J2534-1
(DEC2004) has no `CONFIG_SAMPLES_PER_BIT` equivalent. The value is stored at
the service level and returned by `GetComParam`, but is not applied to the
hardware adapter. Tester software relying on this for precise CAN timing must
configure the adapter through vendor-specific means.

**⚠️ CP_UartConfig (S — ISO 9141-2, ISO 14230-1, SAE J2610, SAE J1708):**
The D-PDU standard encodes data bits, parity, and stop bits into a single
`CP_UartConfig` value (`0..=17`; see ADR-071 for the full encoding). J2534-1
uses separate `DATA_BITS` (0x20) and `PARITY` (0x16) params, and has no
stop-bit `SET_CONFIG` param at all (implicitly 1 stop bit), and `DATA_BITS`
only distinguishes 7 vs. 8 data bits (no 9). This service therefore accepts
only the 6 `CP_UartConfig` values fully representable in J2534 v04.04 —
`0,1,2,6,7,8` (7N1/7O1/7E1/8N1/8O1/8E1) — at `SetComParam`; every other value
(2-stop-bit or 9-data-bit encodings, or out of range) is rejected with
`INVALID_ARGUMENT`. At the hardware-forwarding call sites
(`ConnectComLogicalLink` and `CoptUpdateparam`), an accepted `CP_UartConfig`
value is decoded (`expand_uart_config`, ADR-071) into **both** `DATA_BITS`
and `PARITY` `SET_CONFIG` entries — the parity portion is no longer lost. If
the same param collection also has an explicit `CP_Parity` (J2534-specific
alias → `PARITY`) entry, that explicit value takes precedence over the
`CP_UartConfig`-derived parity (deterministic, independent of `HashMap`
iteration order). The hardware-forwarding gate (`ComParamId::to_j2534_config_id`)
still only forwards `DATA_BITS`/`PARITY` for ISO9141/ISO14230 channels; on SCI
channels `CP_UartConfig` is accepted by the allowlist and range-checked the
same way, but stored in the Working ComParam set without being forwarded to
hardware.

**² CP_Parity (O — UART):** a J2534-specific alias for the native `PARITY`
`SET_CONFIG` param, with no distinct D-PDU `CP_*` name of its own (D-PDU
folds parity into `CP_UartConfig`, above). Explicit `CP_Parity` forwards
directly to `PARITY` at the hardware-forwarding call sites and, per the
`CP_UartConfig` note above, wins precedence over any `CP_UartConfig`-derived
parity value in the same param collection. `CP_Parity` is `PDU_PC_BUSTYPE`
class (`comparam_support::BUSTYPE_UNUM32`, ADR-110 amendment) — see the note
below the table for why this changed from an earlier, now-superseded
"conservatively excluded" classification.

**Read-back:** `GetComParam(0x20)` always returns the raw stored
`CP_UartConfig` encoding. `GetComParam(0x16)` (`CP_Parity`) derives its value
from the Working `CP_UartConfig` entry (via the same decode table) when no
explicit `0x16` entry has ever been set, instead of returning a stale `0` —
this is what makes a save/restore roundtrip (`GetComParam(0x20)` +
`GetComParam(0x16)`, later replayed through `SetComParam`) idempotent
(ADR-071). One consequence: if a client sets an explicit `CP_Parity` that
disagrees with `CP_UartConfig`'s implied parity (e.g. `CP_UartConfig = 8`
plus explicit `CP_Parity = 1`), the explicit value wins on hardware (actual
parity is odd), but `GetComParam(0x20)` still reports `8` — the *effective*
parity must be read via `0x16`, not inferred from `0x20`.

**CP_CANFDBaudrate / CP_CANFDBitSamplePoint / CP_CANFDSyncJumpWidth (O,T — CAN FD):**
CAN FD physical layer params, allocated in the service-level extension range
(0x80AA–0x80AC). Default values are pre-populated at `CreateComLogicalLink`
for CAN bus types (ISO_11898_2_DWCAN, SAE_J1939_11_DWCAN).
`CP_CANFDBitSamplePoint`/`CP_CANFDSyncJumpWidth` are stored at the service
level only; J2534-0404 has no `SET_CONFIG` equivalent for either, on any
protocol, so neither is ever forwarded to hardware.

**³ CP_CANFDBaudrate (SAE J2534-2 clause 21 CAN FD, ADR-158):** no longer
purely service-only as of ADR-158 (Phase 3 Stage 3a) — together with
`CP_CANFDTxMaxDataLength` (below), a nonzero value is part of the CAN FD
connect-time protocol-substitution trigger (`TX_DL > 8 || CP_CANFDBaudrate
!= 0`, `rpc_link::J2534Service::apply_fd_mode`), and when FD mode is
triggered its value becomes the actual `SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE)`
argument at `ConnectComLogicalLink` time (falling back to `CP_Baudrate` when
`CP_CANFDBaudrate` is `0`). It is still never forwarded as its own,
separately-named `SET_CONFIG` entry — J2534-0404 has no native `CP_CANFDBaudrate`
config id — but its value now genuinely reaches hardware through this
substitution, unlike `CP_CANFDBitSamplePoint`/`CP_CANFDSyncJumpWidth`, which
remain fully inert.

**CP_J1850IFRCtrl (O,T — J1850):**
J1850 In-Frame Response control (value 0 = disabled, 1 = enabled). Stored at
the service level; not forwarded to hardware. Allocated at 0x80AD. Default value
(1 = enabled) is pre-populated at `CreateComLogicalLink` for J1850 bus types.

**⁴ CP_Node_Address (O — alias CP_TesterSourceAddress):** the tester's own
source address, native J2534 `CONFIG_NODE_ADDRESS` (`NODE_ADDRESS = 4`) — a
J2534 native id, not one of this service's own `0x80xx` extension-range
allocations. Accepted by `SetComParam`/`GetComParam` for CAN, KWP
(ISO9141/ISO14230), and both J1850 variants, but only forwarded to hardware
via `SET_CONFIG` when the connected protocol is J1850PWM specifically
(`ComParamId::to_j2534_config_id`, ADR-028) — stored service-level-only on
every other protocol. `CP_Node_Address` is `PDU_PC_BUSTYPE` class
(`comparam_support::BUSTYPE_UNUM32`), added on the same hardware-effect basis
as `CP_Parity` above (PR #53 review round): the fifth ComParam this repo's
own precedent note below the table now covers.

**`PDU_PC_BUSTYPE` and `temp_param_update` (ADR-067):** the table above is
this service's authoritative `PDU_PC_BUSTYPE` class membership list —
`comparam_support::BUSTYPE_UNUM32`/`BUSTYPE_BYTES` mirror it exactly
(`CP_Baudrate` through `CP_J1850IFRCtrl`, plus `CP_Parity`, `CP_Node_Address`,
and the bytefield `CP_CanBaudrateRecord`). A `StartComPrimitive` call with
`temp_param_update=1` (`CoptSendrecv`/`CoptStartcomm`/`CoptStopcomm`) is
rejected synchronously with `PDU_ERR_TEMPPARAM_NOT_ALLOWED` if Working
differs from Active on any of these — `temp_param_update` may stage a
one-off override of per-COP parameters (addressing, tester-present, timing),
never a bus-physical reconfiguration.

This list is keyed by *physical hardware effect* (does the ComParam reach a
native `PassThruIoctl SET_CONFIG` write another CLL's physical-ComParam lock
or a `temp_param_update` bracket must never touch), not by ISO
`PDU_PC_BUSTYPE` label membership — both `apply_bustype_lock` (ADR-110's
`CoptUpdateparam` lock-conflict resolution) and `strip_bustype_keys`
(ADR-110's temp-bracket guard) reuse this same list for that purpose.
`CP_Parity` (`ComParamId(j2534_0404::PARITY)`) is included on that basis: a
prior revision of this note classified it as "conservatively excluded"
(reasoning that its D-PDU membership was ambiguous, since D-PDU folds parity
into `CP_UartConfig` — see ADR-071) — a Codex review on PR #116 found that
exclusion let a non-owning CLL (or a `temp_param_update` call) smuggle a
physical UART-parity change past both `apply_bustype_lock`'s `hw_set`
exclusion and `strip_bustype_keys`, since `expand_uart_config` always
forwards an explicit `PARITY` entry to hardware unconditionally (and it wins
over any `CP_UartConfig`-derived value). See ADR-110's amendment section and
ADR-067 §E's amendment note for the full rationale, including the side
effect that this section's own `PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard now
also rejects a `temp_param_update` call staging a `CP_Parity` difference
from Active (spec-correct per ISO 22900-2 §9.4.16.2.1 c) NOTE 2, not a
regression).

---

## Transport Layer ComParams

Source: ISO 22900-3, transport layer ComParam table (0x8040–0x8097).

All transport layer params listed here are **service-level** (stored in the
Working/Active ComParam sets; not forwarded to J2534 hardware).

### ISO 15765-2 Frame Timing (0x8040–0x804F)

| D-PDU ComParam | D-PDU Requirement | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|---|
| **CP_Ar** | S,T (ISO15765) | S | — | — | — |
| **CP_Ar_Ecu** | S,E (ISO15765) | S | — | — | — |
| **CP_As** | S,T (ISO15765) | S | — | — | — |
| **CP_As_Ecu** | S,E (ISO15765) | S | — | — | — |
| **CP_Br** | S,T (ISO15765) | S | — | — | — |
| **CP_Br_Ecu** | S,E (ISO15765) | S | — | — | — |
| **CP_Bs** | S,T (ISO15765) | S | — | — | — |
| **CP_Bs_Ecu** | S,E (ISO15765) | S | — | — | — |
| **CP_Cr** | S,T (ISO15765) | S | — | — | — |
| **CP_Cr_Ecu** | S,E (ISO15765) | S | — | — | — |
| **CP_Cs** | S,T (ISO15765) | S | — | — | — |
| **CP_Cs_Ecu** | S,E (ISO15765) | S | — | — | — |
| CP_StMin_Ecu | O,E (ISO15765) | S | — | — | — |
| CP_BlockSize_Ecu | O,E (ISO15765) | S | — | — | — |
| CP_AccessTiming_Ecu (Structfield) | O,E (ISO14230) | — | S¹ | — | — |
| CP_AccessTimingOverride (Structfield) | O,T (ISO14230) | — | S¹ | — | — |

> **¹ CP_AccessTiming_Ecu / CP_AccessTimingOverride (not the wider KWP
> family):** ISO 22900-2's own default-by-protocol tables scope both to
> `ISO_14230_2`/`ISO_14230_4` — the KWP family's other member, ISO9141, has
> no equivalent Access Timing Parameter service (ADR-146). `is_param_allowed`
> gates these two to exactly `ChannelProtocol::kwp_access_timing_applies()`'s
> set (bare `ISO14230`, `ISO_14230_3_ON_ISO_14230_2`, and
> `ISO_15031_5_ON_ISO_14230_4` — ADR-150's round-3 correction; kept in sync
> with `TimingChangeConfig::from_params`'s own KWP gate by construction,
> since `is_param_allowed` now calls the same predicate rather than a
> separately hardcoded check that could drift out of sync again), narrower
> than this file's general "KWP" column. As of ADR-146 these are no longer
> purely passive storage: `CP_AccessTiming_Ecu` is auto-populated from an
> observed ISO 14230-2 SID 0x83/0xC3 exchange, and `CP_AccessTimingOverride`
> redirects the derived `CP_P2Min`/`CP_P2Max`/`CP_P2Star`/`CP_P3Min`/`CP_P4Min`
> values for a TPI=2 (read active) exchange — see `CP_ModifyTiming`'s own
> note in the Timing (0x8010–0x8019) table below.

> **Note (CP_Bs / CP_Bs_Ecu):** These are the ISO 15765-2 N_Bs **flow-control
> wait timers** (ms), distinct from `CP_BlockSize` / `ISO15765_BS` which is
> the block-size frame count. Both are implemented.

### General Transport Timing

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| CP_ExtendedTiming (Structfield) | — | S¹ | — | — |
| CP_J1939AddrClaimTimeout | S | — | — | — |
| CP_RepeatReqCountTrans | S | S | — | — |

¹ Same `kwp_access_timing_applies()` scoping and rationale as
`CP_AccessTiming_Ecu`/`CP_AccessTimingOverride` above (ADR-146/150) — not
the wider KWP family, and not the wider ISO14230 hardware-channel family
either as of ADR-150's round-3 correction. Moved here from the
CAN/ISO15765 column, which was backwards (ISO 22900-2's own
default-by-protocol table lists `ISO_14230_2`/`ISO_14230_4` only). This
mechanical allow-list fix is unrelated to `CP_ModifyTiming`'s new live
behavior below — `CP_ExtendedTiming` itself gained no new logic; it merely
shares `is_iso14230_only_structfield_param`'s gate with the two ComParams
that do.

### CAN Addressing (0x8060–0x806F) — CAN / ISO15765 only

All nine CAN PhysReq / RespUSDT / RespUUDT params are `PDU_PC_UNIQUE_ID` class:
they identify a specific ECU's request and response CAN IDs within the UniqueRespIdTable.

| D-PDU ComParam | Status |
|---|---|
| **CP_CanPhysReqExtAddr** | U |
| **CP_CanPhysReqFormat** | U |
| **CP_CanPhysReqId** | U |
| **CP_CanRespUSDTExtAddr** | U |
| **CP_CanRespUSDTFormat** | U |
| **CP_CanRespUSDTId** | U |
| **CP_CanRespUUDTExtAddr** | U |
| **CP_CanRespUUDTFormat** | U |
| **CP_CanRespUUDTId** | U |
| CP_CanFuncReqExtAddr | S |
| CP_CanFuncReqFormat | S |
| CP_CanFuncReqId | S |
| CP_CanDataSizeOffset | S |
| CP_CanFillerByte | S |
| CP_CanFillerByteHandling | S |
| CP_CanFirstConsecutiveFrameValue | S |

> **UNIQUE_ID class params:** CP_CanPhysReqExtAddr through CP_CanRespUUDTId are
> `PDU_PC_UNIQUE_ID` class and are managed exclusively via `SetUniqueRespIdTable` /
> `GetUniqueRespIdTable` (ISO 22900-2 §9.3.3.6). `SetComParam` / `GetComParam`
> reject them with `INVALID_ARGUMENT` regardless of protocol.

### ECU Addressing / COM (0x8070–0x808C)

Params that identify a specific ECU's response source or format are `PDU_PC_UNIQUE_ID` class
and are rejected by `SetComParam` / `GetComParam` (see the "U" legend entry above);
they are set only via `SetUniqueRespIdTable`.

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 |
|---|---|---|---|
| **CP_EcuRespSourceAddress** | U | U | U ³ |
| CP_FuncReqFormatPriorityType | S | S | S |
| CP_FuncReqTargetAddr | S | S | S |
| **CP_FuncRespFormatPriorityType** | U | U | U ³ |
| **CP_FuncRespTargetAddr** | U | U | U ³ |
| CP_PhysReqFormatPriorityType | S | S | S |
| CP_PhysReqTargetAddr | S | S | S |
| **CP_PhysRespFormatPriorityType** | U | U | U ³ |
| CP_RequestAddrMode | S | S | S |
| CP_HeaderFormatJ1850 | — | — | S |
| CP_HeaderFormatKW | — | S | — |
| CP_EnableConcatenation | — | S | S |
| CP_FillerByte | S | S | S |
| CP_FillerByteHandling | S | S | S |
| CP_FillerByteLength | S | S | — |
| CP_5BaudAddressFunc | — | S | — |
| CP_5BaudAddressPhys | — | S | — |

> **³ CP_EcuRespSourceAddress / CP_FuncRespFormatPriorityType / CP_FuncRespTargetAddr /
> CP_PhysRespFormatPriorityType (J1850, ADR-202):** reclassified `PDU_PC_UNIQUE_ID` for J1850
> VPW/PWM per ISO 22900-2:2022 Table B.11 — previously unsupported for J1850 (`SetUniqueRespIdTable`
> rejected every entry keying one of these), mirroring the same 4 params KWP already lists.

> **TX addressing consumption:** `CP_RequestAddrMode`, `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr`,
> `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`, and (CAN/ISO15765 only)
> `CP_CanPhysReqId`/`CP_CanFuncReqId` (+ their `Format`/`ExtAddr` pairs) are read
> by `tx_header::build_tx_message` to construct the outgoing message's ID/header
> prefix — `CP_RequestAddrMode` selects physical (default) or functional
> addressing, which in turn selects which of the Phys/Func pair is used
> (ADR-050, ADR-054). "S" here still means "not forwarded to J2534 hardware via
> `SET_CONFIG`" — this consumption is service-level message construction, not a
> hardware CONFIG write.
>
> **RawMode bypass (ADR-196 Phase 1, extended to hardware K-line by ADR-198
> Phase 2, then to SAE J1850/J1939 by ADR-200 Phase 3):** on a CLL created
> with `CLL_CREATE_FLAG_RAW_MODE` set (base CAN, hardware ISO15765,
> hardware K-line/ISO9141/ISO14230, SAE J1850 (VPW/PWM), or SAE J1939 — the
> protocol allowlist), `tx_header::build_tx_message` skips this whole
> addressing-consumption/header-construction step entirely — none of the
> ComParams this note describes are read for that CLL's `CoptSendrecv`/
> tester-present sends, since the client's own `cop_data`/
> `CP_TesterPresentMsg` already IS the literal `PassThruMessage.Data`
> (including, for K-line with `CLL_CREATE_FLAG_CHECKSUM_MODE` OFF, the
> client's own checksum byte — this service never computes or verifies a
> K-line checksum itself in any RawMode/ChecksumMode combination). For SAE
> J1939 specifically, this also means `CP_MessagePriority`/
> `CP_J1939PDUFormat`/`CP_J1939PDUSpecific`/`CP_J1939TargetAddress` (this
> file's own J1939 rows below) are not read for header construction either
> — the client's raw `cop_data` already encodes the equivalent bytes
> directly, and `tx_header::raw_j1939_tx_message` derives only the native
> destination-address byte from those raw bytes, never from a ComParam
> (ADR-200). This does not change these ComParams' `SetComParam`/
> `GetComParam` acceptance or class — only whether `build_tx_message` ever
> reads them for that CLL.

> **CP_EnableConcatenation protocol scope:** ISO 22900-2:2022 Table B.11 lists
> this ComParam as applicable to the KWP family (ISO 9141-2, ISO 14230-2/-4)
> and SAE J1850 VPW/PWM only, not CAN/ISO15765 or SCI. The 2009(E) edition
> disagreed on this scoping — see ADR-148 for the
> reasoning behind following the 2022 edition.

> **CP_EnableConcatenation same-key collision:** enabling this ComParam tells
> this service to merge any same-`(unique_resp_identifier, source_id, SID)`
> frame into the open buffer as a continuation, with no way to distinguish a
> genuine continuation from an independent exchange that happens to reuse the
> same key (an ECU retransmission, a repeat/cyclic exchange, or two distinct
> requests answered with the same SID) — this is the ComParam's spec-defined
> behavior (ISO 22900-2:2022 Table B.20), not an implementation gap; see
> ADR-148 Amendment 12. If an attached ECU is known to exhibit this pattern,
> disable `CP_EnableConcatenation` on that link.
| CP_SendRemoteFrame | S | — | — |
| CP_TPConnectionManagement | S | — | — |
| CP_MessagePriority | S | S | S |
| CP_MidReqId | S | S | S |
| **CP_MidRespId** | U | U | U |
| CP_J1939AddressNegotiationRule | S | — | — |
| CP_J1939DataPage | S | — | — |
| CP_J1939MaxPacketTx | S | — | — |
| CP_J1939PDUFormat | S | — | — |
| CP_J1939PDUSpecific | S | — | — |
| **CP_J1939SourceAddress** | U | — | — |
| CP_J1939TargetAddress | S | — | — |

> **CP_J1939SourceAddress protocol scope (ADR-184):** this row's `U` lands
> in the `CAN / ISO15765` column purely because this section's table has no
> dedicated J1939 column (the same layout convention every other
> `CP_J1939*` row above uses) -- the classification itself is J1939-only.
> `unique_id_params` (`comparam_support.rs`) never returns this ComParam for
> `CAN`/`ISO15765`; it is `PDU_PC_UNIQUE_ID` class for `J1939_PS` alone,
> reachable there via `SetUniqueRespIdTable` for per-ECU response routing by
> source address. See `docs/j2534-0404-architecture.md`'s own dedicated
> J1939 `PDU_PC_UNIQUE_ID` row for the accurate per-protocol picture.

### INIT (0x8090–0x8097)

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| **CP_InitializationSettings** | S | S² | — | — |
| CP_SCITransmitMode | — | — | — | S¹ |
| CP_J1939PreferredAddress (Bytefield) | S | — | — | — |
| CP_J1939PreferredAddress_Ecu | S | — | — | — |
| CP_J1939Name (Bytefield) | S | — | — | — |
| CP_J1939Name_Ecu (Bytefield) | S | — | — | — |
| **CP_J1939SourceName (Bytefield)** | S³ | — | — | — |
| CP_J1939TargetName (Bytefield) | S | — | — | — |

> **³ CP_J1939SourceName (ADR-184):** not `PDU_PC_UNIQUE_ID` class, despite
> sitting alongside `CP_J1939SourceAddress` (marked `U` above, under the
> `CAN / ISO15765` column for this section's own pre-existing J1939 layout
> convention) -- `SetUniqueRespIdTable` matching by NAME would need
> NAME<->SA network-management bookkeeping this codebase's J1939 claim
> subsystem does not build; deliberately left a plain settable ComParam
> pending that follow-up (the Prioritized Backlog).
>
> **² CP_InitializationSettings drives `CoptStartcomm`'s K-line init
> sequence selection (ADR-074):** on ISO9141/ISO14230, the value chooses
> which J2534 init call the service issues at `CoptStartcomm` time — `1` =
> 5-baud init (`IOCTL_FIVE_BAUD_INIT`), `2` = fast-init (`IOCTL_FAST_INIT`),
> `3` = no init sequence at all (the call is skipped entirely and the COP
> proceeds as though init had succeeded). `SetComParam` rejects any other
> value with `INVALID_ARGUMENT`. If the ComParam is absent from the bound
> set entirely (a link created with no matching K-line preset), the service
> falls back to the pre-ADR-074 heuristic: ISO9141, or a single-byte
> `init_data`, selects 5-baud init; everything else selects fast-init.

### Protocol-Layer Service Params (0x80AE–0x80C3)

These params are stored at the service level (not forwarded to J2534 hardware).
Default values are pre-populated at `CreateComLogicalLink` time per the protocol
name (`protocol_default_params` in `comparam_defaults.rs`).

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI | J1708 |
|---|---|---|---|---|---|
| CP_DisableTransportChecksumCheck | — | S | — | — | — |
| CP_TestMode | — | S | — | — | — |
| CP_EnableInitSeqRepetition | — | S | — | — | — |
| CP_TesterPresentImmed | S² | S² | S² | S² | — |
| CP_NumHeaderBytesStartCommKW | — | S | — | — | — |
| CP_P3Func (CAN context) | S | — | — | — | — |
| CP_P3Phys (CAN context) | S | — | — | — | — |
| CP_5BaudCommBaudrateOverride | — | S | — | — | — |
| CP_5BaudInitBaudrate | — | S | — | — | — |
| CP_ISOKeybyteCount | — | S | — | — | — |
| CP_IgnoreChecksum | — | S | — | — | S |
| CP_CanMixedFormat | S | — | — | — | — |
| CP_CANFDTxMaxDataLength | S³ | — | — | — | — |
| CP_EscapeSequenceHandling | S | — | — | — | — |
| CP_MaxDataLength_Ecu | S | — | — | — | — |
| CP_MaxCTSReq | — | — | — | — | S |
| CP_CollisionTestMode | — | — | — | — | S |
| CP_T3Max (TP transport context) | S | — | — | — | — |
| CP_T4Max (TP transport context) | S | — | — | — | — |
| CP_T5Max (TP transport context) | S | — | — | — | — |
| CP_SCISetProgVoltage | — | — | — | S¹ | — |
| CP_SCIEcuSimulator | — | — | — | S | — |

> **¹ CP_SCITransmitMode / CP_SCISetProgVoltage drive `PASSTHRU_MSG.TxFlags`
> (not `SET_CONFIG`):** unlike a plain "S" entry, these two are not merely
> stored — every transmitted message on an SCI-protocol CLL has
> `TX_FLAG_SCI_MODE`/`TX_FLAG_SCI_TX_VOLTAGE` computed from them
> (`CP_SCITransmitMode != 0` → `SCI_MODE`; `CP_SCISetProgVoltage` any value
> other than its seeded `0xFFFF_FFFF` "no override" default → `SCI_TX_VOLTAGE`,
> which applies 20V to the SCI bus after transmit). They are still never
> pushed via `PassThruIoctl SET_CONFIG` — the "not forwarded to hardware" in
> the legend refers specifically to that mechanism, not to `TxFlags` (ADR-062).

> **² CP_TesterPresentImmed — this "S" is currently aspirational, not
> live (known bug, tracked in the backlog):** this
> row and `comparam_defaults.rs`'s ~10 preset seeds for the param disagree
> with `comparam_support.rs`, where it is absent from every one of
> `is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/`is_j1850vpw_param`/
> `is_sci_param` — so `GetComParam`/`SetComParam` actually reject it with
> `PDU_ERR_COMPARAM_NOT_SUPPORTED` on every one of these protocols today,
> and nothing in this crate reads the seeded value at runtime either. See
> the backlog entry for the two resolution paths (wire up the allowlist, or
> remove the dead param).

> **³ CP_CANFDTxMaxDataLength (SAE J2534-2 clause 21 CAN FD, ADR-158):** no
> longer purely "stored, not forwarded" as of ADR-158 (Phase 3 Stage 3a) —
> was previously seeded in `comparam_defaults.rs` but absent from
> `comparam_support::is_can_param`'s allowlist entirely, so `SetComParam`
> rejected it outright before this fix. Now allow-listed and range-validated
> at `SetComParam` (accepted values: `{0, 8, 12, 16, 20, 24, 32, 48, 64}`,
> Table 90/97), and — together with `CP_CANFDBaudrate` (see that param's own
> ³ note in the Physical Layer table above) — drives the CAN FD
> connect-time protocol-substitution trigger (`TX_DL > 8 || CP_CANFDBaudrate
> != 0`). It is still never forwarded as its own `SET_CONFIG` entry — J2534-0404
> has no native config id for it — but it is no longer inert: its value
> decides whether the connect substitutes `PROTOCOL_FD_CAN_PS` for plain
> `CAN` at all.

> **CP_P3Func / CP_P3Phys disambiguation:** For CAN-family protocols these are
> stored as service-level params (0x80B3–0x80B4) and not forwarded to the
> J2534 CAN adapter — but, unlike most "S" entries in this document, they do
> drive real service-level behavior: before a functionally- or physically-
> addressed `CoptSendrecv` transmits, the poll task enforces a minimum gap
> since the shared physical channel's last send of the same addressing, when
> either that previous send or (functional only) this upcoming send required
> no response (`NumReceiveCycles == 0`) — see ADR-060. For KWP protocols the
> identical D-PDU param names both resolve to the single hardware timer
> `j2534_0404::P3_MIN`, forwarded via `SET_CONFIG` (there is no separate KWP
> forwarding path for `P3_MAX`: per `ComParamId::to_j2534_config_id`,
> `P3_MAX` has no J2534 `SET_CONFIG`/`GET_CONFIG` support for any protocol
> and is never forwarded to hardware). The distinction is handled by which
> `comparam_support` allowlist the channel's protocol falls into. Until
> ADR-056, `is_can_param` omitted both service-level IDs and `names.rs`
> unconditionally resolved both shortnames to native `P3_MIN`, making the
> CAN-context values unreachable via `SetComParam`/`GetComParam` despite
> being pre-seeded in some CAN presets; both are now fixed.

> **CP_T3Max / CP_T4Max / CP_T5Max disambiguation:** For J1939/J1587 TP
> protocols these are service-level TP timers (0x80BF–0x80C1). For SCI
> protocols the same D-PDU names map to the hardware `j2534_0404::T3_MAX`
> / `T4_MAX` / `T5_MAX` registers forwarded via `SET_CONFIG`.

### Analog Inputs / UART Echo Byte Remaining Parameters (0x80D3–0x80E3, ADR-216)

Seventeen project-minted, no-ISO-22900-2-source ComParams, each allowed on
exactly one standalone protocol family (this table's CAN/KWP/J1850/SCI/J1708
columns above do not apply — see `CP_AnalogSampleRate`'s own `0x80C4` entry,
Context, for why this file's column set predates the standalone-protocol
phases and has not yet been generally reconciled with them; this section is
scoped to the 17 ComParams this ADR adds, not a full backfill).

`✓` here means the same as this document's own legend: forwarded to J2534
hardware via `SET_CONFIG` (all 14 writable entries below reach hardware,
unlike the `0x80AE–0x80C3` service-level-only table above). The three
`ANALOG_IN_x` read-only entries use `RO` (a symbol this file has not needed
before this ADR): populated only by a connect-time `GET_CONFIG` readback,
never accepted by `SetComParam` at all.

| D-PDU ComParam | Id | Protocol | Native config id | Support | Notes |
|---|---|---|---|---|---|
| CP_AnalogActiveChannels | 0x80D3 | ANALOG_IN_x | CONFIG_ACTIVE_CHANNELS | ✓ | also connect-time readback (fresh + join) |
| CP_AnalogSamplesPerReading | 0x80D4 | ANALOG_IN_x | CONFIG_SAMPLES_PER_READING | ✓ | range `>= 1`; no `CoptUpdateparam` re-stage; also connect-time sync from applied (fresh + join) |
| CP_AnalogReadingsPerMsg | 0x80D5 | ANALOG_IN_x | CONFIG_READINGS_PER_MSG | ✓ | range `1..=0x408`; no `CoptUpdateparam` re-stage; also connect-time sync from applied (fresh + join) |
| CP_AnalogAveragingMethod | 0x80D6 | ANALOG_IN_x | CONFIG_AVERAGING_METHOD | ✓ | no service-side range check; also connect-time readback (fresh + join) |
| CP_AnalogSampleResolution | 0x80D7 | ANALOG_IN_x | CONFIG_SAMPLE_RESOLUTION | RO | connect-time readback only |
| CP_AnalogInputRangeLow | 0x80D8 | ANALOG_IN_x | CONFIG_INPUT_RANGE_LOW | RO | connect-time readback; reports `Snum32` |
| CP_AnalogInputRangeHigh | 0x80D9 | ANALOG_IN_x | CONFIG_INPUT_RANGE_HIGH | RO | connect-time readback; reports `Snum32` |
| CP_UebT0Min | 0x80DA | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T0_MIN | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT1Max | 0x80DB | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T1_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT2Max | 0x80DC | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T2_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT3Max | 0x80DD | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T3_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT4Min | 0x80DE | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T4_MIN | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT5Max | 0x80DF | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T5_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT6Max | 0x80E0 | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T6_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT7Min | 0x80E1 | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T7_MIN | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT7Max | 0x80E2 | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T7_MAX | ✓ | native ms, 0x0000–0xFFFF |
| CP_UebT9Min | 0x80E3 | UART_ECHO_BYTE_PS/_CHx | CONFIG_UEB_T9_MIN | ✓ | native ms, 0x0000–0xFFFF |

The three `ANALOG_IN_x` read-only entries reject any `SetComParam` attempt
outright with `PDU_ERR_COMPARAM_NOT_SUPPORTED` (SAE J2534-2 clause
10.3.3.2.6-.2.8's own native `ERR_INVALID_IOCTL_PARAM_ID` rejection for the
same operation) — they, along with `CP_AnalogActiveChannels`/
`CP_AnalogAveragingMethod`, read `0` before the owning CLL's first
successful `ConnectComLogicalLink` (populated only by a one-time
connect-time `GET_CONFIG` readback, never a live `GetComParam`-time hardware
read). `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg` are instead
synced at that same connect-time point from the channel's own
already-recorded applied value (not a fresh `GET_CONFIG`), so they read
their seeded Working default (`1`) before first connect rather than `0`. All
four writable entries — `CP_AnalogActiveChannels`/`CP_AnalogSamplesPerReading`/
`CP_AnalogReadingsPerMsg`/`CP_AnalogAveragingMethod` — receive this
connect-time sync/readback on BOTH a fresh physical-channel connect and a
CLL joining an already-open `ANALOG_IN_x` channel, silently overwriting any
value a joiner staged of its own before connecting (SAE J2534-2 clause
10.3.3.2's own subsystem-wide, not per-CLL, framing of this configuration);
`CP_AnalogSampleRate` is the deliberate exception to this "live value wins"
pattern, staying mandatory-to-stage with an outright rejection on a
mismatched re-stage rather than a silent overwrite (ADR-178). The ten
`CP_UebT*` entries store native whole-millisecond values, not the 1 µs ISO
22900-2 `CP_*` timing convention every other timing ComParam in this table
uses. See ADR-216 and `docs/rpc-api-guide.md`'s Analog Inputs / UART Echo
Byte sections for the full design.

---

## Application Layer ComParams (0x8001–0x8037)

Source: ISO 22900-3, application layer ComParam table.

### Tester Present (0x8001–0x8009)

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| CP_TesterPresentMessage (Bytefield) | S | S | S | S |
| CP_TesterPresentTime | S | S | S | S |
| CP_TesterPresentAddrMode | S¹ | S¹ | S¹ | — |
| CP_TesterPresentExpPosResp | S | S | — | — |
| CP_TesterPresentExpNegResp | S | S | — | — |
| CP_TesterPresentHandling | S | S | S | — |
| CP_TesterPresentReqRsp | S | S | S | — |
| CP_TesterPresentSendType | S | S | S | — |
| CP_TesterPresentTime_Ecu | S | S | — | — |

¹ Drives tester-present's own addressing (`AddrModeSource::TesterPresent` in
`tx_header.rs`), independent of `CP_RequestAddrMode` — see ADR-138.

### Timing (0x8010–0x8019)

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| CP_CyclicRespTimeout | S | S | S | S |
| CP_P2Min | S | S | — | — |
| CP_P2Max | S | S | — | — |
| CP_P2Star | S | S | — | — |
| CP_P2Star_Ecu | S | S | — | — |
| CP_P2Max_Ecu | S | S | — | — |
| CP_ModifyTiming | S² | S¹ | — | — |
| CP_SessionTiming_Ecu (Structfield) | S² | S | — | — |
| CP_SessionTimingOverride (Structfield) | S² | S | — | — |
| CP_CanTransmissionTime | S | — | — | — |
| CP_MessageIndicationRate | S | S | S | S |
| CP_ChangeSpeedTxDelay | S³ | S | — | — |

CP_P2Min / CP_P2Max map to the native J2534 IDs (`P2_MIN` 0x0A / `P2_MAX` 0x09,
see `comparam-mapping.md`) rather than the 0x8010-range, but are never forwarded
to `SET_CONFIG` on any protocol (`comparam_id.rs`). The Active `CP_P2Max` is the
`CoptSendrecv` response window (µs; default 50 ms — ADR-053).

`CP_P2Star` (us, like `CP_P2Min`/`CP_P2Max`) is the RC78 (response-pending)
auto-handler's per-occurrence reload window: each 0x78 reloads the response
deadline to `now + CP_P2Star` (ISO 14229-2 §7.3 P2*client semantics,
ADR-102 — supersedes ADR-057's anchor-once treatment of this value).
`CP_RC78CompletionTimeout` below is a separate, independent total-duration
ceiling. `CP_P2Star_Ecu`/`CP_P2Max_Ecu` remain stored-only; no per-ECU
override mechanism exists.

¹ `CP_ModifyTiming` on KWP is no longer purely passive storage: when
enabled on a protocol where `ChannelProtocol::kwp_access_timing_applies()`
holds -- bare ISO14230, `ISO_14230_3_ON_ISO_14230_2`, and (a deliberate
inclusion, not a "not ISO9141" shorthand -- see ADR-150's round-3
correction) `ISO_15031_5_ON_ISO_14230_4` specifically, not the wider
ISO14230 hardware-channel family -- the service passively observes a
client-issued ISO 14230-2 Access Timing Parameter exchange (SID 0x83/0xC3)
and derives `CP_P2Min`/`CP_P2Max`/`CP_P2Star`/`CP_P3Min`/`CP_P4Min` from it,
pushing the result to hardware and recording the ECU's reported values into
`CP_AccessTiming_Ecu` (ADR-146). The service never originates the 0x83
request itself.

² `CP_ModifyTiming` on UDS is likewise no longer passive storage: when
enabled on a protocol where `ChannelProtocol::uds_session_timing_applies()`
holds -- bare ISO15765 and the specific ISO 14229-3/ISO 15765-3
service-level protocols (ADR-150's round-3 correction enumerates the exact
set; several other protocols that merely share the ISO15765 HARDWARE
channel, e.g. `SAE_J2190_ON_ISO_15765_2`/OBD/`ISO_14230_3_ON_ISO_15765_2`,
are deliberately excluded since their diagnostic services layer isn't UDS)
-- the service passively observes a client-issued ISO 15765-3/14229-3
DiagnosticSessionControl exchange (SID 0x10/0x50) and derives
`CP_P2Max`/`CP_P2Star` from the response's `P2Server_max`/`P2*Server_max`
(plus `CP_CanTransmissionTime`), pushing the result to hardware and
recording the ECU's reported values into `CP_SessionTiming_Ecu`,
optionally redirected by `CP_SessionTimingOverride` (ADR-150). The service
never originates the 0x10 request itself. This is the KWP mechanism's
CAN/UDS sibling ADR-146's own text once described as "remains storage-only
and out of scope" -- that scope note is now
superseded by ADR-150.

³ `CP_ChangeSpeedTxDelay` has no documented native mapping on any protocol
(ISO 22900-2:2009 Annex A.1.2 Table A.3 omits it, and SAE J2534-2 clause 9's
own GMW3110 speed-change handshake is adapter-driven with no configurable
"delay" `SET_CONFIG` param) -- stored-only, same as before ADR-164/Phase 4.
See the COM section below for the three `CP_ChangeSpeed*` params that DO
gain a native translation on an SW link.

### Error Handling (0x8020–0x802A)

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| CP_RC21CompletionTimeout | S | S | S | — |
| CP_RC21Handling | S | S | S | — |
| CP_RC21RequestTime | S | S | S | — |
| CP_RC23CompletionTimeout | S | S | S | — |
| CP_RC23Handling | S | S | S | — |
| CP_RC23RequestTime | S | S | S | — |
| CP_RC78CompletionTimeout | S | S | S | — |
| CP_RC78Handling | S | S | S | — |
| CP_RCByteOffset | S | S | S | — |
| CP_RepeatReqCountApp | S | S | S | — |
| CP_SuspendQueueOnError | S | S | S | — |

`CP_RC78CompletionTimeout` is reinstated (ADR-102) as an independent
total-duration anti-stall ceiling for the RC78 sequence, anchored once at
the first 0x78 — `CP_P2Star` (above) drives the per-occurrence reload
instead. `CP_RC21CompletionTimeout`/`CP_RC23CompletionTimeout` are unchanged:
each is still a fixed ceiling anchored at its code's first occurrence, not a
per-occurrence reset (ADR-057). All `CompletionTimeout`/`RequestTime`
ComParams here, and `CP_P2Star`, are microsecond-denominated.

`CP_SuspendQueueOnError` (ADR-147) is implemented: when Active `= 1`, this
CLL's TX queue suspends (transmitting items only — a queued `CoptUpdateparam`
that will itself clear the suspension, per its own queued param snapshot,
still runs) the moment either a COP's response times out, or a `0x7F`-led
negative response echoing that COP's own request SID arrives whose NRC the
RC engine was never asked to auto-handle (either an NRC outside the
RC21/RC23/RC78 set, or one of those codes with its matching
`CP_RCxxHandling` currently `0`); a bound `0x7F`-led response that can't be
confirmed as this COP's own negative response (wrong SID, truncated, or an
`rc_byte_offset < 2` protocol) declines to affect the queue either way. Held items drain, in FIFO order, on the
first of: a later positive response on the CLL, an explicit
`PDU_IOCTL_RESUME_TX_QUEUE`, or a `CoptUpdateparam` promotion landing Active
`CP_SuspendQueueOnError` back to `0`. `PDU_IOCTL_CLEAR_TX_QUEUE` cancels held
items but does not itself resume dispatch (pre-existing, unchanged
behavior). `CP_RepeatReqCountApp` remains stored-only with no runtime
effect, unlike `CP_SuspendQueueOnError` above.

`CP_RCByteOffset` (`RcHandlingConfig::rc_byte_offset`) stays raw-relative
even on a RawMode=ON CLL (ADR-196 Phase 1's base CAN/hardware ISO15765,
extended to hardware K-line by ADR-198 Phase 2, then to SAE J1850/J1939 by
ADR-200 Phase 3): it is a client-configured ComParam that locates the
RC/NRC value byte within `cop_data` exactly as the client wrote it, and
RawMode does not change that meaning. What changes is the internal
`0x7F <SID>` shape anchors this RC engine and `CP_SuspendQueueOnError`'s own
negative-response detector use to confirm the frame is genuinely a negative
response for this COP's SID before trusting `rc_byte_offset` at all — those
anchors are re-based by the frame's `raw_prefix`/`tx_prefix` (the RawMode
CAN-ID prefix width for CAN/ISO15765, the client's own KWP header width for
K-line, a fixed 3 bytes for SAE J1850, or the client's own 4-byte CAN-ID
prefix for SAE J1939 — the LATTER computed before `raw_j1939_tx_message`'s
DA-byte insertion on TX / after `raw_j1939_rx_drop_destination_address`'s
DA-byte removal on RX, i.e. always the client-visible 4-byte shape, never
the native 5-byte one) so they still land on the true `0x7F`/SID-echo bytes
rather than inside the prefix (ADR-196 Decision item 3b; the KWP
header-width derivation is ADR-198's own addition, reusing
`events::kwp_header_and_payload_len`; the J1850/J1939 arms are ADR-200's own
addition).

### COM (0x8030–0x8037)

| D-PDU ComParam | CAN / ISO15765 | KWP | J1850 | SCI |
|---|---|---|---|---|
| CP_ChangeSpeedCtrl | ✓⁴ | — | — | — |
| CP_ChangeSpeedMessage (Bytefield) | S⁵ | — | — | — |
| CP_ChangeSpeedRate | ✓⁴ | — | — | — |
| CP_ChangeSpeedResCtrl | ✓⁴ | — | — | — |
| CP_EnablePerformanceTest | S | S | S | S |
| CP_StartMsgIndEnable | S | S | S | S |
| CP_TransmitIndEnable | S | S | S | S |
| CP_SwCAN_HighVoltage | S⁶ | — | — | — |

⁴ ADR-164/Phase 4: `CP_ChangeSpeedCtrl`/`Rate`/`ResCtrl` translate to native
`CONFIG_SW_CAN_SPEEDCHANGE_ENABLE`/`CONFIG_SW_CAN_HS_DATA_RATE`/
`CONFIG_SW_CAN_RES_SWITCH` (`comparam_id.rs::to_j2534_config_id`) only when
the link's hardware id is `SW_CAN_PS`/`SW_ISO15765_PS`
(`resources::is_sw_protocol_id`); on a non-SW CAN link the value is
accepted and stored only, same as `CP_ChangeSpeedMessage`/`TxDelay` below.
All five `CP_ChangeSpeed*` params are allowlisted family-wide on CAN
(`comparam_support::is_can_param`), not just on an SW link, mirroring the
existing `CP_P3Func`/`CP_P3Phys` "seeded but previously unreachable"
precedent.

⁵ `CP_ChangeSpeedMessage` (`CP_ChangeSpeedMsg`) has no documented native
mapping on any link — ISO 22900-2:2009 Annex A.1.2 Table A.3 omits it, and
clause 9's own GMW3110 speed-change handshake is a fixed adapter-detected
byte sequence, not a configurable message pattern — accepted-but-unmapped,
same as `CP_ChangeSpeedTxDelay` above.

⁶ `CP_SwCan_HighVoltage` is not forwarded via `SET_CONFIG` at all (ADR-164
Decision 2): it is wired as a genuine per-message `TxFlags` bit
(`TX_FLAG_SW_CAN_HV_TX`, via `ComParamSet::sw_can_tx_flags`, mirroring
ADR-062's `sci_tx_flags` precedent), ORed in only when the link's hardware
id is `SW_CAN_PS`/`SW_ISO15765_PS` — so it cannot bleed onto a dual-wire CAN
link that happens to have this family-wide-allowlisted ComParam set to a
nonzero value it never consults.

---

## Summary of Required (S) ComParams with Incomplete Support

The following params are marked **mandatory (S)** by ISO 22900-3 but cannot be
fully implemented due to J2534-1 (DEC2004) limitations:

### CP_SamplesPerBit (S,T — CAN family)
- **What it does:** Specifies the number of CAN bit samples (typically 1 or 3)
  used by the tester's CAN controller per bit period.
- **Limitation:** J2534-1 has no `CONFIG_SAMPLES_PER_BIT` parameter. The value
  is stored in the Working/Active ComParam set and returned by `GetComParam`,
  but is **not applied to the hardware adapter**.
- **Impact:** CAN timing behavior at the physical layer cannot be controlled
  through this param. Most CAN adapters default to 1 sample per bit.
- **Workaround:** Use vendor-specific J2534 IOCTL commands if the adapter
  supports sampling configuration. ADR-219 implements a raw
  `IoctlID`/`ConfigParameterID` passthrough for the tool-manufacturer-
  specific range (`0x10000`-`0xFFFFFFFF`) that lets a client reach such a
  vendor-specific IOCTL through this service's own `IoCtl` RPC (or, if the
  adapter's vendor extension is a `SET_CONFIG`/`GET_CONFIG` parameter rather
  than an IOCTL, through `SetComParam`/`GetComParam`), rather than requiring
  out-of-band access to the adapter, on every protocol (not just an
  unrecognized one — see "Protocol-Based Rejection" above). See
  `docs/rpc-api-guide.md`'s "IO Control" section for the wire-level payload
  format.

### CP_UartConfig (S — ISO 9141-2, ISO 14230-1, SAE J2610, SAE J1708)
- **What it does:** In ISO 22900-3, encodes UART data bits, parity, and stop
  bits in a single parameter value (`0..=17`).
- **Limitation:** J2534-1 uses separate `DATA_BITS` (0x20) and `PARITY` (0x16)
  registers and has no stop-bit `SET_CONFIG` param at all (implicitly 1 stop
  bit); `DATA_BITS` also only distinguishes 7 vs. 8 data bits (no 9). This
  service decodes `CP_UartConfig` into both `DATA_BITS` and `PARITY`
  (ADR-071) for the 6 values that are fully representable —
  `0,1,2,6,7,8` (7N1/7O1/7E1/8N1/8O1/8E1) — and rejects every other value at
  `SetComParam` with `INVALID_ARGUMENT` rather than silently truncating it.
- **Impact:** Callers needing 2 stop bits or 9 data bits cannot represent that
  configuration through this service at all (`SetComParam` fails); the 6
  representable configurations no longer require setting `CP_Parity`
  separately, though an explicit `CP_Parity` (J2534-specific alias) still
  takes precedence over the `CP_UartConfig`-derived parity if both are set.

---

## Behavioral Notes

### J2534 Hardware vs. Service-Level Enforcement

- **Hardware params** (✓): Forwarded to the adapter via `PassThruIoctl SET_CONFIG`
  during `ConnectComLogicalLink` and `CoptUpdateparam`. The J2534 adapter may
  return `ERR_NOT_SUPPORTED` for params it cannot handle, which surfaces as an
  internal error at connect time.

- **Service params** (S): Stored in the Working/Active ComParam sets in memory.
  Not forwarded to the adapter. The D-PDU API application layer uses these to
  control service-level behavior (tester-present timing, addressing, error retry
  logic, etc.).

### Protocol-Based Rejection

`SetComParam` and `GetComParam` validate `com_param_id` against the channel's
`protocol_id` before processing. Sending an unsupported param returns
`INVALID_ARGUMENT`. Params not listed in this document for a given protocol
are rejected.

**Exception:** Unknown protocol IDs (not one of the standard J2534 IDs) bypass
validation and accept all params, to preserve compatibility with custom or
future protocols.

**Vendor (`>= 0x10000`):** a `com_param_id` in SAE J2534-1's tool-manufacturer-
reserved range (`0x10000`-`0xFFFFFFFF`, ADR-219) is admitted unconditionally
on every protocol — checked ahead of every protocol-specific rule above,
including the closed-list protocols (Honda DIAG-H, Analog Inputs,
Ethernet_NDIS, UART Echo Byte) that otherwise reject even `CP_Baudrate`. No
SAE/ISO parameter table can rule on a tool-manufacturer id's protocol
applicability, so this service defers entirely to the connecting vendor
DLL's own `SET_CONFIG`/`GET_CONFIG` result (ADR-219's allowlist-gate
amendment).

### Bus Type and Protocol Name Default ComParams

When `CreateComLogicalLink` is called, the Working ComParam set is pre-populated
in two stages:

1. **Bus-type defaults** (`bustype_default_params` keyed on `RscData.BusTypeName`).
2. **Protocol-name defaults** (`protocol_default_params` keyed on `RscData.ProtocolName`,
   applied on top via `HashMap::extend`; protocol values override bus-type values
   where they overlap).

This reduces the number of `SetComParam` calls needed before `ConnectComLogicalLink`.
`SetComParam` still overrides any default as usual. CLLs created with `ResourceId`
or `ResourceName` (no bus type or protocol name) start with empty Working params
when no resource-table row matches. A raw-id connect (no `ResourceId`/`ResourceName`
row match either) is more nuanced: naming one of six standalone `_PS`-only hardware
protocol ids directly -- SWCAN, FT-CAN, UART Echo Byte, Honda DIAG-H, SAE J1708, or
SAE J1939 -- receives that protocol's own real bustype defaults via
`resources::bustype_default_name_for_hw_protocol_id`, even with no caller-supplied
`bus_type_name` at all (and even overriding a mismatched one, since these six
protocols' identity is authoritative on this route). A raw-id connect naming a
base/generic `_PS`/`_CHx` id instead (e.g. plain `CAN_PS`, `ISO15765_PS`) still
starts with empty Working params -- this remains a real, separate,
deliberately-out-of-scope gap; see the corresponding P3 backlog entry in
the Prioritized Backlog.

Bus-type defaults:

| Bus Type | CP_Baudrate | Notable defaults |
|---|---|---|
| ISO_11898_2_DWCAN | 500 000 | BitSamplePoint=80, SyncJumpWidth=15, ListenOnly=0 |
| ISO_11898_3_DWFTCAN | 125 000 | BitSamplePoint=80, SyncJumpWidth=15 |
| SAE_J1939_11_DWCAN | 250 000 | BitSamplePoint=80, SyncJumpWidth=15 |
| SAE_J2411_SWCAN | 33 333 | applied to a raw `SW_CAN_PS`/`SW_ISO15765_PS` connect today (see above); SWCAN itself is not natively supported by J2534-0404, but this default is genuinely used, not merely stored |
| ISO_14230_1_UART | 10 400 | |
| ISO_9141_2_UART | 10 400 | |
| SAE_J1708_UART | 9 600 | |
| SAE_J2610_UART | 7 812 | |
| SAE_J1850_PWM | 41 600 | NetworkLine=0, J1850IFRCtrl=1 |
| SAE_J1850_VPW | 10 400 | J1850IFRCtrl=1 |

Recognized D-PDU protocol names (21 total, case-insensitive):

| Protocol Name | Transport |
|---|---|
| ISO_11898_RAW | Raw CAN |
| ISO_14230_3_on_ISO_14230_2 | KWP2000 over K-Line |
| ISO_14230_3_on_ISO_15765_2 | KWP2000 over CAN |
| ISO_15031_5_on_ISO_14230_4 | OBD-II over K-Line |
| ISO_15031_5_on_ISO_15765_4 | OBD-II over CAN |
| ISO_15031_5_on_ISO_9141_2 | OBD-II over ISO 9141-2 |
| ISO_15031_5_on_SAE_J1850_PWM | OBD-II over J1850 PWM |
| ISO_15031_5_on_SAE_J1850_VPW | OBD-II over J1850 VPW |
| ISO_15765_3_on_ISO_15765_2 | UDS over CAN |
| ISO_OBD_on_ISO_15765_4 | OBD over CAN |
| ISO_OBD_on_K_Line | OBD over K-Line |
| ISO_OBD_on_SAE_J1850 | OBD over J1850 |
| ISO_OBD_on_SAE_J1939_73 | OBD over J1939 |
| SAE_J1587_on_SAE_J1708 | J1587 over J1708 |
| SAE_J1939_73_on_SAE_J1939_21 | J1939 over CAN |
| SAE_J2190_on_ISO_14230_2 | GM KWP over K-Line |
| SAE_J2190_on_ISO_15765_2 | GM KWP over CAN |
| SAE_J2190_on_ISO_9141_2 | GM KWP over ISO 9141-2 |
| SAE_J2190_on_SAE_J1850_PWM | GM KWP over J1850 PWM |
| SAE_J2190_on_SAE_J1850_VPW | GM KWP over J1850 VPW |
| SAE_J2610_on_SAE_J2610_SCI | SCI (Chrysler) |

### UniqueRespIdTable

ComParams with class `PDU_PC_UNIQUE_ID` (CP_CanPhysReqId, CP_CanRespUSDTId,
etc. — marked `U` throughout this document) are managed exclusively via
`SetUniqueRespIdTable` / `GetUniqueRespIdTable` for multi-ECU scenarios, per
ISO 22900-2 §9.3.3.6. `SetComParam` / `GetComParam` reject them with
`INVALID_ARGUMENT` (see ADR-042); they cannot be read or written through the
regular ComParam RPCs at all. The boundary is enforced in the other direction
too: `SetUniqueRespIdTable` rejects any entry containing a param that is
**not** `PDU_PC_UNIQUE_ID` class for the CLL's protocol (e.g. `CP_Baudrate`)
with `INVALID_ARGUMENT` — the table is not a second, unchecked path for
regular ComParams (ADR-042).

# ISO 22900-2 CP_* to J2534-1 (DEC2004) Mapping

This document captures how `j2534-0404-service` maps ISO 22900-2 ComParam shortnames (`CP_*`) to SAE J2534-1 (DEC2004) `GET_CONFIG` / `SET_CONFIG` parameter IDs.

## Scope

- Source references:
  - ISO 22900-2:2022-06 — no converted/markdown copy is currently available in this
    workspace; only the 2009(E) edition is (see below). Values here were mapped against a
    locally-held 2022 PDF at the time this document was written, not reproducible from
    what's checked out today — re-verify against the 2009(E) text (or the actual 2022 PDF,
    if obtained) before trusting an edition-sensitive value.
  - `J2534_1_200412.md` (J2534-1 DEC2004) — sibling `vehicle-comm-specs` repository, at
    `j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru Vehicle Programming.md`
- This mapping is intentionally conservative and focuses on ComParams that are used by the service.
- If a ComParam has no direct J2534-1 DEC2004 equivalent, it is treated as unsupported in this service layer.

## Implemented Mapping

| ISO ComParam | J2534 item | J2534 Param ID |
|---|---:|---:|
| `CP_Baudrate` | `DATA_RATE` (also the write-back target for the baud rate calculated by a successful 5-baud init, see below) | `0x01` |
| `CP_Loopback` | `LOOPBACK` | `0x03` |
| `CP_Node_Address` (name variant) | `NODE_ADDRESS` | `0x04` |
| `CP_NetworkLine` | `NETWORK_LINE` | `0x05` |
| `CP_P1Min` | `P1_MIN` | `0x06` |
| `CP_P1Max` | `P1_MAX` (unit conversion, see below) | `0x07` |
| `CP_P2Min` | `P2_MIN` | `0x08` |
| `CP_P2Max` | `P2_MAX` | `0x09` |
| `CP_P3Min` | `P3_MIN` (unit conversion, see below) | `0x0A` |
| `CP_P3Phys` | `P3_MIN` (approximation, unit conversion) | `0x0A` |
| `CP_P3Func` | `P3_MIN` (approximation, unit conversion) | `0x0A` |
| `CP_P3Max_Ecu` | `P3_MAX` (closest) | `0x0B` |
| `CP_P4Min` | `P4_MIN` (unit conversion, see below) | `0x0C` |
| `CP_P4Max` | `P4_MAX` | `0x0D` |
| `CP_W1Max` | `W1` (closest, unit conversion) | `0x0E` |
| `CP_W2Max` | `W2` (closest, unit conversion) | `0x0F` |
| `CP_W3Max` | `W3` (closest, unit conversion) | `0x10` |
| `CP_W4Min` | `W4` (closest, unit conversion) | `0x11` |
| `CP_W5Max` (closest; also receives the `CP_TIdle` (`0x13`) fan-out on ISO14230 links, see below — explicit `CP_W5Max` wins) | `W5` (unit conversion) | `0x12` |
| `CP_TIdle` (fans out to `W5`/`W0`, see below) | `TIDLE` (unit conversion, see below) | `0x13` |
| `CP_TInil` | `TINIL` (closest, unit conversion) | `0x14` |
| `CP_TWup` | `TWUP` (closest, unit conversion) | `0x15` |
| `CP_Parity` | `PARITY` | `0x16` |
| `CP_BitSamplePoint` | `BIT_SAMPLE_POINT` | `0x17` |
| `CP_SyncJumpWidth` | `SYNC_JUMP_WIDTH` | `0x18` |
| `CP_W0Max` (closest; also receives the `CP_TIdle` (`0x13`) fan-out on ISO9141 links, see below — explicit `CP_W0Max` wins) | `W0` (unit conversion) | `0x19` |
| `CP_T1Max` | `T1_MAX` (unit conversion, see below) | `0x1A` |
| `CP_T2Max` | `T2_MAX` (unit conversion, see below) | `0x1B` |
| `CP_T4Max` | `T4_MAX` (unit conversion, see below) | `0x1C` |
| `CP_T5Max` | `T5_MAX` (unit conversion, see below) | `0x1D` |
| `CP_BlockSize` | `ISO15765_BS` | `0x1E` |
| `CP_StMin` | `ISO15765_STMIN` | `0x1F` |
| `CP_UartConfig` | `DATA_BITS` + `PARITY` (decoded, see below) | `0x20` |
| `CP_5BaudMode` | `FIVE_BAUD_MOD` | `0x21` |
| `CP_5BaudAddressFunc` | *(none — service-level only, no J2534 SET_CONFIG equivalent; consumed at `StartComPrimitive` call time, see below)* | `0x807F` |
| `CP_5BaudAddressPhys` | *(none — service-level only, no J2534 SET_CONFIG equivalent; consumed at `StartComPrimitive` call time, see below)* | `0x8080` |
| `CP_W1Min` | *(none — J2534-1 Figure 30's `W1` register is MAX-only; store-only, never forwarded, see ADR-181)* | `0x80C5` |
| `CP_W2Min` | *(none — J2534-1 Figure 30's `W2` register is MAX-only; store-only, never forwarded, see ADR-181)* | `0x80C6` |
| `CP_W3Min` | *(none — J2534-1 Figure 30's `W3` register is MAX-only; store-only, never forwarded, see ADR-181)* | `0x80C7` |
| `CP_W4Max` | *(none — J2534-1 Figure 30's `W4` register is MIN-only; store-only, never forwarded, see ADR-181)* | `0x80C8` |
| `CP_BlockSizeOverride` / `CP_BsTx` | `BS_TX` | `0x22` |
| `CP_StMinOverride` / `CP_StMinTx` | `STMIN_TX` (unit conversion, see below) | `0x23` |
| `CP_T3Max` | `T3_MAX` (unit conversion, see below) | `0x24` |
| `CP_CanMaxNumWaitFrames` | `ISO15765_WFT_MAX` | `0x25` |

`CP_W0Max` (`0x19`) and `CP_W5Max` (`0x12`) are the primary ISO ComParam
names for the native `W0`/`W5` `SET_CONFIG` IDs — not aliases. `CP_TIdle`
(`0x13`) is a distinct ComParam ID with no native `SET_CONFIG` ID of its
own beyond `TIDLE` itself; because J2534 v04.04 splits the K-line idle
timer into `W0` (ISO9141) and `W5` (ISO14230), `SetComParam(CP_TIdle, ...)`
additionally *derives* a `W0`/`W5` entry at hardware-forwarding time (see
below) so a client does not have to set both `CP_TIdle` and `CP_W0Max`/
`CP_W5Max` separately. An explicit `SetComParam(CP_W0Max, ...)` /
`SetComParam(CP_W5Max, ...)` always takes precedence over the
`CP_TIdle`-derived value for the same native ID.

## Runtime Behavior in j2534-0404-service

- `SetComParam` writes the value into the CLL's in-memory Working `ComParamSet`; it does not call J2534 `SET_CONFIG` directly. Working values are pushed to the hardware (as `SET_CONFIG`) at `ConnectComLogicalLink` (Offline → Online) and at `CoptUpdateparam` (Working → Active), via `ComParamId::to_j2534_config_id`, which also gates which protocol each param ID is actually forwarded for (see ADR-027, ADR-028, and `comparam-protocol-support.md`).
- `GetComParam` reads back from the same in-memory Working `ComParamSet`; it does not issue a live J2534 `GET_CONFIG` query — it returns the value the client set (ComParam units), never the converted value actually written to hardware. The one exception is `CP_Parity` (`0x16`): see the `CP_UartConfig` note below.
- `CP_StMinOverride`/`STMIN_TX` additionally goes through a value conversion, `to_j2534_config_value`, at the same two hardware-forwarding call sites: `CP_StMinOverride` is a microsecond value (1 us resolution, `0xFFFFFFFF` = "use the vehicle-reported value"), while native `STMIN_TX` uses the ISO 15765-2 STmin byte encoding (`0x00`-`0x7F` = 0-127 ms, `0xF1`-`0xF9` = 100-900 us, `0xFFFF` = "use the vehicle-reported value"). Values are rounded to the nearest representable `STMIN_TX` step; see ADR-037.
- The K-line/KWP timing family (`P1_MAX`, `P3_MIN`, `P4_MIN`, `W0`-`W5`, `TIDLE`, `TINIL`, `TWUP`) and the SCI hardware timers (`T1_MAX`-`T5_MAX`) go through the same `to_j2534_config_value` hook: their ComParam-space values are microseconds (1 us resolution, matching `comparam_defaults.rs`'s seeded presets), while native J2534 `SET_CONFIG` uses 0.5 ms resolution for `P1_MAX`/`P3_MIN`/`P4_MIN` and 1 ms resolution for the rest. `us_to_half_ms`/`us_to_ms` round to the nearest native step (same saturating-add round-to-nearest policy as `stmin_override_to_stmin_tx`); see ADR-072.
- `CP_TIdle` (`0x13`) additionally fans out to `W0` (ISO9141 links) or `W5` (ISO14230 links) at the hardware-forwarding call sites, via `expand_tidle`: a single `SetComParam(CP_TIdle, ...)` reaches both native `TIDLE` and whichever of `W0`/`W5` the connected protocol uses, unit-converted the same way. If the same param collection also contains an explicit `W0`/`W5` entry (`CP_W0Max`/`CP_W5Max`, set directly), that explicit value wins over the `CP_TIdle`-derived one — the same explicit-entry-wins precedence `expand_uart_config` uses for `CP_Parity` (ADR-071, ADR-072).
- `CP_UartConfig` (ComParam ID `0x20`, aliased as `data_bits` in `GetObjectId`/`SetComParam`-by-name for historical reasons — see `comparam-protocol-support.md`) stores the ISO 22900-2 combined data-bits/parity/stop-bits encoding (`0..=17`) in the Working set. Only the 6 values fully representable in J2534 v04.04 are accepted (`0,1,2,6,7,8` = 7N1/7O1/7E1/8N1/8O1/8E1); `SetComParam` rejects every other value with `INVALID_ARGUMENT`. At the hardware-forwarding call sites, `expand_uart_config` decodes an accepted value into **two** native `SET_CONFIG` entries, `DATA_BITS` and `PARITY`, unless the same param collection also contains an explicit `CP_Parity` (`0x16`) entry, in which case the explicit value wins over the `CP_UartConfig`-derived parity (ADR-071). `GetComParam(0x16)` mirrors this on the read side: when Working has no explicit `0x16` entry, it derives the value from the Working `CP_UartConfig` (`0x20`) entry instead of returning a stale `0`, so a `GetComParam`+`SetComParam` save/restore roundtrip is idempotent (ADR-071).
- `CP_5BaudAddressFunc` (`0x807F`) / `CP_5BaudAddressPhys` (`0x8080`) have no native `SET_CONFIG` equivalent — they are read directly by `j2534-0404-service`, not forwarded to hardware. For a K-line `COPT_STARTCOMM` whose bound `CP_InitializationSettings` is explicitly `1` (the spec-mandated 5-baud contract, ADR-076), the service resolves the 5-baud wakeup address from whichever of the two applies (`CP_5BaudAddressFunc` when `CP_RequestAddrMode == 2`, else `CP_5BaudAddressPhys`) at `StartComPrimitive` call time, defaulting to `0x33`/`0x01` respectively when absent. Both are range-checked to `0x00`-`0xFF` (a single address byte) at `SetComParam` time and again at call time. After a successful 5-baud init (this path or the legacy `cop_data[0]` heuristic), the service reads back the adapter's negotiated baud rate via `GET_CONFIG(DATA_RATE)` and writes it into `CP_Baudrate` in both the Working and Active sets (ADR-076) — buffer-only, per ADR-011's exclusion of `DATA_RATE` from hardware-forwarding pushes.
- `GetObjectId(OBJT_COMPARAM, shortname)` resolves supported `CP_*` names to J2534 parameter IDs.
- `GetObjectId(OBJT_COMPARAM, shortname)` accepts both ISO-style names (for example `CP_StMin`) and common J2534-style names (for example `ISO15765_STMIN`).

## Notes

- J2534-1 DEC2004 has fewer protocol parameters than ISO 22900-2; many CPs (TesterPresent, RCxx handling, DoIP, J1939/ISOBUS families) do not have a direct equivalent.
- Mappings marked as approximation represent nearest behavior, not exact semantic parity.
- `CP_StMinOverride` (ADR-037) and the K-line/KWP/SCI timing family listed above (ADR-072) are the mappings in this table with an actual unit conversion (not just an ID mapping) applied to their value.
- `CP_W1Min`/`CP_W2Min`/`CP_W3Min`/`CP_W4Max` each got their own project-chosen ComParam ID (`0x80C5`-`0x80C8`) rather than sharing their sibling's native `W1`-`W4` ID, since J2534-1 Figure 30 gives each of those native registers only one direction (max-only for `W1`-`W3`, min-only for `W4`) — sharing an ID with the mapped sibling silently collided the two names' stored values (ADR-181).

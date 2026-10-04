# ADR-054: Build Functional (Broadcast) Addressing from CP_RequestAddrMode

**Date:** 2026-07-04
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/tx_header.rs`,
             `j2534-0404-service/src/service/rpc_primitive.rs`

## Context

ADR-050 introduced `tx_header::build_tx_message` to construct the outgoing
`PassThruMessage.Data` ID/header prefix from ComParams and the CLL's
UniqueRespIdTable, but explicitly scoped out functional/broadcast
addressing: `CP_RequestAddrMode`'s value was stored (as `PARAM_REQUEST_ADDR_MODE`)
but never consulted, so every request was built as physical addressing
regardless of the ComParam's value.

This was not just a missing edge case: several of this service's own
default ComParam sets (`comparam_defaults.rs`) seed `CP_RequestAddrMode = 2`
(functional) as the default addressing mode — notably `iso15765_4_common`
(OBD-II on ISO15765-4, used by `iso_15031_5_on_iso_15765_4` /
`iso_obd_on_iso_15765_4`) and every J1850 OBD preset built via
`j1850_common`. Those presets already carry the functional-addressing
ComParams they need (`CP_CanFuncReqId`/`Format`/`ExtAddr`,
`CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`) — they were simply
unused, so a client relying on these defaults for a standard OBD functional
request got a physical-addressed message instead.

## Decision

`tx_header::resolve_can_addressing` and the KWP/J1850 header builders in
`tx_header.rs` now branch on `CP_RequestAddrMode` (`PARAM_REQUEST_ADDR_MODE`):
value `2` selects functional addressing, any other value (including the
standard physical value `1`) or an absent ComParam defaults to physical —
matching every seeded default and preserving prior behavior for links that
never touch this ComParam.

Functional addressing is a COM-class (not `PDU_PC_UNIQUE_ID`) ComParam
(ADR-042) — it names one shared broadcast address for the whole logical
link, not a per-ECU one — so the functional branch reads directly from the
Active ComParam set and never consults the UniqueRespIdTable:

- **CAN / ISO15765**: `CP_CanFuncReqId`/`CP_CanFuncReqFormat`/`CP_CanFuncReqExtAddr`
  replace `CP_CanPhysReqId`/`CP_CanPhysReqFormat`/`CP_CanPhysReqExtAddr`.
  There is no defined per-ECU FlowControl pairing for a broadcast request,
  so `CanAddressing::fc_can_id` is `None` and `fc_rx_addressing` is `Normal`
  in this branch (unchanged from the "no UniqueRespIdTable entry" fallback
  behavior already used elsewhere).
- **ISO9141 / ISO14230 (KWP)**: `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`
  replace `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr`; the
  UniqueRespIdTable's `CP_EcuRespSourceAddress` override (used only for
  physical addressing) is skipped.
- **J1850PWM / J1850VPW**: same substitution as KWP. (Client-facing
  `SetComParam`/`GetComParam` for `CP_FuncReqFormatPriorityType`/
  `CP_FuncReqTargetAddr` remains rejected on J1850 per the existing
  allowlist in `comparam_support.rs` — this is unrelated to and unchanged
  by this decision; the service still reads its own seeded defaults for
  these ComParams when building a functional J1850 message.)
- **SCI**: unaffected — it has no addressing ComParam at all in this
  service's mapping and `build_tx_message` already treats it as a pure
  payload passthrough.

`CanAddressing`'s field previously named `phys_req_id` is renamed to
`req_id`, since it now holds either the physical or the functional CAN ID
depending on the resolved addressing mode.

`resolve_can_addressing`'s signature changes from `(entries)` to
`(active, entries)` so it can read `CP_RequestAddrMode` and the functional
ComParams; both call sites in `rpc_primitive.rs`
(`CoptSendrecv`'s `tx_addressing`/size-range lookup and `CoptStartcomm`'s
software-ISO-TP tester-present framing) are updated to pass `link.active`.

## Consequences

- A CAN/ISO15765 CLL with no `CP_CanFuncReqId` set (unusual — every
  seeded bustype default in `comparam_defaults.rs` sets one) now rejects
  `CoptSendrecv`/`CoptStartcomm` under functional addressing with a
  dedicated `invalid_argument` message, mirroring the existing
  `CP_CanPhysReqId`-missing error for physical addressing.
- `resolve_can_addressing` is a breaking signature change within this
  crate (`pub(super)`, not part of the external gRPC contract); both
  internal call sites were updated in the same commit.
- No change to which ComParams `SetComParam`/`GetComParam` accepts per
  protocol (`comparam_support.rs` is untouched) — this decision only
  changes which already-stored ComParam values `tx_header.rs` reads to
  build the outgoing message.
- `CP_FuncRespTargetAddr` / `CP_FuncRespFormatPriorityType` (response-side,
  `PDU_PC_UNIQUE_ID` class per ADR-042) remain out of scope, as does
  interpreting multiple functional responses from different ECUs — this
  decision is about constructing the *outgoing* header only, same scope
  boundary ADR-050 drew for physical addressing.

# ADR-040: `CP_Can*Format` (Table B.13) Decoding for Point-to-Point `FLOW_CONTROL_FILTER`

**Date:** 2026-07-02
**Status:** Superseded by ADR-041 (for the UUDT address only — the `CP_Can*Format` bit
decoding and the USDT filter behavior described here still stand)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`CanIdFormat`, `CanAddress`,
             `can_filter_message`, `install_point_to_point_fc_filter`,
             `install_point_to_point_fc_filters`), `j2534-0404-mock`

## Context

ADR-039 built point-to-point `FLOW_CONTROL_FILTER`s from `CP_CanRespUSDTId` /
`CP_CanPhysReqId` alone, explicitly scoping out extended ISO-TP addressing:
"no other part of this service currently interprets `CP_CanPhysReqFormat` /
`CP_CanRespUSDTFormat`, so there is nothing to derive the extension byte
from consistently."

ISO 22900-2 Table B.13 defines the `CP_Can*Format` UNUM32 bitfield (shared by
`CP_CanPhysReqFormat`, `CP_CanFuncReqFormat`, `CP_CanRespUSDTFormat`,
`CP_CanRespUUDTFormat`):

| Bits | Name | Meaning |
|---|---|---|
| 5,4 | Padding Overwrite | TX-only; applies to `CP_CanPhysReqFormat`/`CP_CanFuncReqFormat` outgoing-frame padding. Not related to filters. |
| 3 | Addressing Scheme | 0 = normal (4-byte CAN Id), 1 = extended (CAN Id + a target-address-extension byte in Data[4], i.e. N_AE/N_TA). |
| 2 | Data Transfer Handling | 0 = UUDT (no PCI bytes), 1 = USDT (segmented). |
| 1 | CAN Id Size | 0 = 11-bit, 1 = 29-bit. |
| 0 | Flow Control | 0 = no flow-control frames used for this address, 1 = flow-control frames used. |

This closes the gap ADR-039 left open: bits 3, 1, and 0 map directly onto
`PassThruStartMsgFilter`'s `pMaskMsg`/`pPatternMsg`/`pFlowControlMsg` byte
length, `TxFlags`, and whether the filter should be installed at all.
`CP_Can{PhysReq,RespUSDT}ExtAddr` (already stored per-entry in
`UniqueRespIdTable`, see `comparam_support::CAN_UNIQUE_ID_UNUM32`) supplies
the extension-address byte value itself.

## Decision

> **Superseded by ADR-041:** `install_point_to_point_fc_filters` now also
> builds a second point-to-point `FLOW_CONTROL_FILTER` from
> `CP_CanRespUUDTId`/`CP_CanRespUUDTFormat`/`CP_CanRespUUDTExtAddr`, so the
> "never from the UUDT variant" statement below (and its bit-2 rationale) no
> longer holds. `CP_CanRespUUDTFormat` bit 0 (Flow Control) is deliberately
> *not* read for that filter, unlike the USDT gate described here. The rest
> of this ADR — the `CanIdFormat` bit decoding (bits 5,4 / 3 / 1) and the
> USDT filter's behavior — is unchanged.

`CanIdFormat::from_raw` decodes bits 3, 1, and 0 (bits 5,4 and bit 2 are not
read — see below). `install_point_to_point_fc_filters` now, per
`UniqueRespIdTable` entry:

- Reads `CP_CanRespUSDTFormat`; if bit 0 (Flow Control) is clear, the entry is
  skipped entirely — no `FLOW_CONTROL_FILTER` is installed for it. If the
  format param is absent, it defaults to `{ extended_addressing: false,
  extended_can_id: false, flow_control_enabled: true }` (ADR-039's original,
  format-unaware behavior), so entries that only set `CP_CanRespUSDTId` /
  `CP_CanPhysReqId` keep working unchanged.
- Otherwise builds `pMaskMsg`/`pPatternMsg` from `CP_CanRespUSDTId` +
  `CP_CanRespUSDTFormat` + `CP_CanRespUSDTExtAddr`, and `pFlowControlMsg`
  from `CP_CanPhysReqId` + `CP_CanPhysReqFormat` + `CP_CanPhysReqExtAddr`,
  independently: bit 3 set → 5 bytes (CAN Id + extension byte, mask byte
  `$FF`) and `TxFlags |= ISO15765_ADDR_TYPE`; bit 1 set → `TxFlags |=
  CAN_29BIT_ID`. `pMaskMsg` and `pPatternMsg` always share `CP_CanRespUSDTFormat`
  (their `DataSize` must match); `pFlowControlMsg` is independent since it is
  a separate message struct and may use a different addressing scheme than
  the response side.
- Bits 5,4 (Padding Overwrite) are not read: they govern the tester's own
  outgoing-frame padding, not filter construction, and this service has no
  other CAN-ID-format-aware TX path yet to apply them to.
- Bit 2 (Data Transfer Handling) is not read: this service only ever builds
  point-to-point filters from `CP_CanRespUSDTId`/`CP_CanRespUSDTFormat` (USDT
  by construction, per the param name), never from the UUDT variant, so the
  bit would be redundant with that choice of param.

The bundled mock now records each filter message (`mask`/`pattern`/
`flow_control`) as a full `StoredMessage` (including `TxFlags`), not just raw
bytes, and exposes `__mock_get_filter_{pattern,flow_control}_tx_flags` so
tests can assert the extended-addressing / 29-bit-Id flags were set.

## Alternatives Considered

1. **Skip installing a filter for an entire `UniqueRespIdTable` entry that has
   any address with flow control disabled, and count that CLL as "not fully
   covered"** — Would require per-entry (not per-CLL) coverage tracking for
   `sync_channel_fc_pass_all_filter`, a materially larger change to
   `LogicalLinkState`. Rejected as out of scope for this decision: as with
   ADR-039's existing per-CLL granularity, an entry that produces no filter
   (missing address pair, or flow control disabled) does not by itself keep
   the pass-all fallback alive if the CLL already has a filter from another
   entry — such an address remains reachable only through the pass-all
   fallback if it is still active, or not at all otherwise. This mirrors a
   limitation ADR-039 already accepted for missing address pairs; extending
   it to format-disabled entries keeps the two skip reasons consistent
   without a design change.

2. **Pass `pFlowControlMsg = NULL` instead of skipping the entry when flow
   control is disabled** — The real J2534 API requires a non-null
   `pFlowControlMsg` specifically for `FLOW_CONTROL_FILTER` (only
   `PASS_FILTER`/`BLOCK_FILTER` allow `NULL`), so this would not be
   spec-conformant either. Rejected.

3. **Interpret the Padding Overwrite bits (5,4) too, wiring them into the
   outgoing `PassThruWriteMsgs` path** — Unrelated to `FLOW_CONTROL_FILTER`
   construction (the subject of this ADR and ADR-039), and no existing
   CAN-ID-format-aware TX composition path exists to hang it off. Left for a
   future ADR if/when TX-side padding-per-address is needed.

## Consequences

- Extended ISO-TP addressing and 29-bit CAN Ids are now correctly represented
  in the point-to-point `FLOW_CONTROL_FILTER`s this service installs, closing
  the gap ADR-039 left open.
- An address whose format explicitly disables flow control gets no dedicated
  filter. Whether it can still receive frames depends on whether the shared
  channel's pass-all fallback happens to still be installed (see Alternative
  1) — a pre-existing, now-explicit limitation rather than a regression.
- `CanIdFormat`/`CanAddress`/`can_filter_message` are private to `rpc_link.rs`
  (not reused elsewhere yet); if a future TX-composition path needs the same
  bitfield decoding, it should be promoted to a shared location then.

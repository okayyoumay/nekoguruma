# ADR-041: UUDT Point-to-Point `FLOW_CONTROL_FILTER` from `CP_CanRespUUDTId` Addressing

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`CanIdFormat`, `install_point_to_point_fc_filter`,
             `install_point_to_point_fc_filters`), `j2534-0404-mock`

## Context

ADR-039 and ADR-040 build one point-to-point `FLOW_CONTROL_FILTER` per
`UniqueRespIdTable` entry from the entry's `CP_CanRespUSDTId` (the ECU's
segmented/USDT response CAN ID) and `CP_CanPhysReqId` (the tester's physical
request CAN ID, also used as the flow-control CAN ID). ADR-040 explicitly
scoped this to the USDT address only: "this service only ever builds
point-to-point filters from `CP_CanRespUSDTId`/`CP_CanRespUSDTFormat` ...
never from the UUDT variant."

`CP_CanRespUUDTId` / `CP_CanRespUUDTExtAddr` (the ECU's unsegmented/UUDT
response CAN ID and its ISO-TP extension-address byte) are already stored per
entry in `UniqueRespIdTable` (`comparam_support::CAN_UNIQUE_ID_UNUM32`) and
already drive frame *routing* to the correct CLL in `poll_rx` (ADR-007), but
had no corresponding hardware filter: an ISO15765 channel with a
`UniqueRespIdTable` entry that only sets `CP_CanRespUUDTId` (no
`CP_CanRespUSDTId`) produced zero point-to-point filters for that entry, so
the CLL's UUDT traffic depended entirely on the channel-wide pass-all
`FLOW_CONTROL_FILTER` fallback staying installed — which ADR-039 tears down
as soon as every CLL sharing the channel has *any* filter.

Per the J2534 v04.04 spec (ADR-038), `FLOW_CONTROL_FILTER` is the only valid
filter type on an ISO15765 channel, so a UUDT-specific filter must also be a
`FLOW_CONTROL_FILTER` — there is no `PASS_FILTER` option on this channel type
— even though UUDT frames are single-frame and never actually trigger an
ISO 15765-2 flow-control (CTS) response. This means `pFlowControlMsg` (a
required, non-null field for this filter type per ADR-040 Alternative 2) must
still be populated with *something*, despite flow control being conceptually
inapplicable to UUDT addressing.

ADR-040's Table B.13 decoding treats bit 0 (`CP_Can*Format`'s "Flow Control"
bit) as a gate: when clear, no filter is installed for that address. The
service's default `ComParam` sets (`comparam_defaults.rs`) set
`CP_CanRespUUDTFormat` to `0` (bit 0 clear) precisely because flow control
never applies to UUDT. Reusing the USDT gate verbatim for the UUDT filter
would therefore mean the new filter almost never installs under default
configuration, defeating the purpose of adding it.

## Decision

`install_point_to_point_fc_filters` now installs up to **two** point-to-point
`FLOW_CONTROL_FILTER`s per `UniqueRespIdTable` entry, independently:

- **USDT filter** (unchanged from ADR-039/040): built from `CP_CanRespUSDTId`
  when present, skipped if `CP_CanRespUSDTFormat` bit 0 (Flow Control) is
  clear.
- **UUDT filter** (new): built from `CP_CanRespUUDTId` when present. Table
  B.13 bit 0 of `CP_CanRespUUDTFormat` is **not** read for this filter — it is
  always installed when `CP_CanRespUUDTId` is present, since flow control is
  inherently not applicable to unsegmented UUDT addressing and gating on that
  bit would make the filter dead under the service's own defaults. Bits 3
  (extended addressing) and 1 (29-bit CAN Id) of `CP_CanRespUUDTFormat` are
  still decoded exactly as for USDT, selecting a 4- vs. 5-byte filter message
  and the `TxFlags`.

Both filters, when both are installed for the same entry, share the same
`pFlowControlMsg`: built from `CP_CanPhysReqId` / `CP_CanPhysReqFormat` /
`CP_CanPhysReqExtAddr`, exactly as the USDT filter's flow-control side already
was. This matches how ISO 22900-2 documents `CP_CanPhysReqId`: it also serves
for CAN transmission of flow-control frames, with no restriction to USDT-only
traffic, so reusing it as the (spec-required, if practically unused)
flow-control CAN ID for the UUDT filter needs no new addressing ComParam. An
entry missing `CP_CanPhysReqId` gets neither filter, since a `pFlowControlMsg`
is required for `FLOW_CONTROL_FILTER` regardless of which response address the
filter is protecting.

Both filter IDs (when installed) are appended to the same
`LogicalLinkState::unique_resp_filter_ids` vector used by ADR-039's coverage
tracking (`sync_channel_fc_pass_all_filter`): a CLL counts as "covered" the
moment it has at least one filter of either kind, unchanged from before.

## Alternatives Considered

1. **Reuse the USDT flow-control gate for UUDT too** — Rejected per explicit
   direction: under the service's own default `ComParam` sets
   (`CP_CanRespUUDTFormat = 0`), this would mean the UUDT filter is
   essentially never installed, making the feature dead on arrival for the
   common case where a caller never explicitly sets a nonzero format.

2. **Only install a UUDT filter on non-ISO15765 (plain CAN) channels, via
   `PASS_FILTER`** — UUDT (unsegmented, unacknowledged) traffic is arguably
   more at home on a plain CAN channel (e.g. OBD-II functional requests) than
   on ISO15765. Rejected for this ADR: it is a materially different code path
   (a new per-address `PASS_FILTER` construction and its own
   install/coverage/fallback bookkeeping for non-ISO15765 channels, which
   ADR-005 previously rejected as too complex) and a separate design decision
   from "extend the existing ISO15765 point-to-point filter mechanism to the
   UUDT address," which is what this ADR scopes to. Left for a future ADR if
   non-ISO15765 per-address filtering is needed.

3. **Pass `pFlowControlMsg = NULL` for the UUDT filter, since UUDT frames
   never trigger flow control** — Rejected for the same reason ADR-040
   rejected it for USDT: `PassThruStartMsgFilter` requires a non-null
   `pFlowControlMsg` specifically for `FLOW_CONTROL_FILTER` (only
   `PASS_FILTER`/`BLOCK_FILTER` allow `NULL`), and `FLOW_CONTROL_FILTER` is the
   only filter type valid on an ISO15765 channel (ADR-038).

## Consequences

- A `UniqueRespIdTable` entry that sets `CP_CanRespUUDTId` (with or without
  `CP_CanRespUSDTId`) now gets a dedicated hardware filter on ISO15765
  channels, closing the gap ADR-040 left open. An entry with both response
  types set gets two filters.
- `CP_CanRespUUDTFormat` bit 0 is effectively a no-op for filter construction
  (only bits 3 and 1 matter); this is a deliberate asymmetry with
  `CP_CanRespUSDTFormat`, not an oversight — see Alternatives #1.
- `install_point_to_point_fc_filter` (the single-filter builder) and its
  `resp`/`req` `CanAddress` parameters are unchanged; only the caller,
  `install_point_to_point_fc_filters`, now invokes it up to twice per entry.
- The bundled mock's existing per-filter recording (`StoredMessage`, ADR-039)
  and accessors already index filters by installation order on a channel, so
  no mock changes were needed to observe two filters from one entry in tests.

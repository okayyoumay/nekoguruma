# ADR-039: Point-to-Point `FLOW_CONTROL_FILTER` from UniqueRespIdTable Addressing

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`LogicalLinkState`, `SharedChannel`, `LinkView`),
             `j2534-0404-service/src/service/rpc_link.rs` (`install_pass_all_filter` and friends,
             `rpc_connect_com_logical_link`), `j2534-0404-service/src/service/rpc_misc.rs`
             (`rpc_set_unique_resp_id_table`, `CLEAR_MSG_FILTERS`), `j2534-0404-mock`

> **Note:** ADR-048 removes the unconditional pass-all fallback described
> below specifically at `ConnectComLogicalLink` — `SetUniqueRespIdTable`'s
> own point-to-point-filter and pass-all-fallback behaviour (the bulk of
> this ADR) is otherwise unchanged. See ADR-048 for the connect-time-scoped
> revision, including why Alternative #3 below is revisited for that one
> call site.

> **Amended by ADR-068**: `SetUniqueRespIdTable` no longer installs/removes
> filters itself. The UniqueRespIdTable gets a Working/Active split; this
> ADR's per-entry filter derivation (mask/pattern/flow-control bytes) is
> unchanged, but the timing moves from `SetUniqueRespIdTable` call time to
> promotion time (this CLL's own `ConnectComLogicalLink`, or `CoptUpdateparam`
> execution via `J2534Service::promote_unique_resp_id_table`), diff-gated
> against the previous Active table.

> **Amended by ADR-122**: the pass-all fallback described below (the
> zero-mask `FLOW_CONTROL_FILTER` this ADR's "Decision" and "Alternatives
> Considered #3" sections discuss) is removed entirely — ADR-048 already
> dropped it at `ConnectComLogicalLink`; ADR-122 drops it from the two
> remaining lifecycle points (`CoptUpdateparam` promotion,
> `CLEAR_MSG_FILTERS`) too. This ADR's per-entry point-to-point filter
> derivation is otherwise unchanged and remains authoritative.

## Context

ADR-005 (superseded by ADR-038 for filter-type selection) installs a
zero-mask/pattern/flow-control-frame `FLOW_CONTROL_FILTER` on every ISO15765
channel as a pass-all strategy: mask = pattern = flow-control = all-zero
4-byte messages.

The J2534 v04.04 spec constrains `PassThruStartMsgFilter` arguments for
`FLOW_CONTROL_FILTER` more tightly than that:

- `pMaskMsg` must be all `$FF` bytes: 4 of them, or 5 when the ISO15765
  extended-address `TxFlags` bit is set.
- Flow control filters are **point-to-point**: one filter must not cover
  several CAN identifiers (the single exception is masking the priority field of a
  29-bit CAN ID per ISO 15765-2 Annex A, where `Data[0]` can be `$E3` —
  not used here).
- `pPatternMsg` holds the real CAN ID of the far end of the ISO 15765-2
  conversation (the ECU's response ID); frames that do not match it are
  to be dropped.
- `pFlowControlMsg` holds the real CAN ID used when sending segmented
  frames (the tester's physical request ID), and is what the CAN ID of an outgoing
  segmented `PassThruWriteMsg` is checked against.

A zero mask matches every CAN ID, which violates all four of the above: it
is not `$FF`, and it is not point-to-point. This was a known, deliberate
trade-off in ADR-005 (see "Alternatives Considered" #3, rejected as too
complex to keep in sync with addressing ComParam changes), but it means the
service's default ISO15765 filter is not spec-conformant, and a
spec-conformant adapter could reasonably reject it.

`SetUniqueRespIdTable` (ADR-007, ADR-014) already carries exactly the
addressing information needed to build a conformant filter: each
`EcuUniqueRespEntry.params` can include `CP_CanRespUSDTId` (the ECU's
physical response CAN ID) and `CP_CanPhysReqId` (the tester's physical
request CAN ID) as `PDU_PC_UNIQUE_ID`-class params.

## Decision

`SetUniqueRespIdTable` now (re)builds the ISO15765 channel's
`FLOW_CONTROL_FILTER` set from the table it is given:

- For every entry with both `CP_CanRespUSDTId` and `CP_CanPhysReqId` set,
  install one point-to-point filter: `pMaskMsg` = 4 bytes of `$FF`,
  `pPatternMsg` = `CP_CanRespUSDTId` (big-endian), `pFlowControlMsg` =
  `CP_CanPhysReqId` (big-endian). Entries missing either param are skipped
  — a full pair is required to build a conformant filter.
- The CLL's previously-installed point-to-point filters (tracked in the new
  `LogicalLinkState::unique_resp_filter_ids`) are stopped first via
  `PassThruStopMsgFilter`.
- Once every CLL sharing the physical channel has at least one point-to-point
  filter installed, the channel-wide zero-mask pass-all fallback (installed
  at `ConnectComLogicalLink`, tracked in the new
  `SharedChannel::fc_pass_all_filter_id`) is redundant and non-conformant, so
  it is torn down (`sync_channel_fc_pass_all_filter`). If a CLL's table is
  later cleared (or a new CLL joins the channel before configuring
  addressing), the fallback is re-installed so the channel does not go deaf.
- `CLEAR_MSG_FILTERS` wipes every filter on the channel, including
  point-to-point ones. It now rebuilds the full filter set — point-to-point
  filters from each CLL's existing `UniqueRespIdTable`, then the pass-all
  fallback for any CLL still uncovered — instead of always re-installing a
  single zero-mask filter (`reinstall_iso15765_channel_filters_after_clear`).

Multiple `FLOW_CONTROL_FILTER`s may coexist on one channel; nothing in the
spec limits a channel to one. This is what makes per-ECU point-to-point
filters compatible with channel sharing (adapter-design.md's
"Physical Channel Sharing" — multiple CLLs, e.g. one per ECU, commonly share
one ISO15765 channel at the same baud rate).

Extended ISO-TP addressing (5-byte mask/pattern with a target-address-extension
byte, `TxFlags` `ISO15765_ADDR_TYPE` bit) was out of scope at the time of this
ADR: no other part of this service interpreted `CP_CanPhysReqFormat` /
`CP_CanRespUSDTFormat` to decide when extended addressing applies, so there
was no consistent source for the extension byte. **Addressed in ADR-040**,
which decodes the `CP_Can*Format` bitfield (ISO 22900-2 Table B.13) once its
exact bit layout was available.

The bundled mock (`j2534-0404-mock`) previously discarded `pMaskMsg` /
`pPatternMsg` / `pFlowControlMsg` entirely. It now records them per filter
(keyed by filter ID, cleared on `PassThruStopMsgFilter` /
`IOCTL_CLEAR_MSG_FILTERS`) and exposes both an in-process accessor
(`mock_get_channel_filters`) and `__mock_get_filter_*` FFI back-door exports,
so tests can assert the exact bytes sent to `PassThruStartMsgFilter`.

## Alternatives Considered

1. **Build the filter from `CP_CanPhysReqId` / `CP_CanRespUSDTId` in the
   regular (non-unique-ID) ComParam set instead of `UniqueRespIdTable`** —
   Rejected per explicit direction: `UniqueRespIdTable` is the canonical
   per-ECU addressing source (ADR-007), and using it means the filter update
   is driven by the same call a caller already makes to configure per-ECU
   routing, rather than a second, easy-to-forget step.

2. **Keep a single filter per channel, chosen from one "primary" table
   entry** — Fails for channels shared by multiple CLLs/ECUs (the documented
   common case), since a single point-to-point filter cannot match more than
   one ECU. Rejected in favor of one filter per valid entry.

3. **Drop the pass-all fallback entirely once any `SetUniqueRespIdTable`
   call is made** — Would starve any other CLL sharing the channel that has
   not yet configured its own table (including a CLL that joins the channel
   later). Rejected in favor of tracking full coverage across all CLLs on
   the channel before removing the fallback.

## Consequences

- ISO15765 channels are spec-conformant once every CLL sharing them has
  configured a `UniqueRespIdTable` entry with a full address pair — no more
  zero-mask `FLOW_CONTROL_FILTER` coexisting with (or standing in for) a
  precise one.
- Callers that never call `SetUniqueRespIdTable`, or whose entries lack a
  full `CP_CanRespUSDTId` + `CP_CanPhysReqId` pair, keep receiving the
  ADR-005 zero-mask fallback — behavior is unchanged for them.
- `SetUniqueRespIdTable` now does real hardware I/O (`PassThruStopMsgFilter`
  / `PassThruStartMsgFilter`) when the CLL is connected on an ISO15765
  channel, where before it only updated in-memory state. Individual
  `PassThruStartMsgFilter` failures are logged and skipped rather than
  failing the whole call, consistent with ADR-008's best-effort posture.
- `CLEAR_MSG_FILTERS` on an ISO15765 channel is more expensive (one
  `PassThruStartMsgFilter` call per covered CLL instead of one), but restores
  the full spec-conformant filter set rather than regressing to the zero-mask
  fallback.

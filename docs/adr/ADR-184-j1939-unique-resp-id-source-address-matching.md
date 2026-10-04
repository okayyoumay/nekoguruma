# ADR-184: SAE J1939 `CP_J1939SourceAddress`-Based UniqueRespIdTable Matching

**Date:** 2026-08-20
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events_rx_routing.rs`,
`events.rs`, `comparam_support.rs`, `tx_header.rs`,
`docs/implementation-notes.md` (`j2534-0404-service`)

## Context

`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
carried an open P2 item (ADR-179/Phase 5 deferred scope): `CP_J1939SourceAddress`
(`PARAM_J1939_SOURCE_ADDRESS`) was defined as a ComParam but unreachable on
both sides of `SetUniqueRespIdTable`-based per-ECU response routing.

On the RX-matching side, `CllRxEntry::unique_resp_ids` (read by
`events_rx_routing.rs`'s `route_frame`/`route_frame_uudt_only`/
`route_frame_matched_uudt`) was a raw `(unique_resp_identifier,
CP_CanRespUSDTId, CP_CanRespUUDTId)` 3-tuple with no slot for a J1939 source
address at all. `tx_header.rs` carried a correct, tested,
`#[allow(dead_code)]`'d `j1939_source_address(data: &[u8]) -> Option<u8>`
helper (`data[3]`, the received 29-bit CAN identifier's low byte) that fed
nothing, because there was nowhere in the matching path to feed it.

On the RPC-acceptance side, `comparam_support::unique_id_params` — the
function `SetUniqueRespIdTable`/`GetUniqueRespIdTable` (and
`is_param_allowed`'s own `is_unique_id_param` gate) consult to decide which
ComParams are `PDU_PC_UNIQUE_ID` class for a given protocol — had no
`J1939_PS` branch and fell to its `(&[], &[])` default. `SetUniqueRespIdTable`
therefore rejected every unum32/bytefield param on a J1939 CLL with
`PDU_ERR_COMPARAM_NOT_SUPPORTED`, including `CP_J1939SourceAddress` itself —
a J1939 CLL's `active_unique_resp_id_table` could only ever be empty. Both
gaps were confirmed, while investigating a related Codex review finding (PR
#72 round 1: a J1939-only, CAN-ID-less UniqueRespIdTable entry used to
silently blackhole every RX frame for that CLL — fixed separately by
filtering such entries out of `unique_resp_ids` entirely, restoring the
correct no-table wildcard fallback), to be genuinely unreachable together:
wiring either side alone would have been inert without the other.

Separately, `PARAM_J1939_SOURCE_ADDRESS` and `PARAM_J1939_SOURCE_NAME`
(`CP_J1939SourceName`) were listed inside `comparam_support.rs`'s
`CAN_UNIQUE_ID_UNUM32`/`CAN_UNIQUE_ID_BYTES` constants — i.e. classified as
CAN-family `PDU_PC_UNIQUE_ID` params. This classification was never actually
reachable: `unique_id_params` only ever dispatches to those two lists for
`protocol.is_can_family()` (`CAN`/`ISO15765`), and `J1939_PS.is_can_family()
== false`. Both entries were vestigial.

ISO 22900-2:2022's `CP_J1939SourceAddress` entry defines it as a
`PDU_PC_UNIQUE_ID`-class ComParam identifying a responding ECU's SAE
J1939-21 source address, with a NOTE describing that this ComParam's value
range formally extends to `0xFFFF` — wider than a plain 8-bit source-address
byte — to cover an alternate URID-assignment mode keyed by an 11-bit CAN
identifier rather than the ordinary 29-bit one. `CP_J1939SourceName`'s own
entry is the analogous NAME-keyed (network-management, ISO 22900-2's own
`PDU_PC_UNIQUE_ID`-classified NAME concept) counterpart; matching received
frames against it would require tracking NAME<->source-address claim
bookkeeping this codebase's existing J1939 address-claim/defend state
machine (ADR-180, `events_j1939_claim.rs`) does not build — a materially
different, unscoped mechanism.

## Decision

1. **`CllRxEntry::unique_resp_ids` becomes a named `UniqueRespIdKey` struct**
   (`events_rx_routing.rs`), replacing the raw
   `(u32, Option<u32>, Option<u32>)` 3-tuple: `unique_resp_identifier`,
   `can_resp_usdt_id` (`CP_CanRespUSDTId`), `can_resp_uudt_id`
   (`CP_CanRespUUDTId`), and the new `j1939_source_address`
   (`CP_J1939SourceAddress`). A shared `matched(can_id) -> Option<MatchKind>`
   method replaces the previously hand-duplicated USDT-then-UUDT `find`
   predicate `route_frame`/`route_frame_matched_uudt` each carried
   separately — a duplication already flagged as error-prone during this
   change's design review — with USDT -> UUDT -> J1939 SA precedence. SA
   matching reads `can_id & 0xFF`: SAE J1939-21's 29-bit CAN identifier
   packs the 8-bit source address into the identifier's low byte, so this is
   equivalent to (and replaces) `tx_header::j1939_source_address`'s raw
   `data[3]` read — that now-fully-dead helper (and its tests) is removed;
   its byte-layout derivation is preserved in `UniqueRespIdKey::matched`'s
   own doc comment. `route_frame_uudt_only` (the dual-channel-mode Companion
   path, ADR-046) is deliberately NOT routed through `matched` — it keeps
   its own `CP_CanRespUUDTId`-only check, since J1939 has no UUDT/Companion
   concept and a J1939 CLL never becomes a Companion channel.
   `build_cll_rx_entries`'s entry filter widens from "keep if USDT or UUDT
   is configured" to "keep if USDT, UUDT, or J1939 SA is configured".

2. **`comparam_support::unique_id_params` gains a `J1939_PS` branch**
   (dispatched via `resources::is_j1939_protocol_id`, mirroring
   `is_param_allowed`'s own J1939 dispatch idiom), returning a new
   `J1939_UNIQUE_ID_UNUM32 = &[PARAM_J1939_SOURCE_ADDRESS]` list (no
   bytefield entry). `PARAM_J1939_SOURCE_ADDRESS` is removed from
   `CAN_UNIQUE_ID_UNUM32` (never actually reachable there — see Context).

3. **`CP_J1939SourceName` stays out of scope.** It is removed from
   `CAN_UNIQUE_ID_BYTES` (its sole, likewise-unreachable entry) rather than
   relocated to a J1939-specific bytefield list — leaving CAN's own
   bytefield `PDU_PC_UNIQUE_ID` list empty — and is NOT added anywhere as
   `PDU_PC_UNIQUE_ID` class. It remains a plain settable ComParam via
   `is_j1939_param`'s existing allowlist, unchanged. RX-side NAME matching
   is recorded as a P3 backlog residual (`docs/implementation-notes.md`).

4. **The spec's 11-bit-CAN-ID extended-range NOTE is an accepted residual,
   not enforced.** `CP_J1939SourceAddress` accepts its full documented
   `[0, 0xFFFF]` range in `SetUniqueRespIdTable` (correct — accepting the
   spec's own range is not a gap); no additional range rejection is added.
   In practice a configured value above `0xFF` can never match: this
   codebase's J1939 CLLs always connect with the flat `CAN_29BIT_ID` flag
   (`rpc_link.rs`'s own `connect_flags` derivation, the
   `PROTOCOL_J1939_PS => j2534_0404::CAN_29BIT_ID` arm), never the 11-bit
   mode the NOTE's extended range exists for, so a real received frame's
   `can_id & 0xFF` can never equal a value above `0xFF`. Such an entry is
   configurable but permanently unmatchable — an explicitly-configured
   table with only unmatchable entries drops every frame, which is the
   correct, distinct-from-empty-table behavior (an explicit table is an
   explicit filter; the empty-table wildcard is a different, separate
   mode — `route_frame`'s own `unique_resp_ids.is_empty()` check).

5. **`tx_header::response_header_bytes`'s J1939 arm (ADR-179 Decision 9)
   is left as-is, with an updated comment — not wired to consult
   `entries.first()`'s SA.** Decision 9's comment cited "no ComParam records
   an expected response PGN at all (`comparam_support::unique_id_params` has
   no J1939 branch)" to justify wildcarding bytes 0-2. That citation is now
   corrected in place: `unique_id_params` has a J1939 branch as of this ADR,
   but it covers only `CP_J1939SourceAddress`, not an expected-response-PGN
   ComParam — no such ComParam exists, so bytes 0-2 remain correctly
   wildcarded, unchanged. Byte 3 (the responding ECU's source address) was
   evaluated separately for an entries-first-then-active fallback mirroring
   the pattern this same function already applies for KWP (round 11) and
   J1850: prefer a per-entry `PDU_PC_UNIQUE_ID` value, fall back to a
   coarser Active-set value. That pattern does not transfer cleanly here.
   KWP/J1850 resolve the SAME ComParam (`CP_PhysRespFormatPriorityType`)
   from two sources; J1939's existing byte-3 derivation instead reads a
   DIFFERENT ComParam (`CP_J1939TargetAddress`, the swapped-direction
   counterpart of this CLL's own configured destination) with its own
   sentinel semantics (`0xFFFF` = not configured, `0xFF` = BAM/broadcast)
   that do not apply to `CP_J1939SourceAddress`'s value space (`0` is an
   ordinary source address; the `>0xFF` extended range is this ADR's own
   accepted residual, not "unconfigured"). Reusing the existing match arms
   against an entries-sourced SA would misapply TARGET-address sentinel
   meaning to a SOURCE-address value space that does not share it. Byte 3
   therefore stays resolved from `CP_J1939TargetAddress` only, unchanged
   from pre-ADR-184 behavior; this remains a documented, deliberate
   residual, not an oversight.

## Consequences

- J1939 per-CLL source-address disambiguation on a shared physical channel
  is now live: a client that explicitly configures `SetUniqueRespIdTable`
  with `CP_J1939SourceAddress` entries gets real RX routing by responding
  ECU, closing the routing gap `tx_header::j1939_source_address` was
  originally added, but never wired up, to close.
- **Two RPC-surface behavior changes, both correcting prior misbehavior
  rather than removing working functionality:**
  - CAN/ISO15765 channels no longer accept `CP_J1939SourceAddress` or
    `CP_J1939SourceName` in `SetUniqueRespIdTable` — they never should have
    (both were vestigial, unreachable-in-practice CAN-family list entries).
  - J1939 `Get`/`SetComParam` no longer accepts `CP_J1939SourceAddress`
    directly (it previously did, via `is_j1939_param`'s allowlist, since
    `is_unique_id_param` had nothing to gate it with). It is now
    `PDU_PC_UNIQUE_ID` class and therefore `UniqueRespIdTable`-only, per
    ISO 22900-2 §9.3.3.6's reservation rule this codebase already enforces
    for every other `PDU_PC_UNIQUE_ID` ComParam.
- `CP_J1939SourceName` RX matching and the spec's 11-bit-CAN-ID
  extended-range NOTE mode both remain documented, un-implemented
  residuals (`docs/implementation-notes.md` P3 backlog) — not silently
  dropped, and not conflated with this ADR's actual scope.
- Amends ADR-179 Decision 9's own note in place (does not supersede it, and
  does not change Decision 9's established byte-3/wildcard behavior): the
  "`comparam_support::unique_id_params` has no J1939 branch at all" claim
  that justified wildcarding bytes 0-2 is corrected to scope specifically to
  "no expected-response-PGN ComParam", since a J1939 branch now exists for
  a different param. ADR-179's Status line is unchanged — nothing in it is
  superseded by this ADR.

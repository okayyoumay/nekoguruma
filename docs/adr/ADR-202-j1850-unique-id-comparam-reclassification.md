# ADR-202: SAE J1850 Response-Addressing ComParam Reclassification to `PDU_PC_UNIQUE_ID`

**Date:** 2026-08-30
**Status:** Accepted (its own accepted "RX-side per-ECU URID tagging does
             NOT work for J1850 -- or KWP" limitation resolved by ADR-203)
**Affects:** `j2534-0404-service/src/service/comparam_support.rs`,
`docs/implementation-notes.md` (`j2534-0404-service`)

## Context

`j2534-0404-service/docs/implementation-notes.md`'s 2022-edition delta audit
(2026-08-11) flagged a `PDU_PC_UNIQUE_ID` classification mismatch for SAE
J1850 VPW/PWM: ISO 22900-2:2022 Table B.11 (the transport-layer ComParam
summary table) marks `CP_EcuRespSourceAddress`, `CP_FuncRespFormatPriorityType`,
`CP_FuncRespTargetAddr`, and `CP_PhysRespFormatPriorityType` as mandatory-support
`PDU_PC_UNIQUE_ID`-class for J1850 — the same four params
`KWP_UNIQUE_ID_UNUM32` (`comparam_support.rs`) already lists for the KWP
family — while scoping `CP_MidRespId` to SAE J1708 only, not J1850. This
repo's `J1850_UNIQUE_ID_UNUM32` had exactly the inverse membership: it
listed only `CP_MidRespId`.

The 2009 edition could not have settled this classification on its own: its
own transport-layer and application-layer ComParam tables disagree with each
other about these four params' class, which is why the mismatch survived
the repository's original 2009-based conformance audit and was only caught
by the later 2022-edition delta pass.

This misclassification had a concrete, reachable consequence, not just a
paperwork mismatch. `SetUniqueRespIdTable`'s per-entry class validation
(`rpc_misc.rs::rpc_set_unique_resp_id_table`) consults
`comparam_support::unique_id_params` to decide which ComParams a table entry
may contain; with only `CP_MidRespId` classified, every entry a
2022-conforming J1850 client tried to key by `CP_EcuRespSourceAddress` was
rejected with `PDU_ERR_COMPARAM_NOT_SUPPORTED`, and
`GetUniqueRespIdTable`'s no-table template offered such a client only 1
fillable param instead of the real 5. Meanwhile the READ side of this
mechanism was already built and working: `tx_header.rs`'s
`ecu_addr`/`response_header_bytes` (used by, among other things, SAE
J2534-2 clause 14 Repeat Messaging's stop-condition template composition)
already resolves `CP_EcuRespSourceAddress` entries-first from the
UniqueRespIdTable identically for KWP and J1850 — but no J1850 entry
configuring it could ever be successfully staged, so that mechanism was
permanently unreachable for J1850 specifically.

The audit finding also noted that `CAN_UNIQUE_ID_UNUM32`/
`KWP_UNIQUE_ID_UNUM32` both already carry `CP_MidRespId` as a documented
surplus/inert entry — Table B.11 is a minimum-support matrix, not an
exclusive whitelist, so an empty cell for a given protocol column means
"not required," not "forbidden." Fixing J1850's list creates the same
question for its own now-inverted-in-reverse `CP_MidRespId` entry.

Separately, the audit surfaced that Table B.11 actually scopes
`CP_MidRespId` to SAE J1708 exclusively — a protocol this repo does
implement (ADR-175, Phase 11) but which has no `unique_id_params` branch of
its own at all, falling instead into the function's final "SCI and unknown
protocols" `else` arm.

## Decision

1. **`J1850_UNIQUE_ID_UNUM32` gains the same four response-addressing
   params `KWP_UNIQUE_ID_UNUM32` already lists** —
   `PARAM_ECU_RESP_SOURCE_ADDR`, `PARAM_FUNC_RESP_FORMAT_PRIORITY`,
   `PARAM_FUNC_RESP_TARGET_ADDR`, `PARAM_PHYS_RESP_FORMAT_PRIORITY` — in the
   same order, alongside the existing `PARAM_MID_RESP_ID`. No bytefield
   list change: `J1850_UNIQUE_ID_BYTES`'s empty return stays empty, since
   Table B.11 has no J1850-scoped bytefield `PDU_PC_UNIQUE_ID` entry.

2. **`CP_MidRespId` stays in J1850's list, deliberately, as documented
   surplus** — matching the existing `CAN_UNIQUE_ID_UNUM32`/
   `KWP_UNIQUE_ID_UNUM32` precedent for the identical reason (an empty
   Table B.11 cell means "not required," not "forbidden"). Removing it now
   would create a cross-protocol inconsistency (present for CAN/KWP,
   absent for J1850) that a future uniform cleanup pass across all three
   lists would have to partially undo. The companion P3 backlog entry
   already recording this residual for CAN/KWP is extended, not
   duplicated, to also cover J1850's own copy.

3. **SAE J1708's own adjacent gap is explicitly deferred, not fixed
   here.** Unlike J1850, where this fix completes an already-built
   read-side mechanism, J1708 has no read-side consumer for MID values
   anywhere in this codebase: `tx_header.rs` has zero MID references, and
   `is_j1708_param` (`comparam_support.rs`) is ADR-175's own deliberate
   closed allow-list of exactly `DATA_RATE`/`LOOPBACK`/
   `PARAM_MESSAGE_PRIORITY`, with no MID param reachable via plain
   `SetComParam` either. Adding a `J1708_UNIQUE_ID_UNUM32` branch now would
   make `SetUniqueRespIdTable` accept `CP_MidRespId` entries for J1708 with
   no corresponding read-side effect — a misleading no-op capability, worse
   than the current honest rejection. Closing this needs a real J1708
   MID-addressing feature (TX composition from `CP_MidReqId`, the new list
   itself, and an RX-disambiguation decision), recorded as a new P3
   backlog entry rather than attempted here.

## Alternatives rejected

- **Removing `CP_MidRespId` from J1850's list instead of keeping it as
  surplus.** Rejected: creates the exact cross-protocol inconsistency
  Decision 2 avoids, and shrinks `GetUniqueRespIdTable`'s J1850 template to
  a strange near-empty intermediate shape if done without also touching
  CAN/KWP in the same pass. A future uniform-removal pass across all three
  lists remains the right place for this, if ever done.
- **Adding the four params to J1850's general `SetComParam` allow-list
  instead of the `PDU_PC_UNIQUE_ID` list.** Rejected: contradicts ISO
  22900-2 §9.3.3.6's exclusive-URID-management design for
  `PDU_PC_UNIQUE_ID`-class params, and this repo's own established
  KWP/CAN/J1939 (ADR-184) precedent for how these get gated — a
  `PDU_PC_UNIQUE_ID` param is reachable only through
  Get/SetUniqueRespIdTable, never plain Get/SetComParam.
- **Fixing SAE J1708's own `CP_MidRespId` gap in this same change.**
  Rejected: would ship a write-accepting, read-side-no-op capability,
  contradicting ADR-175's own deliberate closed ComParam allow-list without
  a dedicated design pass to justify widening it. Recorded as a separate P3
  backlog item instead (see Context/Decision 3).
- **Also teaching `events_rx_routing.rs`/RX URID tagging about these
  entries in this same change.** Deferred, not unnecessary. (An earlier
  revision of this bullet claimed the RX side was verified unaffected; a
  Codex review of this PR showed that verification checked the wrong
  rule — frame *delivery* is indeed unaffected, because
  `build_cll_rx_entries` filters `CP_EcuRespSourceAddress`-only entries
  out of `unique_resp_ids` and `route_frame` falls back to no-table
  wildcard delivery, but every delivered frame's `unique_resp_identifier`
  is therefore the wildcard sentinel `0`, never the application-assigned
  URID, contrary to ISO 22900-2:2022 §8.4.28.7.2's match-then-return-URID
  model.) The fix is KWP-and-J1850-shaped — KWP has shipped the identical
  gap since `KWP_UNIQUE_ID_UNUM32` first accepted these params — and
  carries its own open design points (pre-header-split source-byte
  availability, §8.4.28.7.3 unknown-response/`PDU_ID_UNDEF` semantics,
  RawMode gating per ADR-196/198/200, TX-echo handling); see Consequences
  and the new P2 backlog entry.

## Consequences

- 2022-conforming J1850 clients can now populate per-ECU UniqueRespIdTable
  entries following the spec's own template-then-fill-then-Set flow;
  `GetUniqueRespIdTable`'s J1850 template now offers all 5 unum32 params
  instead of 1.
- `tx_header.rs`'s `ecu_addr`/`response_header_bytes` — already built,
  already tested at the unit level, and already live for KWP — becomes
  reachable for J1850 for the first time via a real `SetUniqueRespIdTable`
  call, including for SAE J2534-2 clause 14 Repeat Messaging's
  stop-condition template composition. Only two of the four newly-added
  params are actually consumed this way: `CP_EcuRespSourceAddress`
  (`ecu_addr`) and `CP_PhysRespFormatPriorityType` (`response_header_bytes`'s
  format byte). **`CP_FuncRespFormatPriorityType`/`CP_FuncRespTargetAddr`
  (Codex review, this PR) are accepted and echoed by
  Get/SetUniqueRespIdTable but never read anywhere** —
  `response_header_bytes` rejects functional addressing outright for
  KWP/J1850 (this service has no functional-response composition path for
  either protocol), the identical situation KWP's own copies of these two
  params have always been in. Not a regression this PR introduces; not
  fixed here — a genuine K-line/J1850 functional-response feature (if one
  is ever built) would need its own design pass, and functional
  *responses* are not a common concept on these buses to begin with
  (unlike functional *requests*, which already work). Documented, not
  tracked as an open backlog item, mirroring how the surplus `CP_MidRespId`
  entry is documented rather than tracked.
- Plain `SetComParam`/`GetComParam` on these four params for J1850 still
  rejects with the same `PDU_ERR_COMPARAM_NOT_SUPPORTED` error as before —
  only the gate producing that rejection changed, from falling through
  `is_j1850pwm_param`/`is_j1850vpw_param`'s general allow-list (which never
  listed these four params) to `is_unique_id_param`'s dedicated gate. No
  observable accept/reject behavior change on that path.
- Closes the pre-existing `CP_PhysRespFormatPriorityType` P3 backlog item
  in `docs/implementation-notes.md`, which was explicitly waiting on this
  ComParam-classification question.
- Two residuals recorded in `docs/implementation-notes.md`: the
  CAN/KWP/J1850 surplus-`CP_MidRespId` cleanup companion P3 (extended to
  cover J1850's own copy, not duplicated), and a new SAE J1708
  MID-addressing P3 (Decision 3) recording the still-open, structurally
  different gap this fix deliberately does not close.
- **Accepted limitation at the time of this PR (Codex review): RX-side
  per-ECU URID tagging did NOT work for J1850 — or KWP, which had always
  shared this gap.** A staged `CP_EcuRespSourceAddress` entry affected
  TX-side response-header composition only; `build_cll_rx_entries` filtered
  it from `unique_resp_ids`, so `route_frame` ran in no-table wildcard mode
  and every delivered frame carried `unique_resp_identifier = 0`. Two
  client-visible consequences: (1) RX events reported URID `0` instead of
  the application-assigned identifier (a conformance gap against
  §8.4.28.7.2); (2) an `ExpectedResponseData` descriptor with a restricted
  nonzero `unique_resp_ids` set could never match on a J1850/KWP CLL — the
  COP timed out silently. **Resolved by
  [ADR-203](ADR-203-kwp-j1850-source-address-rx-routing.md):** `route_frame`
  now has a real `CP_EcuRespSourceAddress`-keyed matching tier for KWP/J1850,
  closing both consequences for a CLL that configures such entries (a CLL
  with none still wildcards, unaffected).

# ADR-203: KWP/J1850 `CP_EcuRespSourceAddress`-Based RX Routing

**Date:** 2026-08-31
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs`,
`events_rx_routing.rs`, `docs/implementation-notes.md` (`j2534-0404-service`),
`docs/j2534-0404-architecture.md`, `tests/live_grpc_flow.rs`
(`j2534-0404-service`)

## Context

[ADR-202](ADR-202-j1850-unique-id-comparam-reclassification.md) made
`CP_EcuRespSourceAddress`-keyed `SetUniqueRespIdTable` entries acceptable
for J1850 (KWP already accepted them, and always had), closing the
write/acceptance side of a gap ADR-202 itself deferred as its own accepted
limitation. But `events_rx_routing.rs::route_frame` — the function that
decides which `unique_resp_identifier` a received frame is delivered under
— had no matching tier for either protocol family's own
`CP_EcuRespSourceAddress` at all: it only ever consulted
`CP_CanRespUSDTId`/`CP_CanRespUUDTId` (CAN) and `CP_J1939SourceAddress`
(J1939, [ADR-184](ADR-184-j1939-unique-resp-id-source-address-matching.md)).
A table entry keyed only by `CP_EcuRespSourceAddress` was filtered out of
`CllRxEntry::unique_resp_ids` entirely by `build_cll_rx_entries` (the
pre-existing PR #72 round-1 fix, generalized), so every KWP/J1850 CLL ran
in wildcard mode — `unique_resp_identifier == 0` on every RX delivery,
regardless of how many distinct ECU entries a client configured.

This violates ISO 22900-2:2022 §8.4.28.7.2's known-response model
(paraphrased): a received message's header is matched, protocol-specifically,
against the UniqueRespIdTable, and only once a real per-ECU identifier is
found is the payload matched against the active ComPrimitives' expected-
response structures and the identifier returned to the client. With
`route_frame` never producing anything but `0` for KWP/J1850,
`ExpectedResponseData.unique_resp_ids`-restricted matching was permanently
unsatisfiable on these two families: a COP naming any real configured
identifier could never see a frame carrying it, and would time out
silently.

Routing happens on RAW, pre-header-split frame bytes: `poll_rx_inner`
computes `frame_can_id` and calls `process_frame_for_entry`/`route_frame`
directly off the native `PASSTHRU_MSG` data, and only afterward — for a
frame that was actually delivered — does the ADR-051 header/footer split
(`header_footer_len`) run, to carve `ResultData.extra_info`. This ordering
is why a NEW raw-byte parse was needed for this fix rather than reusing
either of the two existing "read a KWP/J1850 source byte" mechanisms this
codebase already has, both of which operate strictly downstream of routing:
`header_footer_len` itself (produces a header/footer split, not a
source-address value), and the ADR-148 third-Amendment Fix 1 `source_id`
derivation in `bind_frame` (operates on `concat_meta.header_bytes`, which
only exist AFTER a frame has already been routed and split — a `ConcatBuf`-
keying-only mechanism).

## Decision

1. **A new shared raw-byte helper, `kline_j1850_source_addr`** (`events.rs`,
   next to `kwp_header_and_payload_len`): given a base protocol id and a raw
   frame, returns the responding ECU's source-address byte, or `None` when
   the frame is source-less or too short to contain one. For KWP
   (ISO9141/ISO14230), it leans on `kwp_header_and_payload_len` to find the
   header shape and reads index 2 whenever the header is at least 3 bytes
   long (every KWP header shape of that length places the source byte
   there); a source-less "unaddressed" KWP header (length 1 or 2) correctly
   reports `None`. An explicit `data.len() >= 3` guard closes a bounds gap
   in `kwp_header_and_payload_len` itself: for the addressed/embedded-length
   shape, that function can report a 3-byte header for a frame actually
   shorter than 3 bytes, with no bounds check of its own in that arm. For
   J1850 (VPW/PWM) the header is unconditionally 3 bytes, mirroring
   `header_footer_len`'s own J1850PWM arm, so `data.len() >= 3` alone
   suffices. This per-protocol header-length logic is documented as needing
   to stay in sync with `header_footer_len`'s own table, and is explicitly
   cross-cited against (and kept separate from) the ADR-148 Fix 1
   derivation — different bounds guards, different inputs (raw vs.
   already-split bytes), different purposes (RX delivery routing vs.
   `ConcatBuf` keying).

2. **`poll_rx_inner` pre-computes `frame_source_addr` once per message**,
   alongside the existing `frame_can_id` computation, keyed off the frame's
   own native `frame_protocol_id` (normalized through
   `resources::base_protocol_id`) — so it is always `None` on CAN/J1939/
   TP2.0/ISO15765 channels by construction, a provable no-op there. This is
   threaded through `process_frame_for_entry` as a new `frame_source_addr:
   Option<u8>` parameter, reaching only the plain `route_frame` call site
   (`RxEntryKind::Hardware`'s non-`native_mixed` arm) — `route_frame_uudt_only`/
   `route_frame_matched_uudt` do not need it, since KWP/J1850 CLLs never use
   the UUDT dual-channel-mode companion path.

3. **`route_frame` gains a new protocol-gated branch**, placed after the
   existing `unique_resp_ids.is_empty()` wildcard check (an SA-less table
   still wildcards, unchanged) but before the generic CAN-ID `.find()`. For
   ANY KWP/J1850 entry (`entry.header_protocol`), the branch takes one of two
   paths and NEITHER ever falls through to the generic CAN-ID `.find()`
   below — a content frame (a real message body) is matched against
   `UniqueRespIdKey::ecu_resp_source_addr` directly, and `None` — either no
   source byte was parsed, or no configured entry matched it — means the
   frame is DROPPED for this CLL, not delivered at `0`; a non-content frame
   (see the `is_content_frame` bullet below) is delivered unconditionally at
   the wildcard `0`. This is deliberately NOT routed through
   `UniqueRespIdKey::matched`: that method's `can_id: u32` parameter is
   meaningless for KWP/J1850, and widening every one of `matched`'s existing
   five tiers to accept `Option<u32>` instead is a much larger, riskier
   change than a self-contained early-return branch. Precedent for this
   "protocol-specific routing decision made outside `matched()`, before the
   generic tiers" shape already exists in this same area: the ADR-192 TP2.0
   broadcast-echo drop in `poll_rx_inner`, and `route_frame_uudt_only`
   itself.

   **`is_content_frame`-gated (Codex review finding, this PR, revised after
   a SECOND Codex review finding on the same mechanism):** the KWP/J1850
   branch's two paths are selected by `poll_rx_inner`'s own
   `is_content_frame` (ADR-100 Decision §3) — a content frame takes the SA-
   matching path above; a non-content frame (SOM/RX_BREAK/TxDone/loopback,
   ADR-097/ADR-098) is delivered unconditionally at `0`, restoring the exact
   pre-ADR-203 behavior for these frames regardless of the SA table's
   contents. This is a dedicated early return for the whole KWP/J1850
   branch, NOT a plain `is_content_frame &&` guard that lets a `false` case
   fall through to the generic CAN-ID `.find()` below — an earlier revision
   of this fix used exactly that guard, and a second Codex review finding
   caught why it was still wrong: two DIFFERENT bugs, not one.
   - Indication frames on ISO9141/ISO14230/J1850 routinely carry no data at
     all (an empty SOM/RX_BREAK payload), so `frame_source_addr` is always
     `None` for them — gating only the SA-matching call itself (not the
     whole branch) would still let `frame_source_addr.and_then` drop every
     such indication on a CLL with ANY SA-keyed entry, before `bind_frame`'s
     own `indication_suppressed` policy (which, notably, never suppresses
     `RX_BREAK`) ever got a chance to run.
   - `frame_can_id` (`events.rs`'s `poll_rx_inner`) is computed generically
     from the frame's own first 4 raw bytes with NO protocol check — `None`
     only when `data.len() < 4`. A loopback/TxDone indication carries the
     real echoed header+payload bytes of whatever was transmitted, routinely
     `>= 4` bytes for any realistic K-line/J1850 request — so `frame_can_id`
     is `Some(<K-line header bytes misread as a u32>)` for it, NOT `None`.
     The guard-only version's assumption that "the CAN-ID fallback always
     evaluates to `Some(0)` for KWP/J1850 since they never carry a CAN id"
     was FALSE for exactly this case: falling through would search
     `entry.unique_resp_ids` (populated only with SA-keyed entries for a
     KWP/J1850 CLL) via `UniqueRespIdKey::matched`, which can never succeed,
     dropping the echo instead of delivering it at `0` — violating the same
     ADR-098 "loopback/TxDone must never be filtered from delivery"
     invariant this whole gate exists to protect, just for a different
     indication subtype than the empty-payload case above. The fix is
     structural, not assumption-based: KWP/J1850 frames — content or not —
     now never reach the generic CAN-ID `.find()` at all.

4. **`UniqueRespIdKey` gains one new field, `ecu_resp_source_addr:
   Option<u32>`** (no new `MatchKind` variant — one would be dead code,
   since the new branch never calls `.matched()`).

5. **`build_cll_rx_entries`'s entry-retention filter gains a
   PROTOCOL-GATED `CP_EcuRespSourceAddress` case.** This is the subtlest
   part of this change. `CP_EcuRespSourceAddress` is not exclusively a
   KWP/J1850 param: it is ALSO a legal, already-accepted, but deliberately
   INERT surplus param inside `CAN_UNIQUE_ID_UNUM32` (documented in
   `docs/implementation-notes.md`'s 2022-edition-delta-audit section) — a CAN
   client can legally stage a table entry containing only
   `CP_EcuRespSourceAddress`, which means nothing to CAN's own USDT/UUDT/
   J1939-SA routing tiers and, before this ADR, was correctly filtered out
   of `unique_resp_ids` entirely, keeping that CLL in wildcard mode. Simply
   widening the filter's retention condition to include
   `ecu_resp_source_addr.is_some()` UNCONDITIONALLY — mirroring how the
   J1939 SA field was added in ADR-184 — would have been wrong: it would
   flip a CAN CLL's SA-only surplus entry from "filtered out → wildcard
   delivery" to "retained but permanently unmatchable by any of CAN's own
   routing tiers → every frame silently dropped for that CLL" — resurrecting
   the exact PR #72 round-1 bug class the filter's own comment already
   documents. The fix instead computes a `sa_routable` boolean from the
   link's own `resources::base_protocol_id(l.hw_protocol_id)` (KWP/J1850
   only) and only reads/retains `PARAM_ECU_RESP_SOURCE_ADDR` when it is
   `true`; for every other protocol family the field stays `None` for that
   entry and contributes nothing to the retention decision, exactly as
   before this ADR. A dedicated regression test
   (`ecu_resp_source_addr_only_surplus_entry_is_filtered_out_for_a_can_cll`,
   `events_build_cll_rx_entries_tests.rs`) pins this: a CAN CLL with an
   SA-only surplus entry still wildcards every frame.

6. **No RawMode gate.** [ADR-196](ADR-196-cll-create-flag-raw-mode-checksum-mode-phase1.md)
   Decision item 3 already establishes that `route_frame`'s own URID-table
   matching stays active under RawMode — RawMode's bypass convention
   applies to ComParam-derived TX composition and the RX header/footer
   split, not to client-configured `SetUniqueRespIdTable` matching itself.
   The new SA tier reads a raw wire byte against a client-configured value,
   the identical class of matching RawMode already keeps active for the
   CAN-ID tiers, so it needs no special-casing.

7. **No `is_tx_side` gate.** Mirrors ADR-184's existing, likewise-ungated
   J1939 SA tier. A TX-side echo frame's own byte-2 value is the TESTER's
   own configured address (`CP_TesterSourceAddress`/`NODE_ADDRESS`, default
   `0xF1` — `tx_header.rs`'s own composition functions place the source
   byte at index 2 in every header shape they build), never a responding
   ECU's configured `CP_EcuRespSourceAddress` value, so an echo naturally
   fails to match any real entry and is dropped in table mode — mirroring
   how a CAN table-mode echo already behaves. No special-case echo-detection
   logic is added; the natural mismatch already produces the correct drop.

## Alternatives rejected

- **A new `MatchKind` tier inside `UniqueRespIdKey::matched`.** Rejected —
  see Decision item 3: `matched`'s `can_id: u32` parameter has no
  correspondence for a CAN-id-less KWP/J1850 frame, and widening every
  existing tier's signature to `Option<u32>` is a much larger, riskier
  change than a self-contained protocol-gated early return.
- **Reordering the ADR-051 header split to run before routing.** Rejected —
  reorders a load-bearing pipeline stage (used by tester-present discard,
  `ConcatBuf` keying, and `ResultData.extra_info` construction) for the sake
  of parsing one byte earlier; the new raw-byte helper achieves the same
  result without touching that ordering at all.
- **Reusing the ADR-148 third-Amendment Fix 1 `source_id` derivation
  directly.** Rejected — it operates on the wrong side of the split
  (`concat_meta.header_bytes`, populated only after a frame is already
  routed and split), uses different bounds-checking guards suited to its
  own already-split input, and feeds a completely different mechanism
  (`ConcatBuf` keying, not RX delivery routing). Cross-cited in both
  functions' doc comments instead of merged.
- **Delivering an unmatched table-mode frame at URID `0` instead of
  dropping it.** Rejected — contradicts ISO 22900-2:2022 §8.4.28.7.3's own
  unmatched-response model (paraphrased): a response is delivered as
  `PDU_ID_UNDEF` only when the client has explicitly configured a
  params-empty catch-all table entry with that identifier; with no such
  entry, the correct behavior is to drop the frame, exactly mirroring this
  codebase's existing CAN/J1939 table-mode behavior.
- **Implementing the `PDU_ID_UNDEF` catch-all-entry delivery path in this
  same change.** Rejected as out of scope — a separate, PRE-EXISTING,
  protocol-WIDE gap: a params-empty `UniqueRespIdTable` entry with
  `unique_resp_identifier = PDU_ID_UNDEF` is stripped by
  `build_cll_rx_entries`'s filter today for EVERY protocol, including CAN,
  not something this fix's scope introduced or needs to close. Recorded as
  a new, separate P3 backlog item in
  `j2534-0404-service/docs/implementation-notes.md`.

## Consequences

- Table-mode KWP/J1850 CLLs now get real per-ECU `unique_resp_identifier`
  values on RX events, and `ExpectedResponseData.unique_resp_ids`-restricted
  matching now actually works on these two protocol families — including
  correct disambiguation between multiple sibling CLLs sharing one physical
  channel by distinct configured `CP_EcuRespSourceAddress` values, the same
  capability ADR-184 already gives J1939.
- **A client configuring an SA table for KWP/J1850 now opts into DROPPING
  unknown-source and source-less (unaddressed-KWP-format) frames in table
  mode** — a real, intentional, spec-conformant behavior change from
  today's blanket wildcard delivery, matching CAN/J1939's existing
  table-mode behavior. A CLL with NO SA-keyed entries at all is completely
  unaffected — still wildcard-`0`.
- **Two accepted residuals, both mirroring existing ADR-184 precedent for
  J1939, neither fixed here:**
  - A configured `CP_EcuRespSourceAddress` value above `0xFF` can never
    match a real frame — the field is `Option<u32>` (matching the ComParam's
    own Unum32 wire type) but a real source-address byte's range is
    `0..=0xFF`, and the comparison is `u32::from(byte)`. Unenforced but
    unreachable in practice.
  - A client configuring a table entry keyed to the TESTER's own address
    (rather than a responding ECU's) would cause TX-side echoes to
    incorrectly match, since the new tier has no `is_tx_side` gate (Decision
    item 7). An accepted, documented pathological-configuration residual,
    not fixed.
- The pre-existing, protocol-wide `PDU_ID_UNDEF` catch-all-entry delivery
  gap (Alternatives rejected, above) is recorded as a new, separate P3
  backlog item — not fixed by this ADR, and not introduced by it.

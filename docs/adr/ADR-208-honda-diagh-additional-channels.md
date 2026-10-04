# ADR-208: SAE J2534-2 Additional Channels (_CHx) for Honda DIAG-H

**Date:** 2026-09-01
**Status:** Accepted
**Affects:** `j2534-0404/src/lib.rs`, `j2534-0404-service/src/service/resources.rs`,
`j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/comparam_id.rs`,
`j2534-0404-mock/src/lib.rs`, `j2534-0404-service/tests/grpc_mock/honda_diagh.rs`,
`docs/j2534-2-support-plan.md`, `j2534-0404-service/docs/implementation-notes.md`

## Context

[ADR-207](ADR-207-uart-echo-byte-additional-channels.md) closed UART Echo Byte's own
`_CHx` gap and left two families in the "ready now, no design pass needed" group:
Honda DIAG-H (SAE J2534-2 clause 13, Phase 10/[ADR-174](ADR-174-j2534-2-honda-diagh-phase10.md))
and SAE J1708 (Phase 11/ADR-175) — TP2.0 (Phase 7) stays deliberately deprioritized as
the most structurally complex of the four. This ADR picks Honda DIAG-H, the earlier of
the two remaining families by phase number, mirroring ADR-207's own "earliest position"
tie-break.

**Unlike ADR-207's own two first drafts, this ADR's investigation was done comprehensively
up front, not discovered round-by-round after implementation.** ADR-207 found three
separate, independent gap classes across three rounds (a missing `names.rs` bypass; a
Discovery-gating arm not widened to recognize `_CHx`; three more raw-`hw_protocol_id`
call sites with the identical gap) — each missed by the investigation that preceded it.
Rather than repeat that pattern, this ADR's investigation explicitly checked all three
classes up front, using ADR-207's own backlog note (`implementation-notes.md`'s
ADR-206/207-citing P2 entry) as a checklist:

1. **`names.rs`'s `resolve_pin_selection` Honda DIAG-H arm** (`is_honda_diagh_protocol_id`
   gate, `names.rs:1102`). The outer gate is already an exact `_PS`-only match — safe,
   like UART Echo Byte's own arm, not the J1939-shaped widened-gate problem. But the arm
   has **no `requested_index.is_some()` bypass at all** — the same missing-piece gap
   ADR-207 found and fixed for UART Echo Byte (Decision item 8 there). Fixed here by
   Decision item 5 below.
2. **Every raw-`hw_protocol_id` consumer of `is_honda_diagh_protocol_id`** (as opposed to
   a normalized `base_hw_protocol_id()`/`ChannelProtocol`-keyed consumer, inherently safe
   since `ChannelProtocol` has no separate `_CHx` variant, or a resource-table-row-keyed
   consumer, inherently safe since no table row is ever itself a `_CHx` id). A full sweep
   of every call site found:
   - `resources.rs`'s `connect_discovery_check` (the ADR-185 Stage-1 Discovery
     fail-fast gate) — a bare exact-`_PS`-match arm, the identical gap
     `connect_discovery_check`'s UART Echo Byte arm had before ADR-207's fix. **Vulnerable.**
   - `resources.rs`'s `bustype_default_name_for_hw_protocol_id` — exact-match branch.
     **Vulnerable.**
   - `resources.rs`'s `peer_closed_set_extra_pins` (`fn peer_closed_set_extra_pins`,
     line ~2422) — operates on `row.hw_protocol_override.unwrap_or_else(...)`, a
     resource-table row's own id, never a live connection's raw id (no table row is ever
     a `_CHx` id). **Safe, confirmed by direct reading — no change needed**, the same
     conclusion ADR-207 reached for `names.rs`'s structurally identical
     `row_needs_dynamic_pin_selection` (`names.rs:401`, which also calls
     `is_honda_diagh_protocol_id` on a row-derived id and is likewise safe for the same
     reason).
   - `names.rs`'s own `row_needs_dynamic_pin_selection` (line 401) — same row-derived-id
     reasoning as above. **Safe, no change needed.**
   - `comparam_id.rs`'s `ComParamId::for_base_protocol_id_if_known` (line ~171) — a
     non-ordinal ComParam-to-native-`CONFIG_*`-id translation block
     (`CP_P1Max`/`CP_P3Min`/`CP_P4Min` → `CONFIG_P1_MAX`/`_P3_MIN`/`_P4_MIN`) for a
     *connected link's* `hw_protocol_id`, explicitly checked against the raw id per its
     own doc comment ("Checked against the raw `hw_protocol_id`"). **A genuinely new gap
     class ADR-207 never hit** — UART Echo Byte and GM UART have no non-ordinal
     `comparam_id.rs` translation block of their own to compare against, and SAE J1939's
     analogous block (line ~193) happens to already be safe only because
     `is_j1939_protocol_id` was independently written range-inclusive from its own
     inception (predating `_CHx` support entirely, per that function's own doc comment),
     not because anyone deliberately widened it for this reason. Left as exact-match here,
     a `_CHx`-connected Honda DIAG-H link would silently lose its P1/P3/P4 native timing
     translation — `SetComParam`/the seeded Working defaults would stop reaching hardware
     for the `_CHx` route only, the same class of silent behavioral loss ADR-207's other
     three fixes closed. **Vulnerable — fixed here (Decision item 7), the fourth call site
     needing the new family-wide predicate.**

   All three vulnerable sites (plus the arm-gate bypass, a different mechanism) are fixed
   by Decision items 5–7 below, closing every gap this investigation found in one pass
   rather than iterating.
3. **The mock's own `is_honda_diagh_protocol` predicate** (`j2534-0404-mock/src/lib.rs`,
   line ~417) — confirmed pin-gating-only (`ChannelState::new`'s `pins_assigned`
   computation), the same purpose GM UART's/UART Echo Byte's own mock predicates serve.
   No family-wide behavioral enforcement exists in the mock for this protocol beyond that
   (no `ERR_ADDRESS_NOT_CLAIMED`-style state machine the way J1939 has). **No split
   needed**, mirroring ADR-207's own conclusion for UART Echo Byte.

Also confirmed directly (not assumed): Honda DIAG-H's clause 13 defines **no** Repeat
Messaging exclusion equivalent to UART Echo Byte's clause 12.3.3.1 — `rpc_misc.rs` has no
Honda-DIAG-H-specific Repeat Messaging rejection to begin with, so there is no
`is_uart_echo_byte_family_protocol_id`-shaped fourth-and-fifth call site here the way
ADR-207 had in `rpc_misc.rs`. The mock's own comment (`j2534-0404-mock/src/lib.rs`, near
line 3521) already documents this explicitly: Honda DIAG-H/J1708 "genuinely support
Repeat Messaging (clause 13/17 define no exclusion equivalent to clause 12.3.3.1's)".

**Vendor header naming: no inconsistency this time.** Unlike UART Echo Byte's
`PROTOCOL_ECHO_BYTE_CH1` (dropping the `UART_` prefix), Honda DIAG-H's `_CHx` block is
named consistently with its `_PS` id: `PROTOCOL_HONDA_DIAGH_CH1`..`PROTOCOL_HONDA_DIAGH_CH128`
(confirmed directly against `j2534-0404-sys/src/bindings/j2534_v0404.h`; `_PS` is
`0x0000800B`, the `_CHx` block is `0x00009680`..`0x000096FF`). No bindgen regeneration
needed — both already exist in the header and all five pre-committed target bindings;
only the `j2534-0404/src/lib.rs` re-export (Decision item 1) is new.

## Decision

1. **`j2534-0404/src/lib.rs` re-exports `PROTOCOL_HONDA_DIAGH_CH1`/`PROTOCOL_HONDA_DIAGH_CH128`**,
   mirroring the existing `PROTOCOL_GM_UART_CH1`/`_CH128` and `PROTOCOL_ECHO_BYTE_CH1`/`_CH128`
   pairs (same file, same shape) — `PROTOCOL_HONDA_DIAGH_PS` is already re-exported.
2. **`resources.rs`'s `chx_block_base` gains an eleventh entry**:
   `j2534_0404::PROTOCOL_HONDA_DIAGH_PS => Some(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1)` —
   keyed by its own `_PS`-only id, the same shape every other standalone family's entry
   uses (clause 13 defines no unqualified base id either).
3. **`chx_base_protocol_id`'s `BLOCKS` array gains an eleventh entry**:
   `(j2534_0404::PROTOCOL_HONDA_DIAGH_CH1, j2534_0404::PROTOCOL_HONDA_DIAGH_PS)`, widening
   `[(u32, u32); 10]` to `[(u32, u32); 11]`.
4. **`j2534-0404-mock/src/lib.rs`'s own crate-local duplicate tables**
   (`BASE_PROTOCOL_IDS`/`CHX_BLOCK_BASE_IDS`) get the matching eleventh entry — the same
   ADR-157 propagation duty ADR-206/ADR-207 already paid down for J1939/UART Echo Byte.
5. **`names.rs`'s Honda DIAG-H arm in `resolve_pin_selection` gains a
   `requested_index.is_some()` bypass**, mirroring GM UART's/SAE J1939's/UART Echo Byte's
   arms verbatim in shape: reject if `dlc_pin_data` is also non-empty (clause 7 channels
   have no J1962 pin concept, so the two are mutually exclusive), otherwise `return
   Ok(None)` so `resolve_channel_selection`'s own generic `_CHx` handling runs for the
   compound-name route instead. Without this, a compound `"...CH1"` name would reach this
   arm with `raw_hw_protocol_id == PROTOCOL_HONDA_DIAGH_PS` and empty `dlc_pin_data`, match
   the resource-table row, and return `Some(pin_select)` instead of `Ok(None)` —
   `resolve_channel_selection`'s own clause-6/clause-7 mutual-exclusion check would then
   reject the connect as combining Pin Selection with an Additional Channel. Found by
   up-front investigation this time (Context, above), not by a live failing-test repro —
   confirmed by a new test proving the compound-name route succeeds (see Test Plan).
6. **`resources.rs` gains a new `is_honda_diagh_family_protocol_id` predicate** (`_PS` id
   OR the full `HONDA_DIAGH_CH1..128` range, mirroring `is_gm_uart_protocol_id`'s/
   `is_uart_echo_byte_family_protocol_id`'s exact shape) for the three call sites that must
   treat a `_CHx` link identically to its `_PS` sibling: `connect_discovery_check`'s
   Discovery-gating arm (widened from a bare exact-match to
   `id if is_honda_diagh_family_protocol_id(id) => ...`, mirroring GM UART's/UART Echo
   Byte's own arms), `bustype_default_name_for_hw_protocol_id`, and `comparam_id.rs`'s
   `CP_P1Max`/`CP_P3Min`/`CP_P4Min` native-translation block (item 7). The narrower,
   arm-gate-only `is_honda_diagh_protocol_id` predicate is deliberately left unchanged and
   untouched at its own two safe call sites (`names.rs`'s arm gate and
   `row_needs_dynamic_pin_selection`, `resources.rs`'s `peer_closed_set_extra_pins`) —
   widening it instead of adding a second predicate would break `names.rs`'s own arm-gate
   purpose, which must NOT match a `_CHx` id.
7. **`comparam_id.rs`'s `CP_P1Max`/`CP_P3Min`/`CP_P4Min` → native-timing-`CONFIG_*`
   translation block is re-keyed on `is_honda_diagh_family_protocol_id`**, not the
   arm-gate-only predicate — so a `_CHx`-connected Honda DIAG-H link's P1/P3/P4 timing
   ComParams translate to native `SET_CONFIG` calls the same way its `_PS` sibling's
   already do. This is the fourth call site the up-front sweep in Context found, distinct
   in kind from `resources.rs`'s/`rpc_misc.rs`'s three call sites ADR-207 fixed for UART
   Echo Byte (this one lives in `comparam_id.rs`, not `resources.rs`/`rpc_misc.rs`, and
   the vulnerability shape — a live-link ComParam translation silently going inert for the
   `_CHx` route only — is new to this ADR).
8. **No Repeat Messaging call-site fix needed** (unlike ADR-207's two `rpc_misc.rs` sites)
   — confirmed directly that Honda DIAG-H's clause 13 defines no Repeat Messaging
   exclusion, so no such rejection exists in `rpc_misc.rs` to widen.
9. **Scope this PR to Honda DIAG-H alone**, not SAE J1708 in the same change — mirrors
   ADR-206 Decision item 6's/ADR-207 Decision item 9's own single-family-per-PR precedent.

## Alternatives rejected

- **Picking SAE J1708 instead**, since it is equally "ready now" per the backlog's own
  classification. Rejected only for ordering, not risk — Honda DIAG-H is the earlier of
  the two by phase number (10 vs. 11), the same tie-break ADR-207 used to pick UART Echo
  Byte (Phase 9) ahead of Honda DIAG-H/SAE J1708 (Phases 10/11) in that ADR's own round.
  SAE J1708 remains an equally valid next pick after this one.
- **Skipping the up-front `comparam_id.rs` sweep and iterating like ADR-207 did.**
  Rejected — ADR-207's own Consequences section explicitly frames its three-round history
  as the argument for doing the full sweep before implementation, not after; repeating
  that process here would have re-learned the same lesson a fourth time instead of using
  it.

## Consequences

- Honda DIAG-H `GetResourceIds`/`_CHx` clients are unlocked: a directly-named
  `PROTOCOL_HONDA_DIAGH_CH1`..`_CH128` id (or the equivalent compound `"...CH1"`-suffixed
  name) now resolves and connects, mirroring GM UART's/SAE J1939's/UART Echo Byte's own
  client-visible capability.
- `_CHx` Additional Channels now covers 11 of 18 protocol families.
- The backlog's remaining `_CHx` item (`implementation-notes.md`'s ADR-206/207-citing P2)
  is updated: Honda DIAG-H moves out of the "ready now" list (now one family: SAE J1708;
  TP2.0 stays in its own deprioritized note) into the closed set, alongside GM UART, SAE
  J1939, and UART Echo Byte.
- No new accepted-limitation residuals of this ADR's own — every gap the up-front sweep
  found (the `names.rs` bypass, the Discovery-gating/bustype-name/ComParam-translation
  trio) is fixed here, none deferred. The `implementation-notes.md` backlog entry is
  updated with guidance for whoever picks up SAE J1708 next: check `comparam_id.rs` for an
  analogous non-ordinal translation block keyed on that family's own narrow predicate, in
  addition to the `names.rs`-bypass and `resources.rs`/`rpc_misc.rs`-family-predicate
  checks ADR-206/207 already documented there.

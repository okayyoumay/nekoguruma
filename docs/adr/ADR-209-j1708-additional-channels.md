# ADR-209: SAE J2534-2 Additional Channels (_CHx) for SAE J1708

**Date:** 2026-09-02
**Status:** Accepted
**Affects:** `j2534-0404/src/lib.rs`, `j2534-0404-service/src/service/resources.rs`,
`j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_primitive.rs`,
`j2534-0404-service/src/service/rpc_misc.rs`,
`j2534-0404-mock/src/lib.rs`, `j2534-0404-service/tests/grpc_mock/j1708.rs`,
`docs/j2534-2-support-plan.md`, `j2534-0404-service/docs/implementation-notes.md`

## Context

[ADR-208](ADR-208-honda-diagh-additional-channels.md) closed Honda DIAG-H's own
`_CHx` gap and left two families in the "ready now, no design pass needed" group:
SAE J1708 (SAE J2534-2 clause 17, Phase 11/[ADR-175](ADR-175-j2534-2-j1708-phase11.md))
and TP2.0 (Phase 7) — TP2.0 stays deliberately deprioritized as the more
structurally complex of the two (a connection-oriented lifecycle needing its own
`design-advisor` consult at Phase-7 time). This ADR picks SAE J1708, the earlier
of the two remaining families by phase number, mirroring ADR-207's/ADR-208's own
"earliest position" tie-break.

**Following ADR-208's own established practice, this ADR's investigation swept
every raw-`hw_protocol_id` consumer of `is_j1708_protocol_id` up front, before
implementation, using the four-category checklist `implementation-notes.md`'s
backlog entry records from ADR-206/207/208's own history.** All four categories
were checked directly against the current code (not assumed transferred from a
prior family), with these findings:

1. **`names.rs`'s `resolve_pin_selection` J1708 arm** (`is_j1708_protocol_id`
   gate, `names.rs:1210`). The outer gate is already an exact `_PS`-only match —
   safe, the same shape UART Echo Byte's/Honda DIAG-H's arms have, not the
   J1939-shaped widened-gate problem. But the arm has **no
   `requested_index.is_some()` bypass at all** — the same missing-piece gap
   ADR-207/ADR-208 each found and fixed for their own families. Fixed here by
   Decision item 5 below.
2. **Every raw-`hw_protocol_id` consumer of `is_j1708_protocol_id`** (as opposed
   to a normalized `base_hw_protocol_id()`/`ChannelProtocol`-keyed consumer, or a
   resource-table-row-keyed consumer — both inherently safe, since
   `ChannelProtocol` has no separate `_CHx` variant and no table row is ever
   itself a `_CHx` id). A full crate-wide sweep found:
   - `resources.rs`'s `bustype_default_name_for_hw_protocol_id` — exact-match
     branch. **Vulnerable**, the same shape ADR-207/ADR-208 each fixed for their
     own families.
   - `rpc_primitive.rs`'s `apply_resolved_tx_flags` (its `is_j1708_protocol_id`
     gate ORing in `ComParamSet::msg_priority_tx_flags()`/`TX_FLAG_MSG_PRIORITY_VALUE`,
     ADR-175 Decision 6) — checked against a *connected link's* raw
     `hw_protocol_id`, the same parameter every one of this function's three
     TX-flags call sites (`CoptSendrecv`, tester-present, periodic-frame sends)
     already resolves against. **A genuinely new call-site file/mechanism ADR-207/
     ADR-208 never hit** — left on the narrow predicate, a `_CHx`-connected J1708
     link would silently lose its `CP_MessagePriority` TX flag on every send.
     **Vulnerable — fixed here (Decision item 7).**
   - `rpc_misc.rs`'s `ioctl_start_repeat_message` TxFlags composition (the
     `RepeatMsgData[0]` message's own local mirror of the
     `apply_resolved_tx_flags` gate above, since this composition site builds
     the actually-transmitted message independently of that shared helper) —
     same live-link raw `hw_protocol_id`. **Vulnerable — fixed here (Decision
     item 8).** Unlike UART Echo Byte's own `rpc_misc.rs` fixes (ADR-207 Decision
     item 10, which widened a REJECTION so a `_CHx` link is correctly excluded
     from Repeat Messaging the way its `_PS` sibling is), this is the opposite
     shape: clause 17 places **no** Repeat Messaging exclusion on J1708 at all
     (confirmed directly — no rejection arm exists for J1708 anywhere in
     `rpc_misc.rs`'s Repeat Messaging handling), so the fix here widens a flag
     *application*, not a rejection — a `_CHx`-connected J1708 link doing Repeat
     Messaging must still get its `MSG_PRIORITY_VALUE` flag, the same as its
     `_PS` sibling.
   - `names.rs`'s own `row_needs_dynamic_pin_selection` (line 402) — operates on
     `row.hw_protocol_override.unwrap_or_else(...)`, a resource-table row's own
     id, never a live connection's raw id. **Safe, no change needed**, the same
     conclusion ADR-208 reached for the structurally identical Honda DIAG-H call
     site.
3. **`connect_discovery_check`'s Discovery-gating arm** (`resources.rs`) — a
   bare exact-match arm on `PROTOCOL_J1708_PS`, the identical pre-fix shape
   UART Echo Byte's and Honda DIAG-H's own arms had before ADR-207/ADR-208.
   **Vulnerable — fixed here (Decision item 6).** (SAE J1708 is one of the six
   original ADR-185 Stage-1 families — SWCAN, FT-CAN, UART Echo Byte, Honda
   DIAG-H, J1708, Analog Inputs.)
4. **`comparam_id.rs`'s non-ordinal ComParam translation blocks** — confirmed
   directly (grep for "j1708"/`is_j1708_protocol_id` in that file) that **no
   J1708-specific translation block exists at all**. Unlike Honda DIAG-H (which
   had a genuinely new gap here, ADR-208 Decision item 7), J1708 has nothing to
   fix in `comparam_id.rs` — it mirrors UART Echo Byte's/GM UART's own "no block
   to begin with" case, not Honda DIAG-H's "block exists and needs re-keying"
   case. **No action needed.**

**The mock's own `is_j1708_protocol` predicate** (`j2534-0404-mock/src/lib.rs`)
— confirmed pin-gating-only (`ChannelState::new`'s `pins_assigned` computation),
the same purpose GM UART's/UART Echo Byte's/Honda DIAG-H's own mock predicates
serve. No family-wide behavioral enforcement exists in the mock for this
protocol beyond that. **No split needed.**

Also confirmed directly: SAE J1708's clause 17 places **no** Repeat Messaging
exclusion on this protocol (unlike UART Echo Byte's clause 12.3.3.1) — the
mock's own comment (`j2534-0404-mock/src/lib.rs`, near line 3536) already
documents this explicitly: "`PROTOCOL_HONDA_DIAGH_PS`/`PROTOCOL_J1708_PS` are
included here since both genuinely [support Repeat Messaging]". This is why
Decision item 8 (below) widens a flag-application site rather than adding a new
rejection the way ADR-207's `rpc_misc.rs` fixes did.

**Vendor header naming: no inconsistency this time**, matching Honda DIAG-H's
own precedent rather than UART Echo Byte's. `PROTOCOL_J1708_CH1`..
`PROTOCOL_J1708_CH128` (`0x00009780`..`0x000097FF`) is named consistently with
`PROTOCOL_J1708_PS` (`0x0000800D`), confirmed directly against
`j2534-0404-sys/src/bindings/j2534_v0404.h`. No bindgen regeneration needed —
both already exist in the header and all five pre-committed target bindings;
only the `j2534-0404/src/lib.rs` re-export (Decision item 1) is new.

## Decision

1. **`j2534-0404/src/lib.rs` re-exports `PROTOCOL_J1708_CH1`/`PROTOCOL_J1708_CH128`**,
   mirroring the existing `PROTOCOL_GM_UART_CH1`/`_CH128`,
   `PROTOCOL_ECHO_BYTE_CH1`/`_CH128`, and `PROTOCOL_HONDA_DIAGH_CH1`/`_CH128`
   pairs (same file, same shape) — `PROTOCOL_J1708_PS` is already re-exported.
2. **`resources.rs`'s `chx_block_base` gains a twelfth entry**:
   `j2534_0404::PROTOCOL_J1708_PS => Some(j2534_0404::PROTOCOL_J1708_CH1)` —
   keyed by its own `_PS`-only id, the same shape every other standalone
   family's entry uses (clause 17 defines no unqualified base id either).
3. **`chx_base_protocol_id`'s `BLOCKS` array gains a twelfth entry**:
   `(j2534_0404::PROTOCOL_J1708_CH1, j2534_0404::PROTOCOL_J1708_PS)`, widening
   `[(u32, u32); 11]` to `[(u32, u32); 12]`.
4. **`j2534-0404-mock/src/lib.rs`'s own crate-local duplicate tables**
   (`BASE_PROTOCOL_IDS`/`CHX_BLOCK_BASE_IDS`) get the matching twelfth entry —
   the same ADR-157 propagation duty ADR-206/207/208 already paid down for
   J1939/UART Echo Byte/Honda DIAG-H.
5. **`names.rs`'s J1708 arm in `resolve_pin_selection` gains a
   `requested_index.is_some()` bypass**, mirroring GM UART's/SAE J1939's/UART
   Echo Byte's/Honda DIAG-H's arms verbatim in shape: reject if `dlc_pin_data`
   is also non-empty (clause 7 channels have no J1962 pin concept, so the two
   are mutually exclusive), otherwise `return Ok(None)` so
   `resolve_channel_selection`'s own generic `_CHx` handling runs for the
   compound-name route instead.
6. **`resources.rs` gains a new `is_j1708_family_protocol_id` predicate**
   (`_PS` id OR the full `J1708_CH1..128` range, mirroring
   `is_gm_uart_protocol_id`'s/`is_honda_diagh_family_protocol_id`'s exact
   shape) for the three call sites that must treat a `_CHx` link identically
   to its `_PS` sibling: `connect_discovery_check`'s Discovery-gating arm
   (widened from a bare exact-match to
   `id if is_j1708_family_protocol_id(id) => ...`, mirroring GM UART's/UART
   Echo Byte's/Honda DIAG-H's own arms), `bustype_default_name_for_hw_protocol_id`,
   and (Decision items 7-8 below) `rpc_primitive.rs`'s/`rpc_misc.rs`'s own
   message-priority TX-flag gates. The narrower, arm-gate-only
   `is_j1708_protocol_id` predicate is deliberately left unchanged and
   untouched at its own two safe call sites (`names.rs`'s arm gate and
   `row_needs_dynamic_pin_selection`) — widening it instead of adding a
   second predicate would break `names.rs`'s own arm-gate purpose, which must
   NOT match a `_CHx` id.
7. **`rpc_primitive.rs`'s `apply_resolved_tx_flags` is re-keyed onto
   `is_j1708_family_protocol_id`**, so a `_CHx`-connected J1708 link's
   `CP_MessagePriority` ComParam correctly reaches `TX_FLAG_MSG_PRIORITY_VALUE`
   on every send (`CoptSendrecv`, tester-present, periodic-frame), the same as
   its `_PS` sibling already does. The fourth distinct call-site file this
   effort has now found needing this treatment (after `resources.rs`,
   `rpc_misc.rs`, and `comparam_id.rs` in prior rounds) — this is a new file,
   `rpc_primitive.rs`, not previously touched by ADR-206/207/208.
8. **`rpc_misc.rs`'s `ioctl_start_repeat_message` TxFlags composition is
   likewise re-keyed onto `is_j1708_family_protocol_id`** — its own local
   mirror of Decision item 7's gate (this composition site builds the
   actually-transmitted `RepeatMsgData[0]` message independently of the shared
   `apply_resolved_tx_flags` helper, the same reason it already has its own
   local SW-CAN gate). Unlike ADR-207's `rpc_misc.rs` fixes (which widened a
   *rejection*, since UART Echo Byte excludes Repeat Messaging entirely), this
   widens a flag *application* — clause 17 places no Repeat Messaging
   exclusion on J1708, confirmed directly, so a `_CHx`-connected link doing
   Repeat Messaging must still receive its `MSG_PRIORITY_VALUE` flag.
9. **No `comparam_id.rs` change** — confirmed directly that no J1708-specific
   ComParam translation block exists in that file at all, unlike Honda
   DIAG-H's own genuinely-new gap there (ADR-208 Decision item 7).
10. **Scope this PR to SAE J1708 alone**, not TP2.0 in the same change —
    mirrors ADR-206/207/208's own single-family-per-PR precedent.

## Alternatives rejected

- **Picking TP2.0 instead**, since it is the only other family left in the
  "ready now" group. Rejected — TP2.0 is the more structurally complex of the
  two remaining families (a connection-oriented lifecycle that needed its own
  `design-advisor` consult at Phase 7), so SAE J1708 remains the lower-risk
  pick, the same reasoning ADR-207/208 each used to defer TP2.0 in their own
  rounds.
- **Widening `is_j1708_protocol_id` itself instead of adding a second,
  family-wide predicate.** Rejected — the same reasoning ADR-207/ADR-208 both
  reached: widening the narrow predicate would break `names.rs`'s own
  arm-gate purpose, which must NOT match a `_CHx` id (that's precisely what
  lets a `_CHx` connect fall through to `resolve_channel_selection`'s generic
  handling in the first place).

## Consequences

- SAE J1708 `GetResourceIds`/`_CHx` clients are unlocked: a directly-named
  `PROTOCOL_J1708_CH1`..`_CH128` id (or the equivalent compound `"...CH1"`-
  suffixed name) now resolves and connects, mirroring GM UART's/SAE J1939's/
  UART Echo Byte's/Honda DIAG-H's own client-visible capability.
- `_CHx` Additional Channels now covers 12 of 18 protocol families.
- The backlog's remaining `_CHx` item (`implementation-notes.md`'s
  ADR-206/207/208-citing P2) is updated: SAE J1708 moves out of the "ready
  now" list (now only TP2.0 remains) into the closed set, alongside GM UART,
  SAE J1939, UART Echo Byte, and Honda DIAG-H.
- No new accepted-limitation residuals of this ADR's own — every gap the
  up-front sweep found (the `names.rs` bypass; the Discovery-gating/
  bustype-name/`rpc_primitive.rs`/`rpc_misc.rs` quartet) is fixed here, none
  deferred. The `implementation-notes.md` backlog entry is updated with
  guidance for whoever picks up TP2.0 next: `rpc_primitive.rs`'s
  `apply_resolved_tx_flags` is now a confirmed fifth location (after
  `resources.rs`, `rpc_misc.rs`, `comparam_id.rs`, and now this file) worth
  checking for a family-specific TX-flags gate keyed on the narrow predicate
  — TP2.0's own Repeat Messaging exclusion status and any comparable
  TX-flags gate should both be confirmed directly rather than assumed from
  either UART Echo Byte's (exclude) or J1708's (include, gate a flag) shape.

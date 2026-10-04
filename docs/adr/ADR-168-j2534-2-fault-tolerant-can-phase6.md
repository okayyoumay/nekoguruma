# ADR-168: SAE J2534-2 Fault-Tolerant CAN (ISO 11898-3) — Phase 6

**Date:** 2026-08-11
**Status:** Accepted (its deferred `LINK_FAULT` RxFlag-forwarding question decided against, not
             forwarded, by ADR-191; the Seventh correction's Accepted residual — the general
             clause 6 dual-wire pin-pair-completeness gap — resolved by ADR-201; Decision item 7's
             `FT_CAN_CHx`/`FT_ISO15765_CHx` Additional Channels deferral fulfilled by ADR-211)
**Affects:** `j2534-0404-service/src/service/resources.rs`, `names.rs`, `rpc_link.rs`, `comparam_defaults.rs`, `j2534-0404/src/lib.rs`, `j2534-0404-mock`, `docs/j2534-2-support-plan.md` (`events.rs` deliberately NOT touched — see Decision 4/Consequences)

## Context

`docs/j2534-2-support-plan.md`'s Phase 6 targets SAE J2534-2 clause 20
(Fault-Tolerant CAN, ISO 11898-3), one of the two CAN-family phases (with
SAE J1939, Phase 5) whose dependency chain (Phase 0-2) has already shipped.
Phase 5 needs a `design-advisor` session for its address-claim/defend state
machine; Phase 6's own up-front assessment is `design-advisor: No`, and the
`next-priority` skill picked it over Phase 5 on exactly that basis.

Clause 20's own text is unusually short: `FT_CAN_PS`/`FT_CAN_CHx` are
to be treated as the same as `CAN_PS`/`CAN_CHx` apart from the differences the clause names explicitly, and
likewise `FT_ISO15765_PS`/`FT_ISO15765_CHx` are equivalent to their base
`ISO15765` counterparts. The only differences the clause actually calls out:
no default pin is identified (the caller must select pins — the connector
offers two documented pin-pairs, pins 1/9 or pins 3/11 — before the physical
layer connects, same "connect leaves the link pin-unassigned" requirement
every other `_PS` family without a default has), one new `RxStatus` bit
(`LINK_FAULT`, bit 17 — the fault-tolerant transceiver detected a network
fault but still correctly received the frame), and a Discovery-support
requirement (already generic and implemented, Phase 1/ADR-153). No new
`SET_CONFIG`/ComParam translation is called out anywhere in clause 20,
unlike clause 9's `CP_ChangeSpeed*` requirement for SWCAN.

ISO 22900-2 models `ISO_11898_3_DWFTCAN` as its own bus type, structurally
parallel to `ISO_11898_2_DWCAN` (dual-wire) and `SAE_J2411_SWCAN`
(single-wire, ADR-164/Phase 4) — not a ComParam-inferred substitution the
way CAN FD (ADR-158) is. This is exactly the shape ADR-164 already worked
through for SWCAN: a `_PS`-only protocol family with no unqualified base
id that is *also* its own independently client-selectable D-PDU resource.
Phase 0 already added every FTCAN-related header constant in its one-pass
sweep (`PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`/`_CH1`..`_CH128`,
`RX_FLAG_LINK_FAULT`), `comparam_defaults::bustype_default_params` already
carries `ISO_11898_3_DWFTCAN`'s Table B.21 baudrate default, and
`ps_protocol_id()`'s own doc comment already named `FT_CAN`/`FT_ISO15765`
as an explicit ADR-156 accepted residual deferred to "a future phase" — this
is that phase.

## Decision

1. **Mirror ADR-164's resource-table pattern directly.** Add a new
   `BUSTYPE_ISO_11898_3_DWFTCAN` constant (`0x030A`, the next value after
   SWCAN's `0x0309`) and ten new resource-table rows (`0x0230`-`0x0239`, the
   next block after SWCAN's `0x0226`-`0x022F`) mirroring every existing
   dual-wire-CAN row (one raw-CAN row plus the same nine ISO15765-family
   variants SWCAN mirrors), each reusing its dual-wire sibling's
   `ChannelProtocol` identity unchanged, with a `_FTCAN`-suffixed
   `protocol_name` (avoiding ambiguity with the ten pre-existing dual-wire
   names, same reasoning as SWCAN's `_SWCAN` suffix) and
   `hw_protocol_override` set to `PROTOCOL_FT_CAN_PS`/`PROTOCOL_FT_ISO15765_PS`.
2. **Two-pin default, unlike SWCAN's single pin.** FTCAN is a genuine
   CAN-high/CAN-low differential pair (like DWCAN), not a single-wire
   protocol, so its rows carry a two-pin `dlc_pins` default: pins `(1, 9)`,
   the first-listed pin-pair in clause 20.2.1 — mirroring SWCAN's own
   precedent of picking the first-listed pin as a practical default despite
   the clause saying "no default identified." A caller wanting the second
   pin-pair (3/11) selects it via the existing typed-pin Pin Selection
   mechanism (clause 6, ADR-156) the same way any other non-default pin
   choice is expressed. `resolve_pin_selection` gets a new `is_ft_protocol_id()`
   check alongside its existing `is_sw_protocol_id()` one, so a connect on an
   FT link always issues the explicit `SET_CONFIG(CONFIG_J1962_PINS)` call
   regardless of whether the resolved pins match the row default — the
   physical layer stays unassigned until that call either way, per clause
   20.2.1's "no default pin" text.
3. **`ps_protocol_id()` is NOT touched** (correction — see below): FT ids
   are resolved entirely through their own dedicated resource-table rows
   (Decision 1's `hw_protocol_override`) and `resolve_pin_selection`'s
   dedicated FT branch (Decision 2), the same mechanism SWCAN/CAN FD
   already use. `ps_protocol_id()` only covers protocols reached by
   literally `_PS`-suffixing an existing base hardware id at a caller's
   direct request (its seven-entry match, ADR-156) — SW_CAN/SW_ISO15765/
   FD_CAN/FD_ISO15765 are not among those arms either, despite being fully
   implemented elsewhere, and its doc comment's "out of scope for this
   phase" list continues to name all of them, FT_CAN/FT_ISO15765 included,
   accurately: that list is a "no work needed in this specific function"
   note, not a "not yet implemented anywhere" one.
4. **`LINK_FAULT` forwarding is deferred, not implemented this phase**
   (correction during implementation — the original Decision 4 here assumed
   a bare `RX_STATUS_FLAGS_MASK` extension would suffice; it does not).
   `events.rs`'s existing forwarding path casts `msg.rx_status() &
   RX_STATUS_FLAGS_MASK` to `u8` before packing it into `rx_flag_bytes`'
   single flags byte — `LINK_FAULT` (bit 17) sits entirely outside `u8`'s
   range, so a mask-only extension would silently produce dead code, never
   actually forwarding the bit while looking implemented. This is the same
   already-known problem ADR-164's Consequences deferred for `SW_CAN_HV_RX`/
   `SW_CAN_HS_RX`/`SW_CAN_NS_RX` (RxStatus bits 16-18, `j2534-0404-service/docs/implementation-notes.md`'s
   P2 backlog entry) — `LINK_FAULT`'s bit 17 sits inside that exact
   already-deferred 16-18 range, so this is not a new residual but the same
   one, now touched by a second phase. See Consequences.
5. **No new ComParam translation work.** Clause 20 calls out no
   `SET_CONFIG` changes the way clause 9 did for SWCAN's `CP_ChangeSpeed*`
   family; an FT link uses its base `CAN`/`ISO15765` family's existing
   ComParam translations unchanged, with `bustype_default_params`'s
   already-seeded `ISO_11898_3_DWFTCAN` defaults supplying the bus-type-scoped
   baseline the same way every other bus type's own defaults do.
6. **`j2534-0404-mock` gains `is_ft_protocol()`**, mirroring `is_sw_protocol()`
   exactly: gates `pins_assigned` (unassigned until explicit `SET_CONFIG`,
   same as SW) and `ERR_NOT_SUPPORTED` when FTCAN isn't enabled on the mock
   module. No new state-machine behavior — `LINK_FAULT` uses the existing
   generic RxStatus-injection test helper, the same way SWCAN needed no
   dedicated mock simulation logic.
7. **FT Additional Channels (`_CHx`) are deferred**, matching every prior
   family's `_CHx` deferral (ADR-156 precedent, explicitly repeated for
   SWCAN in ADR-164).

## Consequences

- Accepted residuals, consistent with every prior CAN-family phase's own
  stated deferrals: FT Additional Channels (`_CHx`); `DEVICE_INFO_FT_CAN_*`/
  `DEVICE_INFO_FT_ISO15765_*` discovery-cache connect-time enforcement is
  superseded in part by [ADR-185](ADR-185-discovery-cache-connect-time-enforcement.md)
  Stage 1 (`DEVICE_INFO_FT_CAN_SUPPORTED`/`DEVICE_INFO_FT_ISO15765_SUPPORTED`
  are now consulted at connect time; their `_SIMULTANEOUS` companion bits
  remain unwired by both of ADR-185's stages, tracked in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog); and
  `LINK_FAULT` (RxStatus bit 17) forwarding into `RxFlag`
  (Decision 4 correction above) — folded into the same already-open P2
  backlog item ADR-164 opened for `SW_CAN_HV_RX`/`SW_CAN_HS_RX`/`SW_CAN_NS_RX`
  (bits 16-18), since `LINK_FAULT` needs the identical "which `RxFlag` byte
  carries bits 16-23" design question resolved, not a separate one.
- **Correction (implementation-time finding, not anticipated in the Decision
  above): an FD-on-FT substitution hazard does exist**, contradicting this
  ADR's original claim that it didn't. Giving FT links `base_protocol_id`
  arms (Decision 5, needed so `comparam_id::to_j2534_config_id` treats an
  FT link as CAN-family "for free") makes `rpc_link.rs::apply_fd_mode`'s
  `match base_protocol_id(...) { CAN => ..., ISO15765 => ... }` reachable
  for FT links too — without an explicit guard, staging CAN-FD ComParams on
  an FT-connected link would silently substitute it to
  `FD_CAN_PS`/`FD_ISO15765_PS`, the exact bug class ADR-156/157/164
  repeatedly fixed elsewhere. Fixed by adding explicit FT reject arms to
  `apply_fd_mode`, mirroring its pre-existing SWCAN reject arms exactly (see
  `staging_fd_comparams_on_an_ft_link_is_rejected_not_substituted` in
  `tests/grpc_mock/ft_can.rs` for the regression test). The code is correct;
  this ADR's original reasoning for why no guard was needed was wrong.
- **Second correction (edge-case-hunter finding, BLOCKING, fixed): the same
  `base_protocol_id` FT arms also exposed `GetResourceStatus` to a false
  "in use" report.** `rpc_link.rs`'s `matches_status_hw_id` closure has a
  guard (originally added for ADR-164's own "Bug 1" fix) that drops the
  normalized-`base` candidate from matching when the *queried* resource's
  hardware id is SW-family, so a query naming the plain dual-wire CAN
  resource correctly does not fall back to matching a connected SWCAN
  link's `base`. That guard checked only `is_sw_protocol_id(hw_id)`; this
  phase's `base_protocol_id` FT arms collapse `PROTOCOL_FT_CAN_PS`/
  `PROTOCOL_FT_ISO15765_PS` down to `CAN`/`ISO15765` the same way SWCAN's
  arms do, exposing the identical hazard to FT links, but the guard was not
  extended to also check `is_ft_protocol_id(hw_id)`. Result: querying the
  plain dual-wire CAN resource while only its FTCAN sibling was connected
  falsely reported bit 0 ("in use") set. Fixed by adding the missing
  `is_ft_protocol_id(hw_id)` check to the same guard; see
  `get_resource_status_does_not_let_a_connected_ft_can_link_occupy_its_dual_wire_sibling`
  and its reverse-direction sibling in `tests/grpc_mock/ft_can.rs`
  (mirroring `sw_can.rs`'s own pinned regression pair for the identical
  SWCAN hazard).
- **Third correction (Codex review round 2, P2, fixed): raw-protocol-id
  connects bypassed the "no default pin" requirement.** `resolve_pin_selection`'s
  FT arm (Decision 2) computed `raw_hw_protocol_id` from either a matched
  resource-table row's `hw_protocol_override` *or*, when no row matched
  (`resource_row: None` — a caller naming `FT_CAN_PS`/`FT_ISO15765_PS`
  directly, bypassing the resource table), the caller's own raw protocol
  value passed through unchanged. Both cases satisfied `is_ft_protocol_id`
  identically, so an empty `dlc_pin_data` silently assigned the table row's
  own default pin pair (`0x0000_0109`) even when no table row — and hence
  no real default — was actually involved, connecting a caller to wiring it
  never selected. Fixed by requiring `resource_row.is_some()` before
  applying the hardcoded default, rejecting with `invalid_argument`
  otherwise (`connecting_the_raw_ft_protocol_id_directly_without_pins_is_rejected`
  in `tests/grpc_mock/ft_can.rs`). **The identical, pre-existing bug in the
  SW arm** (not introduced by this PR, but the same review round surfaced
  it by analogy) was fixed the same way in the same commit, with a matching
  regression test (`connecting_the_raw_sw_protocol_id_directly_without_pins_is_rejected`
  in `tests/grpc_mock/sw_can.rs`).
- **Fourth correction (Codex review round 3, P2, fixed): the second
  correction's own fix broke raw-protocol-id `GetResourceStatus` queries.**
  `rpc_link.rs`'s query-side resolution has a pre-existing fallback (ADR-157
  Correction, predating this ADR) for a `ResourceId` that matches no
  resource-table row: it normalizes the raw id down to its base protocol
  via `resources::base_protocol_id`, discarding any qualifier. That
  normalization is correct for the *general* `_PS` case (a connected `_PS`
  link's own stored `protocol` is already the base identity, per ADR-157),
  but wrong for SW/FT specifically — the one family whose connected link's
  raw `hw_protocol_id` *is* the qualified id itself (the second correction
  above depends on exactly this distinction). Combined with the second
  correction's own guard (which refuses to match a connected SW/FT link's
  *normalized* candidate), a raw `FT_CAN_PS`/`FT_ISO15765_PS` `ResourceId`
  query that bypassed the table lost its FT identity before the guard ever
  ran: `status_hw_id`/`hardware_native_hw_id` collapsed to plain `CAN`/
  `ISO15765`, so a connected FT link was never found (falsely reported
  idle) while a plain dual-wire sibling link could satisfy the query
  instead. Fixed by preserving the raw id as the query candidate's own
  `hw_protocol_override` when it is SW/FT-family, giving it the same
  identity shape a real resource-table row's override already has. **The
  identical, pre-existing bug in the SW arm** (this same query-side
  fallback predates ADR-164 too) was fixed the same way in the same commit.
  See `get_resource_status_with_the_raw_ft_protocol_id_finds_a_connected_ft_link`
  and its sibling tests (both directions, both families) in
  `tests/grpc_mock/ft_can.rs`/`sw_can.rs`.
- **Fifth correction (edge-case-hunter, close-out round 4, test-only, fixed):
  the third and fourth corrections were never exercised together.** The
  raw-id rejection test (third correction) never supplies pins, so it never
  reaches a successful raw-bypass connect; the raw-id `GetResourceStatus`
  test (fourth correction) connects through the resource-table row, not the
  raw-bypass path. A dedicated close-out `edge-case-hunter` pass confirmed
  by manual repro (added and reverted) that the actual combination — a
  successful raw-bypass connect with explicit pins, then a `GetResourceStatus`
  query on that same raw id — already worked correctly; no code defect, only
  a coverage gap. Closed by
  `connecting_the_raw_ft_protocol_id_with_explicit_pins_then_querying_by_raw_id_finds_the_link`
  and its `sw_can.rs` counterpart.
- **Sixth correction (Codex review round 5, P2, fixed): `GetResourceIds`
  couldn't discover any `hw_protocol_override` row by its raw id.**
  `names.rs`'s `candidates_matching_protocol` (the `GetResourceIds`
  protocol-id/name filter) falls back to `row.protocol.j2534_protocol_id()`
  when no row matches by table value, but never consulted
  `row.hw_protocol_override` — so `GetResourceIds(protocol_id =
  PROTOCOL_FT_CAN_PS)` (or `PROTOCOL_FT_ISO15765_PS`) returned an empty
  list, even though those exact ids already work end to end for
  `CreateComLogicalLink`/`ConnectComLogicalLink`/`GetResourceStatus` (this
  phase's own tests). This is a pre-existing bug predating every
  `hw_protocol_override` row (SW/FD's `_PS` rows, the SCI configuration
  rows) that ADR-168's new FT rows also inherited, not something this phase
  introduced. Fixed by mirroring `legacy_bustype_hw_id`'s already-accepted
  pattern a few lines below it: the fallback now matches
  `hw_protocol_override.unwrap_or_else(|| protocol.j2534_protocol_id())`,
  a strict superset of the old behavior. The `ProtocolName` arm's own
  numeric-string fallback had the identical gap (reachable via a raw id
  parsed through `ChannelProtocol::from_raw`) and was fixed the same way.
  See the new `GetResourceIds`-by-raw-id tests in `tests/grpc_mock/ft_can.rs`
  and `sw_can.rs`.
- **Seventh correction (Codex review round 6/design-advisor consult, P2,
  fixed): the FT arm accepted pin selections clause 20.2.1 does not
  document.** Unlike the general clause 6 mechanism the FT arm otherwise
  shares with `CAN_PS`/`ISO15765_PS`/`FD_CAN_PS`/`FD_ISO15765_PS`, clause
  20.2.1 defines FT-CAN (ISO 11898-3) as connectable on exactly two
  pin-pairs — (1,HI)+(9,LOW) or (3,HI)+(11,LOW) — a closed set. With
  explicit `dlc_pin_data` supplied, `resolve_pin_selection`'s FT arm
  delegated straight to `compute_pin_select` with no further validation, so
  a single explicit pin (e.g. just pin 3/HI, packing `0x0000_0300` with a
  zero secondary byte — physically incomplete) or a mismatched pair (e.g.
  pin 1/HI + pin 11/LOW, packing `0x0000_010B` — well-formed per the
  generic pin-typing rules but not one of the two valid FT pairs) was
  silently accepted. Fixed by validating the packed `pin_select` result is
  exactly `0x0000_0109` or `0x0000_030B`, rejecting `invalid_argument`
  otherwise. Deliberately scoped to the FT arm only — design-advisor
  confirmed the general clause 6 mechanism (`CAN_PS`/`ISO15765_PS`/
  `FD_CAN_PS`/`FD_ISO15765_PS`) is out of scope for this fix; see the
  accepted residual below. See
  `connecting_an_ft_resource_with_a_single_explicit_pin_is_rejected` and
  `connecting_an_ft_resource_with_a_mismatched_pin_pair_is_rejected` in
  `tests/grpc_mock/ft_can.rs`.
- **Accepted residual (design-advisor consult, same round): the general
  clause 6 dual-wire pin-pair-completeness gap remains unfixed.**
  `CAN_PS`/`ISO15765_PS`/`FD_CAN_PS`/`FD_ISO15765_PS` still accept an
  incomplete (single-pin) or otherwise arbitrary pin selection with no
  completeness validation, the same class of gap the Seventh correction
  fixes for FT alone. A correct general fix needs a per-bus "secondary pin
  required" predicate that doesn't exist yet — K-line's own secondary pin
  is genuinely optional, so a naive "pin count == 2" check would
  incorrectly break `ISO9141_PS`/`ISO14230_PS`'s K-only connects. Deferred
  pending a real design decision at the time; resolved by
  [ADR-201](ADR-201-pin-selection-general-secondary-pin-validation.md),
  which adds the per-bus `secondary_pin_requirement` predicate this residual
  called for (see this ADR's own Status line).
- This phase closes with one open design question carried forward (the
  `RxFlag`-byte-widening problem above, already tracked since ADR-164) and
  no new one — it remains a direct, evidence-grounded mirror of ADR-164's
  already-accepted pattern, not a new decision requiring a `design-advisor`
  session.

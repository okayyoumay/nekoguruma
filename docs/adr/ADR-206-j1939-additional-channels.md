# ADR-206: SAE J2534-2 Clause 7 Additional Channels (`_CHx`) — Extend to SAE J1939

**Date:** 2026-08-31
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/resources.rs`,
`j2534-0404-service/src/service/names.rs`,
`docs/j2534-2-support-plan.md`, `j2534-0404-service/docs/implementation-notes.md`,
`j2534-0404-service/tests/grpc_mock/j1939.rs`

## Context

`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog (Codex
review, PR #114 round 14) recorded that SAE J2534-2 clause 7 Additional
Channels (`_CHx`) is wired (`resources::chx_block_base`/`chx_protocol_id`/
`chx_base_protocol_id`, ADR-156/Phase 2b) for only 8 of the 18 protocol
families the clause-7/26 `_CHx` region covers, and characterized extending it
to the other 10 as "themselves-independent, per-family work (no shared
blocker)". Investigating SAE J1939 specifically as the next family to add
confirmed that characterization for J1939 — but also surfaced that it does
**not** hold uniformly across all 10 deferred families, a distinction the
original backlog entry did not draw.

**J1939 is genuinely simple, and was already prepared for this exact
extension.** SAE J1939 (Phase 5/ADR-179) has its own `_PS`-only hardware id
(`PROTOCOL_J1939_PS`) with no unqualified base id and no relationship to any
other protocol family — `resources::base_protocol_id` already returns it
unchanged via its identity fallback (no explicit match arm needed, unlike
`CAN`/`ISO15765`'s many qualified variants). More importantly,
`is_j1939_protocol_id` (`resources.rs`) already recognizes the
`PROTOCOL_J1939_CH1..=PROTOCOL_J1939_CH128` range today, even though
`chx_base_protocol_id` does not — its own doc comment states this was
deliberate, so that `comparam_support::is_j1939_param`/
`comparam_id::to_j2534_config_id` and other downstream Plane B call sites
already gate a `_CHx`-qualified J1939 id correctly, in anticipation of a
future direct/legacy creation reaching one. Wiring `chx_block_base`/
`chx_base_protocol_id` is therefore the last remaining step, not the first —
this mirrors GM UART's (Phase 8/ADR-189) own precedent exactly: a standalone
protocol family, its own `_PS`-only id, no cross-family collapse.

**CAN FD and its four siblings in the CAN-family-collapse group are a
different shape, and are NOT ready for the same one-line treatment.**
Investigating CAN FD (`FD_CAN_PS`, Phase 3a/ADR-158) as the originally
suggested starting family surfaced a real interaction risk: `resources.rs`'s
`is_fd_protocol_id` is a strict two-value check (`PROTOCOL_FD_CAN_PS` /
`PROTOCOL_FD_ISO15765_PS` only), and `rpc_link.rs`'s
`J2534Service::apply_fd_mode` — the connect-time state machine that decides
whether a CAN/ISO15765-family link substitutes to its FD variant, and
rejects a handful of incompatible combinations (Single Wire CAN, Fault-
Tolerant CAN, software-ISO-TP, and a link with `channel_index.is_some()`) —
keys its own `CAN`/`ISO15765` match arms on `resources::base_protocol_id`.
Naively adding `PROTOCOL_FD_CAN_PS => Some(PROTOCOL_FD_CAN_CH1)` to
`chx_block_base` (mirroring GM UART's pattern) would make
`base_protocol_id(PROTOCOL_FD_CAN_CH1)` resolve to `PROTOCOL_FD_CAN_PS`
(not `CAN`), which — because `apply_fd_mode`'s match has no arm for
`PROTOCOL_FD_CAN_PS` itself — would silently skip `apply_fd_mode` entirely
for a directly-connected `FD_CAN_CH1` link. That avoids a wrong rejection,
but does **not** by itself guarantee every OTHER FD-behavior gate in the
codebase (TX size-range/padding validation, FD ComParam allow-listing, etc.)
recognizes such a link as FD-capable — unlike J1939's `is_j1939_protocol_id`,
`is_fd_protocol_id` was never written to recognize a `_CHx`-qualified id, and
whether a given call site composes through `base_protocol_id` first (which
WOULD resolve correctly once `chx_base_protocol_id` is extended) or checks
`link.hw_protocol_id` directly (which would NOT) needs a call-site-by-call-
site audit this investigation did not have scope to complete. The two Single
Wire CAN blocks (Phase 4/ADR-164) and two Fault-Tolerant CAN blocks (Phase
6/ADR-168) share the identical shape — `is_sw_protocol_id`/
`is_ft_protocol_id` are the same kind of strict `_PS`-only checks, consulted
by the same `apply_fd_mode` CAN-family match arms — so the same risk class
likely applies to them too, though this was not independently verified to
the same depth as CAN FD.

**A second, deliberately-built gate also had to change: `names.rs`'s J1939
arm.** `resolve_pin_selection`'s J1939 arm (Codex review, PR #72 round 13)
was written when J1939 `_CHx` was genuinely out of scope, and deliberately
widened its own gate from an exact-`_PS`-id match (every other standalone
protocol's own arm, e.g. GM UART's) to the broader
`resources::is_j1939_protocol_id` predicate specifically so it could
intercept a raw `_CHx` id and reject it with an accurate, unambiguous
message — otherwise such an id would have fallen through to
`resolve_channel_selection`'s own generic `_CHx` handling and hit a
misleading "family not supported" message keyed to the wrong reason (that
round's own commit message documents this exact false-rejection-reason bug
it fixed). `resolve_channel_selection` itself required no change — it
already resolves any family `chx_base_protocol_id` recognizes generically
(via `already_chx`/`chx_base_protocol_id`, `names.rs` ~line 1723/1802-1813),
including handling the compound-name-plus-pins mutual-exclusion rejection
uniformly for every family — so once `resources.rs`'s tables recognize
J1939 (Decision items 1-2 below), naming `PROTOCOL_J1939_CH1` directly (or
via the compound-name route) resolves correctly there. **Two existing tests
in `names.rs`'s own unit test module
(`resolve_channel_selection_rejects_a_directly_named_out_of_scope_chx_id`
and its `_when_not_opted_in` sibling) use `PROTOCOL_J1939_CH1` specifically
as their "out of scope family" example** — both need their example id
swapped to a family that remains out of scope after this ADR (e.g. a CAN-FD
`_CHx` id, itself confirmed still out of scope by this ADR's own Context
above).

## Decision

1. **`chx_block_base` gains a ninth entry**: `j2534_0404::PROTOCOL_J1939_PS
   => Some(j2534_0404::PROTOCOL_J1939_CH1)`, following the exact same
   `_PS`-keyed shape GM UART's own entry already established.
2. **`chx_base_protocol_id`'s `BLOCKS` array gains a ninth entry**:
   `(j2534_0404::PROTOCOL_J1939_CH1, j2534_0404::PROTOCOL_J1939_PS)`,
   widening `[(u32, u32); 8]` to `[(u32, u32); 9]`.
3. **No other `resources.rs` function changes.** `chx_protocol_id` derives
   from `chx_block_base` unchanged. `is_chx_protocol_id` already covers the
   full 18-family/19-block region (its range already extends to
   `PROTOCOL_FD_ISO15765_CH128`, so it already recognized a J1939 `_CHx` id
   as "in the `_CHx` vocabulary, opt-in-gated" before this ADR — this ADR
   only makes it additionally *resolvable*, not newly *recognized*).
   `is_j1939_protocol_id` and its downstream ComParam-gating consumers
   already handle the `_CHx` range (Context, above) — genuinely no change
   needed there, confirmed by that function's own doc comment predating
   this ADR.
4. **`names.rs`'s J1939 arm (`resolve_pin_selection`, ~line 1188) is
   restructured to mirror GM UART's own arm (~line 1334) exactly**: the
   outer gate narrows from the broad `resources::is_j1939_protocol_id(...)`
   predicate to an exact `raw_hw_protocol_id ==
   j2534_0404::PROTOCOL_J1939_PS` match, so a raw `_CHx` id no longer enters
   this arm at all — it falls through to `resolve_channel_selection`'s own
   generic `_CHx` handling instead, the same as every other in-scope
   family's directly-named `_CHx` id already does. The old broad
   `raw_hw_protocol_id != PROTOCOL_J1939_PS || requested_index.is_some()`
   rejection block is removed (its first disjunct is now unreachable given
   the narrowed outer gate; its `requested_index.is_some()` half is
   replaced, not deleted outright — see next). A new
   `requested_index.is_some()` bypass is added at the top of the narrowed
   arm, mirroring GM UART's own bypass verbatim in shape (including its own
   PR #98 round 3 fix: reject if `dlc_pin_data` is also non-empty — clause 7
   channels have no J1962 pin concept — otherwise `return Ok(None)` to let
   `resolve_channel_selection` handle the compound-name resolution). The
   rest of the arm (opt-in check, `dlc_pin_data.is_empty()` check,
   `compute_pin_select`) is unchanged.
5. **Two existing `names.rs` unit tests are updated**, not left pointing at
   an example that no longer holds:
   `resolve_channel_selection_rejects_a_directly_named_out_of_scope_chx_id`
   and its `_when_not_opted_in` sibling swap their `PROTOCOL_J1939_CH1`
   example id for a family that remains out of scope after this ADR (e.g. a
   CAN-FD `_CHx` id).
6. **Scope this PR to J1939 alone**, not all 10 remaining families in one
   change — mirrors Phase 8's own single-family-per-PR precedent for the
   same reason: each family's own resource-table/mock/test additions are
   independent work with no shared implementation, and (per Context)
   several of the remaining nine carry a genuine, unaudited design risk
   this PR's scope does not cover.
7. **Backlog entry split, not left as one undifferentiated item.** The
   consolidated Codex-review P2 backlog entry is corrected: J1939 is
   removed from its "other 10" list (now closed by this ADR), and a new,
   more precise note is added distinguishing the four remaining standalone
   families (UART Echo Byte, Honda DIAG-H, SAE J1708, TP2.0 — GM-UART-
   shaped, no design-advisor needed, ready the same way J1939 was) from the
   five CAN-family-collapse families (CAN FD, ISO15765-on-CAN-FD, Single
   Wire CAN, Fault-Tolerant CAN, Fault-Tolerant ISO15765 — needing a
   dedicated design/audit pass over `is_fd_protocol_id`/`is_sw_protocol_id`/
   `is_ft_protocol_id` and every downstream FD/SW/FT-behavior call site
   before any of them can safely gain `_CHx` support).

## Alternatives rejected

- **Implementing CAN FD `_CHx` support in this PR instead** (the originally
  suggested starting family). Rejected after investigation — see Context.
  Wiring `chx_block_base` alone risks shipping a channel that connects
  successfully but is silently NOT treated as FD-capable by whichever
  downstream gates check `is_fd_protocol_id(link.hw_protocol_id)` directly
  rather than composing through `base_protocol_id` first — a real,
  unaudited functional-correctness risk, not a cosmetic one. Needs its own
  design pass (an audit of every `is_fd_protocol_id` call site, and a
  decision on whether to widen that predicate's own definition to a range
  check the way `is_chx_protocol_id`/`is_j1939_protocol_id` already do, or
  compose every call site through `base_protocol_id` instead) before
  attempting.
- **Implementing all 10 remaining families in one PR.** Rejected — bundles
  five genuinely-ready families with five that need a design pass first,
  either delaying the ready ones or risking shipping the FD-interaction gap
  unaudited. Scoping to J1939 alone keeps this PR's risk to what was
  actually verified.

## Consequences

- J1939 `GetResourceIds`/`_CHx` clients are unlocked: a directly-named
  `PROTOCOL_J1939_CH1`..`_CH128` id (or the equivalent compound
  `"...CH1"`-suffixed name) now resolves and connects, mirroring GM UART's
  own client-visible capability.
- `_CHx` Additional Channels now covers 9 of 18 protocol families.
- The backlog's remaining `_CHx` item is corrected from one undifferentiated
  "10 families, no shared blocker" claim into two distinct groups (Decision
  item 7) — the four-family "ready now" group and the five-family
  "needs a design pass" group — so a future session does not repeat this
  investigation's CAN-FD detour before picking its own next family.
- **Accepted residual (`edge-case-hunter` finding against this PR's diff,
  verified by repro, pre-existing for GM UART since ADR-189/Phase 8 — not
  introduced by this ADR, only inherited by mirroring GM UART's own arm
  shape):** a directly-named `_CHx` id combined with non-empty
  `dlc_pin_data` no longer reaches either protocol's own
  `resolve_pin_selection` arm (the arm's exact-`_PS`-match gate lets the raw
  `_CHx` id fall through to the function's generic clause-6 fallback tail
  instead), which produces a misleading "this protocol has no clause 6 Pin
  Selection variant" rejection rather than `resolve_channel_selection`'s
  accurate "clause 6/7 mutually exclusive" message — the request is still
  correctly rejected with `InvalidArgument` either way, so this is
  message-accuracy only, not a functional defect. Not fixed here — recorded
  as a new P3 in `j2534-0404-service/docs/implementation-notes.md`'s
  Prioritized Backlog rather than patched as a one-family special case,
  since the same fix shape (composing the fallback's `ps_protocol_id` check
  through `is_chx_protocol_id` first) would apply to both GM UART and J1939
  at once.
- **Accepted limitation (Codex review, this PR): `CllCreateFlagRawMode` is
  not extended to any `_CHx` link, J1939's own new one included.**
  `rpc_link.rs`'s RawMode protocol allowlist checks `hw_protocol_id`
  against each family's bare `_PS`/base id exactly (e.g.
  `hw_protocol_id == PROTOCOL_J1939_PS`), so a directly-connected
  `PROTOCOL_J1939_CH1..128` link — now legal to connect at all, per this
  ADR — cannot also request RawMode; it fails with
  `PDU_ERR_ID_NOT_SUPPORTED`. Investigated directly against the actual
  ADR-196/ADR-198/ADR-200 text (not just the allowlist's own code comment,
  which paraphrases loosely enough to read either way): none of those three
  ADRs discusses `_CHx`/Additional Channels interaction with RawMode at
  all — the exact-id-only shape is not a considered, documented exclusion
  of `_CHx` specifically, just an artifact of how each entry happens to be
  written. This means the same gap already exists, identically, for every
  one of the 8 protocol families `_CHx` already supported before this ADR
  (CAN, ISO15765, ISO9141, ISO14230, J1850VPW, J1850PWM, SCI, GM UART) —
  RawMode has never been exercised against ANY `_CHx` link, for any family,
  and no test in this codebase does so either. Deliberately NOT fixed here:
  this is a cross-cutting RawMode-scope question spanning all 9 families,
  not something specific to J1939 or introduced by this ADR, and deciding
  it (does RawMode's existing TX/RX shim machinery, keyed on each family's
  logical protocol identity, actually behave correctly end-to-end for a
  `_CHx` link once allowed, for every family, or only for some) is its own
  design question outside this ADR's scope (per its own Decision item 6:
  scoped to J1939's `_CHx` connect/resolve path alone). Recorded as a new
  P3 in `implementation-notes.md`'s Prioritized Backlog.
- **Accepted limitation (Codex review, this PR): the mock's
  `IOCTL_GET_DEVICE_INFO` still reports `DEVICE_INFO_J1939_SUPPORTED`
  unsupported, so a discovery-first client cannot learn that J1939 `_CHx`
  channels now connect.** Verified this is pre-existing since Phase 5/
  ADR-179, not introduced or worsened by this ADR: the mock has NEVER had
  an `IOCTL_GET_DEVICE_INFO` arm for `DEVICE_INFO_J1939_SUPPORTED` at all
  (unlike GM UART's own dedicated arm, or the flat `chx_capacity`-packed
  group the other six CAN-family `_SUPPORTED` params share) — it falls
  through to `Supported = 0` by construction, and
  `j2534-0404-service/src/service/discovery.rs`'s own
  `device_info_reports_not_supported_for_a_j2534_2_only_capability` test
  pins exactly this as deliberate, pre-existing behavior. A second
  `discovery.rs` test confirms real J1939 connects (the already-shipped
  `_PS` one, same as this ADR's new `_CHx` one) never actually consult
  `enforce_discovery_capability`/`DEVICE_INFO_J1939_SUPPORTED` at
  connect time at all — that check is J1939-independent, unrelated
  machinery this ADR doesn't touch — which is why J1939 already
  successfully connects today despite Discovery reporting it unsupported.
  So the gap is real but purely advisory (a well-behaved discovery-first
  client would wrongly skip J1939 entirely, `_PS` included, not just the
  `_CHx` this ADR adds) and identical before and after this ADR. Wiring
  `DEVICE_INFO_J1939_SUPPORTED`/`_SIMULTANEOUS` into the mock (and
  correspondingly into `j2534-0404-service`'s own Discovery cache) is a
  real, separate Phase-5-level gap — it would also mean revisiting the two
  pinning tests above, which currently assert the opposite on purpose —
  out of scope for a single-family `_CHx`-extension ADR to take on
  unilaterally. Recorded as a new P3 in `implementation-notes.md`'s
  Prioritized Backlog.

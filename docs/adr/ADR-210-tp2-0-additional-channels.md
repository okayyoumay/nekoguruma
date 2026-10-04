# ADR-210: SAE J2534-2 Additional Channels (_CHx) for TP2.0

**Date:** 2026-09-02
**Status:** Accepted
**Affects:** `j2534-0404/src/lib.rs`, `j2534-0404-service/src/service/resources.rs`,
`j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_primitive.rs`,
`j2534-0404-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/rpc_link.rs`,
`j2534-0404-service/src/service/events.rs`, `j2534-0404-service/src/service/events_rx_routing.rs`,
`j2534-0404-service/src/service/comparam_id.rs`,
`j2534-0404-mock/src/lib.rs`, `j2534-0404-service/tests/grpc_mock/tp20.rs`,
`docs/j2534-2-support-plan.md`, `j2534-0404-service/docs/implementation-notes.md`

## Context

[ADR-209](ADR-209-j1708-additional-channels.md) closed SAE J1708's own `_CHx`
gap, leaving TP2.0 (SAE J2534-2 clause 19, Phase 7 Stage 7a/
[ADR-188](ADR-188-j2534-2-tp20-phase7-stage-7a-active-connections.md)) as the
last family in the "ready now, no design pass needed" group every prior ADR in
this series (ADR-206 through ADR-209) has deliberately deferred, flagged
consistently as the most structurally complex of the group — TP2.0 has a
genuine connection-oriented lifecycle (`IOCTL_REQUEST_CONNECTION`/teardown, up
to four simultaneous connections of which at most one may be passive,
broadcast/periodic re-trigger per ADR-190/192/193) unlike any of the five
families already closed.

**Given that flagged complexity, this ADR's up-front investigation went
further than ADR-208's/ADR-209's own checklist: beyond the five established
categories (names.rs arm; family-wide `resources.rs`/`rpc_misc.rs`/
`rpc_primitive.rs`/`comparam_id.rs` predicate gaps; `connect_discovery_check`;
Repeat Messaging shape; mock predicate), it explicitly investigated a sixth,
TP2.0-specific question: does the connection-lifecycle state machine
(`events_tp20_connection.rs`, TP2.0-specific code in `events.rs`/`rpc_link.rs`)
key any behavior off a raw `hw_protocol_id` in a way that would break for a
`_CHx`-connected link?** A dispatched investigation surfaced 19 candidate call
sites; **every one of those 19 was then independently re-verified directly
against the actual code** (not trusted at face value), which corrected three
false positives and found two additional genuine gaps the dispatched
investigation missed — a materially different, and more scrutinized, result
than simply accepting the report:

**Confirmed genuinely vulnerable (20 call sites, all keyed on a live link's or
live frame's raw, un-normalized `hw_protocol_id`/`protocol_id` — none derived
from a resource-table row or from `base_protocol_id()`/`ChannelProtocol`,
both of which are inherently `_CHx`-safe):**

- `resources.rs`: `connect_discovery_check`'s Discovery-gating arm (bare
  exact-match, the same pre-fix shape UART Echo Byte's/Honda DIAG-H's/SAE
  J1708's own arms had), `bustype_default_name_for_hw_protocol_id`.
- `comparam_id.rs`: the `PARAM_TP20_BROADCAST_INTERVAL` →
  `CONFIG_TP2_0_T_BR_INT` translation block (checked against the raw
  `hw_protocol_id` parameter, the same shape ADR-208's Honda DIAG-H gap had).
- `rpc_primitive.rs` (5 sites — the largest single-file count of any family in
  this series): `apply_resolved_tx_flags`'s own `tp20_is_broadcast`
  computation (a second TP2.0-specific gate in the same function ADR-209's
  J1708 fix already touched); `resolve_send_recv_tx`'s own, separate
  `tp20_is_broadcast` computation; and three `CP_TP20BroadcastAddress`
  out-of-range synchronous-rejection call sites (`CoptSendrecv`,
  `CoptStartcomm`'s optional-message path, `CoptStopcomm`'s optional-message
  path) sharing `validate_tp20_broadcast_address_range`.
- `rpc_misc.rs`: `ioctl_start_repeat_message`'s conditional Repeat Messaging
  rejection (fires only while `CP_TP20BroadcastAddress` is staged — a
  narrower shape than UART Echo Byte's blanket clause-12.3.3.1 exclusion, see
  Category E below).
- `rpc_link.rs` (4 sites, a genuinely new call-site *file* this series has not
  touched before): `DestroyComLogicalLink`'s and `DisconnectComLogicalLink`'s
  own paired best-effort-teardown/passive-disarm gates (two each) — each
  guards a best-effort `IOCTL_TEARDOWN_CONNECTION`/passive-listener-disarm
  call that runs when a CLL tears down while its physical channel survives.
  Left unwidened, a `_CHx`-connected TP2.0 CLL's own active or passive
  connection would never be torn down on disconnect/destroy, leaking the
  native connection slot — a more severe failure mode than any gap found in
  prior rounds of this series.
- `events.rs` (6 sites, another genuinely new call-site file): the
  `handle_start_comm` TP2.0 entry arm itself (skipping this arm entirely
  means `CoptStartcomm` never initiates the connection request at all for a
  `_CHx`-connected link); the TP2.0 `CONNECTION_ESTABLISHED`/`_LOST`
  indication-frame router (keyed on the incoming frame's own reported
  protocol id, `frame_protocol_id` — an adapter reports RX frames tagged
  with the connect-time protocol id, so a `_CHx`-connected link's own
  connection indications would never be recognized as TP2.0 frames at all);
  the TX-side broadcast-echo early-drop (same `frame_protocol_id`); and two
  pairs of TX-ID-drift-detection sites on the RX and TX dispatch paths
  (`l.hw_protocol_id`).
- `events_rx_routing.rs` (1 site, a genuinely new call-site file): the
  `unique_resp_ids` construction that builds a TP2.0 CLL's own
  `tp20_rx_id`/`tp20_tx_id`-keyed routing entry (or the deliberate
  unmatchable sentinel) — left unwidened, a `_CHx`-connected TP2.0 link would
  get the WRONG entry shape and never correctly receive its own connection's
  content frames.

**Confirmed safe, no change needed (the investigation's own three false
positives, corrected here, plus two already-narrow-by-design sites):**

- `comparam_support.rs:252`'s `is_protocol_param_translation_supported` check
  — operates on `protocol.j2534_protocol_id()`, a `ChannelProtocol` accessor.
  `ChannelProtocol` has no separate `_CHx` variant (confirmed:
  `ChannelProtocol::TP2_0_PS` self-identifies through `j2534_protocol_id()`
  regardless of which raw hardware id the link actually connected through),
  so this value never varies with `_CHx`/`_PS` — inherently safe. The
  dispatched investigation misclassified this as "raw value from link,"
  which it is not.
- `rpc_link.rs:3153`'s pass-all-filter-installation exclusion and
  `rpc_misc.rs:1554`'s `ioctl_clear_msg_filters` exclusion — both check
  `base_proto_id`, the result of `resources::base_protocol_id(...)`/
  `link.base_hw_protocol_id()`. Once this ADR's own Decision items 2-3 wire
  TP2.0 into `chx_block_base`/`chx_base_protocol_id`, `base_protocol_id()`'s
  own existing generic fallback (`other => chx_base_protocol_id(other)...`)
  automatically resolves a TP2.0 `_CHx` id back to `PROTOCOL_TP2_0_PS` — the
  identical "no new arm needed" mechanism every prior family's own `_PS`
  identity-fallback already relies on. **No widening needed at either site.**
  The dispatched investigation misclassified both as vulnerable.
- `comparam_id.rs`'s second TP2.0 block (`BIT_SAMPLE_POINT`/
  `SYNC_JUMP_WIDTH`, `matches!(j2534_protocol_id, CAN | PROTOCOL_TP2_0_PS)`)
  — `j2534_protocol_id` here is `resources::base_protocol_id(hw_protocol_id)`,
  the same normalized value as above. Safe for the identical reason; the
  dispatched investigation's "literal match" framing missed that the input
  is already normalized. **No widening needed.**
- `names.rs:404`'s `row_needs_dynamic_pin_selection` — resource-table-row-id
  keyed, never a live `_CHx` id (the established pattern every prior family
  in this series confirmed safe for the identical reason).
- `names.rs:1391`'s own TP2.0 arm-gate check — correctly stays narrow by
  design; widening it would break the arm-gate's own purpose (a `_CHx` id
  must NOT match here, so it falls through to `resolve_channel_selection`'s
  generic handling instead).

**Category F's own question — does the connection-lifecycle state machine
itself need widening — resolved as: the state machine's own internals
(`events_tp20_connection.rs`) are safe by construction** (no protocol-id
checks inside that module at all; connection tracking is implicitly scoped to
TP2.0 purely by being reachable only from `handle_start_comm`'s own TP2.0
arm), **but every one of its entry/integration points is vulnerable** — see
the `events.rs`/`rpc_link.rs` findings above. This is the sixth, TP2.0-unique
gap class this investigation was specifically dispatched to look for, and it
is real: unlike a missing ComParam translation or Discovery bit (silent
feature loss), a `_CHx`-connected TP2.0 link with these gaps unfixed would
never establish a connection at all (`handle_start_comm`'s own entry gate),
or would establish one but never correctly route its own indication/content
frames, or would leak its native connection slot on teardown — materially
more severe failure modes than any prior family's own pre-fix gaps.

**Category E — Repeat Messaging:** confirmed directly (not assumed from
either UART Echo Byte's exclude-shape or SAE J1708's include-shape) that TP2.0
takes a third shape neither prior family has: `ioctl_start_repeat_message`
does not categorically exclude TP2.0 (unlike UART Echo Byte's blanket clause
12.3.3.1 exclusion) and has no flag-application gate to widen (unlike J1708's
`CP_MessagePriority`/`MSG_PRIORITY_VALUE` gate) — instead, clause 19.3.2.2/
19.3.2.3's broadcast-framing incompatibility with Repeat Messaging is enforced
as a single *conditional* rejection (fires only while
`CP_TP20BroadcastAddress` is staged nonzero in Active), keyed on the narrow
predicate and needing the same family-wide widening as every other Category B
site.

**Mock predicate:** unlike every prior family's own mock predicate (pin-gating
only, correctly narrow), `is_tp2_0_protocol` in `j2534-0404-mock/src/lib.rs`
is consulted well beyond pin-gating — it drives the mock's own TP2.0-specific
`PassThruWriteMsgs`/`PassThruStartPeriodicMsg` validation (connection-bound
write size limits, broadcast address-range checks), which must be widened to
a family-wide `is_tp2_0_family_protocol` mirror to correctly validate a
`_CHx`-connected link's traffic the same way it already does for `_PS`. Its
own `pins_assigned` pin-gating use (`ChannelState::new`) correctly stays on
the narrow predicate, matching every other family's identical convention. The
mock's own `IOCTL_REQUEST_CONNECTION`/`IOCTL_TEARDOWN_CONNECTION` handlers
were confirmed NOT protocol-gated at all (they operate purely on
`channel_id`/the per-channel `pins_assigned` flag), so they need no change —
a `_CHx`-connected channel's `pins_assigned` already evaluates identically to
a `_PS` channel's (neither matches any narrow `_PS`-only predicate), so these
IOCTLs already behave correctly for both routes.

**Vendor header naming:** consistent with `_PS`, matching Honda DIAG-H's/SAE
J1708's own precedent rather than UART Echo Byte's. `PROTOCOL_TP2_0_CH1`..
`PROTOCOL_TP2_0_CH128` (`0x00009800`..`0x0000987F`) confirmed directly against
`j2534-0404-sys/src/bindings/j2534_v0404.h`; `_PS` is `0x0000800E`. No bindgen
regeneration needed — only the `j2534-0404/src/lib.rs` re-export is new.

## Decision

1. **`j2534-0404/src/lib.rs` re-exports `PROTOCOL_TP2_0_CH1`/`PROTOCOL_TP2_0_CH128`**,
   mirroring the existing `_CH1`/`_CH128` pairs for every prior standalone
   family — `PROTOCOL_TP2_0_PS` is already re-exported.
2. **`resources.rs`'s `chx_block_base` gains a thirteenth entry**:
   `j2534_0404::PROTOCOL_TP2_0_PS => Some(j2534_0404::PROTOCOL_TP2_0_CH1)`.
3. **`chx_base_protocol_id`'s `BLOCKS` array gains a thirteenth entry**:
   `(j2534_0404::PROTOCOL_TP2_0_CH1, j2534_0404::PROTOCOL_TP2_0_PS)`, widening
   `[(u32, u32); 12]` to `[(u32, u32); 13]`.
4. **`j2534-0404-mock/src/lib.rs`'s own crate-local duplicate tables**
   (`BASE_PROTOCOL_IDS`/`CHX_BLOCK_BASE_IDS`) get the matching thirteenth
   entry.
5. **`names.rs`'s TP2.0 arm in `resolve_pin_selection` gains a
   `requested_index.is_some()` bypass**, mirroring every prior family's own
   bypass verbatim in shape.
6. **`resources.rs` gains a new `is_tp2_0_family_protocol_id` predicate**
   (`_PS` id OR the full `TP2_0_CH1..128` range) for the twenty call sites
   confirmed vulnerable, spanning seven files: `resources.rs` (2 —
   `connect_discovery_check`, `bustype_default_name_for_hw_protocol_id`),
   `comparam_id.rs` (1), `rpc_primitive.rs` (5), `rpc_misc.rs` (1),
   `rpc_link.rs` (4), `events.rs` (6), `events_rx_routing.rs` (1) — see the
   precise per-site list in Context above. The narrower, arm-gate-only
   `is_tp2_0_protocol_id` predicate stays unchanged at its own two safe call
   sites (`names.rs`'s arm gate and `row_needs_dynamic_pin_selection`).
7. **`resources.rs`'s `connect_discovery_check` and
   `bustype_default_name_for_hw_protocol_id` are re-keyed** onto the new
   family-wide predicate.
8. **`comparam_id.rs`'s `PARAM_TP20_BROADCAST_INTERVAL` translation block is
   re-keyed**; its own `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` block is left
   unchanged (Context: already safe via `base_protocol_id()` normalization).
9. **`rpc_primitive.rs`'s five sites are re-keyed**: both `tp20_is_broadcast`
   computations (`apply_resolved_tx_flags`, `resolve_send_recv_tx`) and all
   three `validate_tp20_broadcast_address_range` call-site gates
   (`CoptSendrecv`, `CoptStartcomm`'s optional message,
   `CoptStopcomm`'s optional message).
10. **`rpc_misc.rs`'s conditional Repeat Messaging rejection is re-keyed.**
11. **`rpc_link.rs`'s four teardown/disarm gates are re-keyed** (both
    `DestroyComLogicalLink`'s and `DisconnectComLogicalLink`'s own
    active-connection-teardown and passive-listener-disarm pairs) — the
    highest-severity fix in this ADR, closing a native-connection-slot leak
    for any `_CHx`-connected TP2.0 CLL that tears down while its channel
    survives.
12. **`events.rs`'s six sites are re-keyed**: `handle_start_comm`'s own TP2.0
    entry arm (without this, a `_CHx`-connected link's `CoptStartcomm` never
    initiates a connection request at all); the `CONNECTION_ESTABLISHED`/
    `_LOST` indication-frame router; the TX-side broadcast-echo early-drop;
    and both TX-ID-drift-detection site pairs (RX and TX dispatch paths).
13. **`events_rx_routing.rs`'s `unique_resp_ids`-construction site is
    re-keyed**, so a `_CHx`-connected TP2.0 CLL gets its own real
    `tp20_rx_id`/`tp20_tx_id`-keyed routing entry instead of an incorrect
    fallback shape.
14. **`j2534-0404-mock/src/lib.rs` gains its own new `is_tp2_0_family_protocol`
    predicate**, applied to its `PassThruWriteMsgs`/`PassThruStartPeriodicMsg`
    validation gates (four call sites) — the first time in this series a
    mock predicate needs widening beyond simple table-array entries, since
    unlike every prior family, TP2.0's mock predicate drives real protocol
    validation, not just pin-gating. `ChannelState::new`'s own
    `pins_assigned` use of the narrow predicate is left unchanged (correct
    by the same reasoning as `names.rs`'s arm gate).
15. **No change to the mock's `IOCTL_REQUEST_CONNECTION`/
    `IOCTL_TEARDOWN_CONNECTION` handlers** — confirmed directly they are not
    protocol-gated at all, operating purely on `channel_id`/the per-channel
    `pins_assigned` flag, which already evaluates correctly for both `_PS`-
    and `_CHx`-connected channels.
16. **This is the final family in the "ready now, no design pass needed"
    group** ADR-206 first identified — after this ADR, every family listed
    there as needing "a dedicated design/audit pass first" (CAN FD,
    ISO15765-on-CAN-FD, Single Wire CAN, Fault-Tolerant CAN,
    Fault-Tolerant ISO15765) remains the only unclosed `_CHx` work, and per
    ADR-206's own investigation, genuinely requires a `design-advisor`
    consult before proceeding (a call-site-by-call-site audit of
    `is_fd_protocol_id`/`is_sw_protocol_id`/`is_ft_protocol_id` plus
    `apply_fd_mode`'s own `base_protocol_id`-keyed state machine).

## Alternatives rejected

- **Trusting the dispatched investigation's 19-site report verbatim.**
  Rejected — independent re-verification of every single site (not a sample)
  found three false positives (`comparam_support.rs`, `rpc_link.rs:3153`,
  `comparam_id.rs`'s second block, plus `rpc_misc.rs:1554`\* — four, not
  three; corrected here) and two missed genuine sites
  (`events.rs:4483`, `events_rx_routing.rs:493`). Given this ADR's own
  Decision items directly gate a connection-lifecycle native-resource leak
  (item 11) and connection-establishment failure (item 12's `handle_start_comm`
  entry gate), an unverified report was not an acceptable basis for the fix
  list — every one of the 20 final sites was read directly, not inferred.

  \*(Four false positives total across the investigation's original
  classifications: `comparam_support.rs:252`, `rpc_link.rs:3153`,
  `rpc_misc.rs:1554`, and `comparam_id.rs`'s `BIT_SAMPLE_POINT`/
  `SYNC_JUMP_WIDTH` block — all four share the same root cause, a value
  already normalized via `base_protocol_id()`/`ChannelProtocol`, which the
  investigation did not distinguish from a genuinely raw value.)
- **Escalating to `design-advisor` given the size of this change.** Rejected
  — every one of the 20 fixes applies the identical, already-established
  principle (a `_CHx`-connected link must be treated identically to its `_PS`
  sibling for family-wide behavioral checks) this series has applied five
  times already; TP2.0 simply has more call sites because its architecture
  (a genuine connection lifecycle) has more surface area than any prior
  family, not because any new design judgment is required. The CAN-family-
  collapse group (item 16 above) is the next work that genuinely needs
  `design-advisor`, not this one.

## Consequences

- TP2.0 `GetResourceIds`/`_CHx` clients are unlocked: a directly-named
  `PROTOCOL_TP2_0_CH1`..`_CH128` id (or the equivalent compound
  `"...CH1"`-suffixed name) now resolves, connects, establishes/tears down
  connections correctly, and routes its own indication/content frames
  correctly — mirroring every other in-scope family's own client-visible
  capability, but requiring materially more fixed call sites to get there
  given TP2.0's own architectural complexity.
- `_CHx` Additional Channels now covers 13 of 18 protocol families — **the
  last of the six "ready now, no design pass needed" families this series
  set out to close** (ADR-206 through ADR-210). The five remaining
  out-of-scope families (CAN FD, ISO15765-on-CAN-FD, Single Wire CAN,
  Fault-Tolerant CAN, Fault-Tolerant ISO15765) all need a `design-advisor`
  consult first, per ADR-206's own original investigation.
- No new accepted-limitation residuals of this ADR's own — every gap the
  up-front investigation found (independently re-verified, not merely
  trusted) is fixed here, none deferred.
- The `implementation-notes.md` backlog's `_CHx` P2 entry closes out entirely
  as a "ready now" tracker (nothing left in that group); its own text is
  updated to note the remaining five out-of-scope families now form the
  entry's sole remaining content, with a pointer to this ADR's own
  investigation as the concrete example of what a `design-advisor`-consulted
  fix for one of them should expect in scope (a connection-lifecycle-shaped
  family, if any of the five turns out to have comparable stateful
  complexity once actually investigated).

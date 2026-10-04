# ADR-157: `hw_protocol_id` Three-Plane Normalization for `_PS` Links

**Date:** 2026-07-31
**Status:** Accepted (corrects Phase 2a's implementation of ADR-156 Decision 2; ADR-156 itself is unchanged, its Status line is annotated to point here. Decision item's `to_j2534_config_id`/`expand_tidle`-callers-normalize mechanism: the `to_j2534_config_id` half is narrowly superseded by ADR-158 — that function's contract changed from "always keyed by the normalized base id" to "keyed by the raw Plane A id, with an internal FD exception" for SAE J2534-2 clause 21 CAN FD; `expand_tidle`'s own base-id keying is unaffected)
**Affects:**
- `j2534-0404-service/src/service/resources.rs` (new `base_protocol_id`)
- `j2534-0404-service/src/service.rs` (new `LogicalLinkState::base_hw_protocol_id`)
- `j2534-0404-service/src/service/comparam_id.rs` (`to_j2534_config_id`, `expand_tidle` callers normalize)
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/src/service/rpc_primitive.rs`
- `j2534-0404-service/src/service/events.rs`
- `j2534-0404-service/src/service/tx_header.rs`
- `j2534-0404-mock/src/lib.rs`

## Context

ADR-156 Decision 2 resolves a Pin Selection connect to a SAE J2534-2 `_PS`
protocol variant id (e.g. `PROTOCOL_CAN_PS`, a real numeric value distinct
from base `CAN`) and stores it in `LogicalLinkState.hw_protocol_id`. Per
Decision 1, no new `ChannelProtocol` variant was added for `_PS` —
`LogicalLinkState.protocol: ChannelProtocol` stays at its pre-Phase-2a value
for a `_PS` link.

Before Phase 2a, `hw_protocol_id` and `protocol.j2534_protocol_id()` were
always equal (one documented exception: software-ISO-TP mode, ADR-046), so
it was safe — and common practice throughout this crate — for a call site to
branch on either interchangeably via a direct `hw_protocol_id == <literal>`
comparison. Phase 2a's implementation broke that invariant without auditing
every site that relied on it. A post-implementation review (edge-case-hunter,
confirmed by two independent code-scout sweeps) found roughly 30 call sites
across `j2534-0404-service` and `j2534-0404-mock` that literal-compare a raw
protocol id (or `ChannelProtocol::from_raw` of one) against base J2534-1
constants to decide protocol-family-specific behavior — every one of them
silently stops recognizing a `_PS` id as belonging to its base family. Concrete
consequences confirmed before this fix: every protocol-specific ComParam
becomes unsupported for a `_PS` link (never reaches hardware); a pass-all
filter gets illegally installed on an `ISO15765_PS` channel (violates
ADR-048); the ADR-038 ban on `PDU_IOCTL_START_MSG_FILTER` for ISO15765 links
is bypassed for `ISO15765_PS`; `GetResourceStatus`/lock-holder checks can't
see a `_PS` link as occupying its base protocol's resource; the mock's
`IOCTL_GET_PROTOCOL_INFO` and K-line init IOCTLs reject `_PS` ids outright.

## Decision

Every read of `hw_protocol_id` (or a raw `u32` threaded from one, or a
`ChannelProtocol::from_raw` of one) is classified into exactly one of three
planes, and only one plane needs to change:

- **Plane A (hardware-facing) — keep the raw `_PS` id, unchanged.** The
  actual `PassThruConnect` argument and `PASSTHRU_MSG.ProtocolID` on
  read/write/filter messages: these must match the literal id the channel
  was actually opened with.
- **Plane B (behavior/family decision) — normalize to the base protocol id
  before use.** Every ComParam-support check, filter-install-type decision,
  protocol-family gate (ISO15765-specific logic, J1850 header format,
  K-line-only IOCTLs, `select_init_sequence`, timing-parameter derivation,
  `GetResourceStatus`/lock-holder occupancy), and any `ChannelProtocol`
  derived via `from_raw` of a raw protocol id for a family check.
- **Plane C (physical-resource identity) — keep the raw `_PS` id,
  unchanged.** `ChannelKey` construction/lookup and
  `find_physical_lock_holder`-style channel-sharing/locking checks: these
  correctly use the literal id including its `_PS` distinction, since
  `ChannelKey`'s `pin_select` component (ADR-156 Decision 2) already handles
  sharing/separation correctly on its own.

**Normalization primitives** (both are pure identity for every non-`_PS`
value, so every existing non-`_PS` path is provably unchanged):

- `resources::base_protocol_id(hw_protocol_id: u32) -> u32` — the exact
  inverse of `ps_protocol_id` for the six one-to-one `_PS` ids, identity
  otherwise. `PROTOCOL_J2610_PS` collapses to one representative SCI id
  (mirroring `ps_protocol_id`'s own forward collapse of all four SCI ids
  onto one `_PS` id) — every Plane B consumer of this free function gates
  identically across all four SCI variants, so the collapse loses no
  distinction that matters to it.
- `LogicalLinkState::base_hw_protocol_id(&self) -> u32` — `self.protocol
  .j2534_protocol_id()` when `self.pin_select.is_some()`, else
  `self.hw_protocol_id` directly. Used at every Plane B site that has a
  `LogicalLinkState` in scope; unlike the free function above, this
  preserves the exact SCI variant (via `protocol`'s existing
  `hw_protocol_override`-aware identity) for the one `_PS` family
  (`SAE_J2610`) where the free function's collapse would otherwise lose it.

Fix every confirmed Plane B site (`comparam_id.rs::to_j2534_config_id` and
`::expand_tidle`, normalized at their callers rather than by changing their
`u32` signatures — normalizing at the call site keeps both functions correct
for software-ISO-TP's raw-CAN-channel-with-ISO15765-service-protocol case,
where the two would otherwise disagree with what the hardware channel
actually is; `can_filter_tx_flags`, `j1850_header_bytes`,
`select_init_sequence`, `timing_protocol_id` capture, the pass-all-filter /
ISO15765 FC-filter / `SetUniqueRespIdTable` rebuild / `PDU_IOCTL
_START_MSG_FILTER` ban / `LOCK_PHYSICAL_COM_PARAMS` / `CLEAR_MSG_FILTERS`
gates, every `ChannelProtocol::from_raw(link.hw_protocol_id)` call in
`rpc_link.rs`/`rpc_primitive.rs`, and the `GetResourceStatus`/lock-holder
in-use checks) to use the appropriate normalization primitive. Fix the
mock's four confirmed-broken IOCTL/response-queue gates
(`IOCTL_GET_PROTOCOL_INFO`'s protocol-id validation, `IOCTL_FIVE_BAUD_INIT`
/`IOCTL_FAST_INIT`'s K-line-protocol checks, the J1850-flavor response-queue
match) with the mock's own local base-protocol mapping (the mock has no
`LogicalLinkState`/`ChannelProtocol` concept — it needs its own small,
crate-local inverse of its existing `PS_PROTOCOL_IDS`/`is_ps_protocol`
table, mirroring the service's `base_protocol_id` in spirit, not shared code
across the crate boundary).

**`GetResourceStatus`/conflict-detection visibility (item 6): a `_PS` link
counts as occupying its base protocol's resource.** A `CAN_PS` link's
`protocol` field is `ChannelProtocol::CAN`, so the resource-candidate lookup
(keyed on `ChannelProtocol`) already reports it as a candidate for the plain
`CAN` resource id; the in-use/lock-holder checks must agree using the same
normalized identity, or the response is internally inconsistent (a resource
status describing a resource other than the one its own `resource_id`
names) — a consistency rule this codebase already documents elsewhere
(`rpc_link.rs`'s existing comment on `resource_status`/`resource_id`
agreement). `ChannelKey`'s `pin_select` component still independently
governs whether two `_PS` links *share a physical channel* (Decision 2's
same-pins-vs-different-pins rule, Plane C, unaffected by this).

**Accepted residual: `_PS` links are hardware-ISO-TP only.** Software-ISO-TP
/ Auto mode (ADR-046) assumes a raw CAN hardware channel underneath an
ISO15765-family service protocol; an `ISO15765_PS` link's hardware channel
is the `_PS` variant, not raw CAN, and reconciling the two modes is out of
scope for this fix. The CAN dual-channel-mode auto-probe and the software
fallback path are both skipped when `pin_select.is_some()` — an `ISO15765_PS`
link always uses hardware ISO-TP. Revisit if/when a future phase needs
software-ISO-TP Pin Selection specifically.

## Consequences

- Every non-`_PS` link (`pin_select == None`, true for every link that
  existed before Phase 2a and every link Phase 2a itself creates for a
  default-pins request) gets `base_hw_protocol_id() == hw_protocol_id`
  and `base_protocol_id(x) == x` — bit-for-bit identity with pre-Phase-2a
  behavior at every fixed site. This is the regression-containment argument:
  nothing about this fix can change behavior for a link that isn't a `_PS`
  link.
- `J2610_PS`'s four-SCI-variant collapse in the free `base_protocol_id`
  function is a deliberate, narrow simplification — documented at its
  definition — not a general precedent; a future `_PS` family that needs
  per-variant Plane B fidelity should use the `LogicalLinkState`-level
  accessor instead.

**Correction (found by Codex review, PR #28, before merge):** the
`LogicalLinkState::base_hw_protocol_id()` accessor's first implementation
derived a `_PS` link's base id via `self.protocol.j2534_protocol_id()`,
believing this preserved the exact SCI variant per ADR-023's `ChannelProtocol`
encoding. It did not: `ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI
.j2534_protocol_id()` returns the shared `SCI_MODE` quirk value (ADR-023's
own documented behavior), not the connecting resource row's specific
`hw_protocol_override` (`SCI_A_ENGINE`/`SCI_A_TRANS`/`SCI_B_ENGINE`/
`SCI_B_TRANS`) — so a pin-selected SCI connect silently dropped its `T1_MAX`
-`T5_MAX` timing ComParams via the exact `to_j2534_config_id` failure mode
this whole ADR exists to fix, missed for this one family. Fixed by having
`names::resolve_pin_selection` return its own already-correctly-resolved
base id (which does consult `hw_protocol_override`) alongside `ps_protocol_id`
/`pin_select`, captured into a new `LogicalLinkState.pin_select_base
_hw_protocol_id: Option<u32>` field at `CreateComLogicalLink` time;
`base_hw_protocol_id()` now reads that field directly rather than
re-deriving it from `protocol.j2534_protocol_id()`.

A second, related bug found in the same review round: `rpc_link.rs`'s
`autodetect_sae_j1850_flavor` (the SAE_J1850 VPW/PWM probe, ADR-070) writes
`hw_protocol_id` directly from its detected base flavor at connect time,
unconditionally — for a pin-selected bus-agnostic J1850 link, this silently
overwrote the `_PS` id set at Create time with the base id while
`pin_select` stayed set, so the connect path opened a non-`_PS` channel and
then attempted `CONFIG_J1962_PINS` on it, which the mock (and real
hardware) rejects as `ERR_CHANNEL_IN_USE` — a pin-selected bus-agnostic
J1850 connect failed outright. Fixed by mapping the detected flavor back
through `resources::ps_protocol_id` before the write, when `pin_select.is_some()`.

**Correction (found by Codex review, PR #28, a subsequent round, before
merge):** two more bugs in the same area, both a direct consequence of the
first Correction above. (1) `rpc_link.rs`'s `GetResourceStatus`/resource-
conflict candidate match (the `active_candidate` search) compared a
resource row's `hw_override` against the link's raw `hw_protocol_id` —
for a pin-selected SCI link, `hw_protocol_id` holds the consolidated
`PROTOCOL_J2610_PS`, never the specific `SCI_A_*`/`SCI_B_*` override, so
this comparison could never match and an ambiguous `SAE_J2610_SCI` query
silently fell back to the wrong row. This is a *different* comparison
shape than this ADR's original ~30-site sweep targeted (a resource row's
override compared against a link, not a literal base-protocol-constant
comparison), which is why it was missed until this round; fixed by using
`link.base_hw_protocol_id()` (now correct, per the first Correction
above) instead of the raw field. (2) `probe_sae_j1850_flavor` (called by
`autodetect_sae_j1850_flavor`) always probed the SAE_J1850 bus on its
*default* wiring, even for a pin-selected connect, since it never applied
`CONFIG_J1962_PINS` to its candidate channels — a bus reachable only via
the caller's selected pins could be missed entirely and the wrong flavor
detected. Fixed by threading `pin_select` into the probe: a pin-selected
probe connects via the `_PS` candidate id and applies `CONFIG_J1962_PINS`
before reading, and the module-wide `j1850_bus_flavor` cache (a single
global slot) is bypassed entirely for a pin-selected probe, since a
different pin selection can represent a genuinely different physical bus
segment — the same reasoning that already required widening `ChannelKey`
to include `pin_select`.

**Correction (found by Codex review, PR #28, a third round, before merge):**
two more bugs, both direct consequences of the two Corrections above. (1)
`autodetect_sae_j1850_flavor`'s write-back (the first Correction's Bug 2
fix) updated `hw_protocol_id` to the detected flavor's `_PS` id but left
`pin_select_base_hw_protocol_id` (the first Correction's Bug 1 fix's new
field) at its Create-time value (always `J1850VPW`, the initial candidate
before autodetection runs) — the two fields must move together since
`base_hw_protocol_id()` reads the latter directly, so a PWM detection left
it reporting stale `J1850VPW`, incorrectly rejecting PWM-only ComParams
like `CP_NetworkLine`. Fixed by refreshing
`pin_select_base_hw_protocol_id` alongside `hw_protocol_id` in the same
write-back. (2) `same_physical_resource`'s pre-connect fallback (used when
at least one side of a physical-lock check lacks a `channel_key` yet —
`find_physical_lock_holder`, called from 8 sites across
`connect_new_physical_channel`, `rpc_lock_resource`/`rpc_unlock_resource`,
4 `PDU_IOCTL_*` filter/queue handlers, `SetUniqueRespIdTable`'s ISO15765
staging gate, and `handle_update_param`'s live lock re-check) compared raw
`hw_protocol_id` alone — for two `_PS` links sharing the same consolidated
`_PS` id but different `pin_select` values, this falsely treated them as
the same physical resource before either connects, letting an
already-locked link block an unrelated different-pins connect that
`ChannelKey`'s widened 3-tuple says should proceed independently. Fixed by
widening the fallback comparison to also require `pin_select` equality
(a no-op for every non-`_PS` case, where both sides are always `None`,
including the software-ISO-TP raw-CAN/ISO15765-sibling sharing case this
fallback exists for in the first place) across all 8 call sites.

**Correction (found by Codex review, PR #28, a fourth round, after the
Accepted residual below was first recorded):** `events.rs`'s
`wait_for_expected_response`/`wait_for_expected_response_inner`
`CP_EnableConcatenation` eligibility gate (ADR-148) derived
`ChannelProtocol::from_raw(wait.protocol_id)` directly from the raw,
unnormalized protocol id to decide `is_kwp_family()`/`is_j1850_family()`
eligibility — the same Plane B shape as every site this ADR's original sweep
fixed, but originally missed because it was found only after this fix's
implementation rounds had already concluded (recorded at the time as an
accepted residual, below). For a pin-selected KWP/J1850 link,
`wait.protocol_id` is the raw `_PS` id, which `from_raw` never recognizes as
belonging to its base family, silently disabling concatenation. A live PR
#28 review round subsequently flagged this residual as worth fixing rather
than leaving deferred; fixed by normalizing via `resources::base_protocol_id`
before the `from_raw` call, mirroring the same normalization the same
function already applies a few lines above for `rc_cfg`'s
`RcHandlingConfig::from_params` call. The "Accepted residual, minor" bullet
below is retained for historical context but no longer describes current
behavior — see its own note.

**Fix (found by Codex review, PR #28, same round as the fourth Correction
above):** `rpc_link.rs`'s dual-channel-mode UUDT companion-channel open
(`ConnectComLogicalLink`'s tail, immediately after the `probe_can_channel_mode`
call already gated on `link_pin_select.is_none()`) had no equivalent guard —
`ensure_uudt_companion_channel` always opens a plain (non-`_PS`) raw-CAN
companion channel using only the link's default pins, discarding
`pin_select` entirely. For a pin-selected `ISO15765_PS`/`CAN_PS` link with
dual-channel mode configured and a UUDT response ID already set at connect
time, this routed UUDT traffic to a companion channel on the wrong physical
pins instead of the caller's selected bus segment. Building genuine
pin-aware companion-channel support (a `_PS`-variant companion opened on the
same pins, its own `SET_CONFIG`, its own `ChannelKey` entry) would be a
substantial new mechanism; per this same ADR's existing "software-ISO-TP ×
Pin Selection interaction remains unimplemented" precedent (an entire
interacting CAN-channel-mode mechanism already scoped out of Pin Selection
support rather than extended), the same strategy is applied here: the
companion-channel open is now also skipped for a pin-selected link (the same
`link_pin_select.is_none()` condition added to this block), so
dual-channel-mode UUDT capture is simply skipped, not attempted or failed,
for a pin-selected link. `promote_unique_resp_id_table` (`rpc_link.rs`) has
its own, separate dual-channel companion-open block — reached when a UUDT id
is added via `SetUniqueRespIdTable` after connect, rather than being present
at connect time — with the identical gap and no shared code with the
connect-time site; fixed with the same `link_pin_select.is_none()` guard.
See the new accepted residual below.

**Fix (found by `edge-case-hunter`, PR #28, final pre-merge pass; confirmed
by an independent repro, reverted after confirmation):** the companion-open
skip above left `rpc_link.rs`'s `install_point_to_point_fc_filters` out of
sync with the two call sites it accompanies. That function separately
decides whether to install a point-to-point `FLOW_CONTROL_FILTER` fallback
for a UUDT response id on the main channel, and skipped that fallback
whenever the *module-wide* `effective_can_channel_mode()` was
`DualChannel` — a decision made with no awareness of whether the specific
connecting link is pin-selected, unlike the two companion-open sites above.
For a pin-selected `ISO15765_PS`/`CAN_PS` link with a UUDT response id
configured, on a device whose CAN channel mode is (or resolves to)
dual-channel: the companion channel is correctly never opened (per the Fix
above), but this function *also* wrongly skipped the compensating
point-to-point filter, believing a companion existed. Net effect: the link
got neither a companion channel nor the fallback filter — total, silent
loss of UUDT response capture, worse than the accepted residual below
originally described. Fixed by threading the connecting link's
`pin_select: Option<u32>` into `install_point_to_point_fc_filters` (its
three call sites — `rpc_connect_com_logical_link`'s tail,
`promote_unique_resp_id_table`'s rebuild, and
`reinstall_iso15765_channel_filters_after_clear`'s rebuild — already had, or
could trivially obtain, the link's `pin_select` in scope from the earlier
`hw_protocol_id`-threading fix above) and narrowing the internal skip
condition to "a companion actually exists for this link":
`effective_can_channel_mode() == DualChannel && pin_select.is_none()`,
rather than the module flag alone. A pin-selected link now always gets the
fallback point-to-point filter, regardless of the module's dual-channel
setting, since it never gets a companion either way. See the corrected
accepted residual below.

- Future `_PS`-eligible protocol additions (SWCAN, J1939, TP2.0, per
  ADR-156's own accepted residuals) must extend `ps_protocol_id` and
  `base_protocol_id` in lockstep — a round-trip test (`base_protocol_id
  (ps_protocol_id(base).unwrap()) == base` for every mapped base id) is
  added by this fix to catch a future one-sided edit.
- **(ADR-156 Decision 3 addendum/Phase 2b)** The same lockstep requirement
  now also covers `_CHx`: a future `_CHx`-eligible protocol addition (the
  same SWCAN/J1939/TP2.0/etc. families) must extend `chx_protocol_id` and
  `chx_base_protocol_id` together (both in `resources.rs`), which
  `base_protocol_id` consults after its `_PS` match so every Plane B site
  this ADR fixed keeps handling `_CHx` for free. The widened round-trip
  test (`chx_protocol_id_round_trips_every_in_scope_base_protocol_id_mapping`)
  covers all seven in-scope families across representative indices (1, 64,
  128), including the SCI-representative collapse, mirroring
  `base_protocol_id_round_trips_every_ps_protocol_id_mapping`'s own
  coverage for `_PS`.
- Software-ISO-TP × Pin Selection interaction remains unimplemented (accepted
  residual above); a caller requesting Pin Selection on an ISO15765 link in
  a context that would otherwise select software ISO-TP gets hardware ISO-TP
  instead, silently. Tracked here rather than blocking Phase 2a further.
- **Formerly an accepted residual, minor — now fixed (see the fourth
  Correction above):** `events.rs`'s `CP_EnableConcatenation` eligibility
  check (ADR-148) derived a `ChannelProtocol::from_raw` from a raw,
  unnormalized protocol id in the same Plane B shape as every site this ADR
  fixes, but was found after this fix's implementation rounds concluded and
  was not itself fixed at the time — for a `_PS` link, concatenation
  eligibility could be misjudged. A subsequent Codex review round (PR #28)
  flagged it as worth fixing rather than leaving deferred; it is now fixed by
  normalizing via `resources::base_protocol_id` before the `from_raw` call.
- **Accepted residual (narrowed by the second "Fix" paragraph above):** a
  pin-selected link on a dual-channel-capable adapter does not get true
  dual-channel-mode UUDT capture via a companion channel —
  `ensure_uudt_companion_channel`'s companion channel is opened on the
  link's default pins, never the caller's selected pins, so
  dual-channel-mode's connect-time companion-channel open is skipped
  entirely for a pin-selected link rather than routing UUDT traffic to the
  wrong physical pins. Building genuine pin-aware companion-channel support
  is tracked as a residual rather than built out now, mirroring this same
  ADR's "software-ISO-TP × Pin Selection interaction remains unimplemented"
  residual's own reasoning and precedent. Unlike when this residual was
  first recorded, the link does not lose UUDT capture altogether:
  `install_point_to_point_fc_filters` now always installs the
  point-to-point `FLOW_CONTROL_FILTER` fallback on the main channel for a
  pin-selected link (the second "Fix" paragraph above), which captures the
  same UUDT traffic a companion channel would have, on the caller's actually
  selected pins — arguably a *better* outcome than genuine dual-channel-mode
  capture would give, since it needs no companion channel at all. The only
  remaining gap is architectural (no companion channel, hence no true
  dual-channel-mode split), not a functional loss of UUDT response capture.

**Fix (found by Codex review, PR #28, a later round, after Codex's clean
approval of commit `99d32f4b`):** `names.rs::resolve_pin_selection` — the
function this whole ADR's Plane B fix and ADR-156 Decision 2's resolution
pipeline both depend on for a correctly-normalized base hardware id — never
normalized its own raw `hw_protocol_id` input before looking up default
pins/`ps_protocol_id`, both keyed by base J2534-1 ids. A caller naming a
`_PS` id directly via `protocol_id` (e.g. `PROTOCOL_CAN_PS`, reaching
`resolve_protocol_id`'s `ChannelProtocol::from_raw` fallback, since no
`resources` table row exists for a `_PS` id) hit two failure modes: empty
`dlc_pin_data` silently returned `Ok(None)`, connecting a `_PS` channel with
`CONFIG_J1962_PINS` never issued; non-empty `dlc_pin_data` incorrectly
rejected the request via the `ps_protocol_id` lookup returning `None`.
Fixed by normalizing the raw id via `resources::base_protocol_id` up front
and tracking whether normalization changed it (`already_ps`): an empty
`dlc_pin_data` now rejects outright when `already_ps`, and the
"matches defaults, no Pin Selection" short-circuit is skipped when
`already_ps`. See ADR-156's Corrections for the full detail (this is
`resolve_pin_selection`'s own mechanism, owned there; noted here only
because it is the direct input to this ADR's `base_hw_protocol_id`
Plane B fix).

**Correction to the above (found by `edge-case-hunter`, PR #28, same round):
the fix only covered `RscData::ProtocolId`/`ProtocolName`, not the bare
`Resource::ResourceId`/`ResourceName` variants, which have no
`dlc_pin_data` field at all and never called `resolve_pin_selection`.**
`resource_id` is an unrestricted `uint32`, and this module's own
`resources.rs` documents that a raw/extended `ChannelProtocol` value
(including, numerically, a `_PS` id) remains an independently valid input
on that route — so a caller supplying `resource_id = PROTOCOL_CAN_PS`
(or a `resource_name` string that parses as that number) reproduced the
exact original bug, plus two additional gaps unique to this route: the
ADR-156 Decision 4/clause 5 J2534-2 opt-in gate lives solely inside
`resolve_pin_selection`, so it was bypassed entirely; and because
`pin_select_base_hw_protocol_id` stayed `None` while `hw_protocol_id` held
the raw unnormalized `_PS` value, `base_hw_protocol_id()` returned the raw
`_PS` id itself instead of a normalized base id — reopening this ADR's
entire Plane B regression class for exactly this link. Fixed by calling
`resolve_pin_selection(protocol, row, &[], j2534_2_opted_in)` (an empty
pin slice — neither variant has pins to supply) from both arms, reusing the
same `already_ps` rejection rather than inventing new logic: a raw `_PS`
id is now rejected the same way it is when named via `protocol_id` with no
pins, and every non-`_PS` id (everything either variant supported before
this fix) still resolves to `Ok(None)` unchanged.

**Accepted residual, SCI variant fidelity when `PROTOCOL_J2610_PS` is named
directly (found by `edge-case-hunter`, PR #28, same round, not fixed):** the
table-row-based SCI route preserves the exact `SCI_A_ENGINE`/`_A_TRANS`/
`_B_ENGINE`/`_B_TRANS` variant via `resource_row.hw_protocol_override`
(this ADR's Correction above). A caller instead naming `PROTOCOL_J2610_PS`
directly via `protocol_id` (no resource row involved) with `dlc_pin_data`
for a non-representative variant's wiring (e.g. `SCI_B_TRANS`'s pins)
still connects and pins correctly — the real `PassThruConnect`/
`SET_CONFIG(CONFIG_J1962_PINS)` sequence uses the caller's actual pins
throughout — but `resources::base_protocol_id(PROTOCOL_J2610_PS)`
unconditionally collapses to the single representative `SCI_A_ENGINE` id
(by this function's own documented, deliberate design — see
`base_protocol_id`'s doc comment), which is what gets stored into
`pin_select_base_hw_protocol_id` and read back by `base_hw_protocol_id()`.
`GetResourceStatus`'s ambiguous-`"SAE_J2610_SCI"`-name candidate match
(`rpc_link.rs`, `hw_override.is_none_or(|hw| hw == link.base_hw_protocol_id())`)
uses that accessor, so querying occupancy for such a link's actual
resource id (e.g. `SCI_B_TRANS`) reports "not in use" while
`SCI_A_ENGINE` falsely reports "in use" — an internally inconsistent
occupancy answer for `GetResourceStatus` specifically, with no effect on
the connect/pin-assignment behavior itself. Disambiguating which SCI
variant a directly-named `PROTOCOL_J2610_PS` request means (matching the
caller's pins against each variant's `PINS_SCI_*` wiring, mirroring what a
table-row resolution already does implicitly) is a real fix, not a
one-line change, and is deferred as a residual rather than built out now —
this is a status-reporting-only gap, on an already-narrow route (naming
the consolidated SCI `_PS` id directly, bypassing the resource table
entirely, is not how the table-row-based flow this service's own
resource-name resolution steers callers toward is used).

**Fix (found by Codex review, PR #28, a later round, on commit `be0eeca`):**
`rpc_connect_com_logical_link`'s own `base_proto_id` derivation — used by
the ISO15765 FC-filter-installation gate and the CAN dual-channel-mode
auto-probe gate, separate from (and pre-dating) `connect_new_physical_channel`'s
own already-correct `resources::base_protocol_id(j2534_proto_id)` — computed
`protocol.j2534_protocol_id()` whenever `pin_select` was set, on the
assumption that `protocol` (the CLL's stored `ChannelProtocol`) is always
the pre-existing base protocol whenever Pin Selection is active. That
assumption held before a caller could name a `_PS` id directly via
`protocol_id` (this ADR's Correction above, and ADR-156's matching
Correction): for that case `protocol` is itself
`ChannelProtocol::from_raw(_PS id)`, whose `j2534_protocol_id()` passes the
raw `_PS` id through unchanged instead of normalizing it. For a directly-named
`ISO15765_PS` link with a UniqueRespIdTable configured, this silently
skipped the ISO15765 FC-filter-installation branch while
`connect_new_physical_channel`'s own (correctly normalized) gate still
correctly suppressed the illegal pass-all fallback — leaving the channel
with **zero** filters installed, silently losing every response, strictly
worse than either gate being individually wrong. Fixed by reading
`link.base_hw_protocol_id()` directly (the same correctly-populated
accessor `promote_unique_resp_id_table`'s sibling site already used) while
`link` is still in scope in the earlier destructuring block, instead of
reconstructing it afterward from `protocol`.

**Correction (design-advisor, PR #28, after a 3rd/4th independent instance of
the same normalization gap surfaced across three review passes): the root
cause was structural, not another missing call site.** While writing a
regression test for the `base_proto_id` fix immediately above, reverting
just that fix and running the new test failed *earlier* than expected — at
`SetUniqueRespIdTable`, before even reaching the code path that fix
touches. `comparam_support::unique_id_params(protocol)` (called by
`check_param_allowed` to decide whether `CP_CanRespUSDTId`/`CP_CanPhysReqId`
are `PDU_PC_UNIQUE_ID` class) calls `protocol.is_can_family()`, which checks
`self.j2534_protocol_id()` against native base ids — for
`protocol = ChannelProtocol::from_raw(PROTOCOL_ISO15765_PS)`,
`j2534_protocol_id()`'s catch-all passes the raw `_PS` id through
unchanged, so `is_can_family()` returns `false` and the request is
incorrectly rejected. This is the same mechanism as the Correction and Fix
immediately above it, but at a fourth independent site — crossing this
repo's own escalation threshold for "stop point-patching, get a systemic
answer" (`.claude/README.md` rule 3's design-advisor tripwire). A
`design-advisor` consult confirmed the structural root cause: this ADR's
Decision item 6 already states the invariant these bugs all violate —
"a `CAN_PS` link's `protocol` field is `ChannelProtocol::CAN`" — which held
for every pre-existing resolution path (the table-resolved Pin Selection
path never stores a `_PS`-wrapped `ChannelProtocol`) but was broken by
`resolve_pin_selection`'s later Correction (above) allowing a caller to
name a `_PS` id directly: `resolve_protocol_id`'s `ChannelProtocol::from_raw`
fallback then stores the raw `_PS` id verbatim into `LogicalLinkState.protocol`
itself, not just into `hw_protocol_id`. Every consumer of `link.protocol`
that assumes the ADR's own stated invariant — `is_can_family`/
`is_kwp_family`/`is_j1850_family`/`is_sci_family`, `unique_id_params`,
`uds_session_timing_applies`, `tx_message_size_range`, the `GetResourceStatus`
`active_candidate` match (`link.protocol == p`) — was a latent site, not
just the ones found so far.

**Fixed at the source rather than by enumerating consumers:**
`names.rs::parse_protocol_id_from_resource` now normalizes its resolved
`protocol` at its single return point, after `resolve_pin_selection` (whose
own `already_ps` detection needs the raw, un-normalized value and must run
first): when `resources::base_protocol_id(protocol.value()) != protocol.value()`,
`protocol` is replaced with `ChannelProtocol::from_raw` of the normalized
base id. This is identity for every non-`_PS` value (including every
extended 0x0100+ id), so it changes nothing for any path that existed
before the raw-`_PS`-direct-naming feature; it restores the ADR's own
Decision-item-6 invariant for every current and future `link.protocol`
consumer in one place, rather than requiring an open-ended sweep (the
consumer set is not enumerable the way ADR-157's original ~30 sites were —
any future code reading `link.protocol` inherits the fixed invariant for
free). For the SAE J2610 SCI case, `base_protocol_id(PROTOCOL_J2610_PS)`
yields the representative `SCI_A_ENGINE` hardware id, so `protocol`
normalizes to `ChannelProtocol::SCI_A_ENGINE` — deliberately not
`ChannelProtocol::SAE_J2610_ON_SAE_J2610_SCI` (0x0160), whose own
`j2534_protocol_id()` is the unrelated `SCI_MODE` TX-flag value
(`protocol.rs`), which would disagree with
`pin_select_base_hw_protocol_id = SCI_A_ENGINE` and reopen this ADR's own
first Correction's mismatch. This SCI collapse is the same accepted
residual already recorded above (naming `PROTOCOL_J2610_PS` directly loses
exact-variant fidelity) — `protocol` and `pin_select_base_hw_protocol_id`
now at least agree with each other on the same representative variant,
which they did not before this fix.

**Query-side sibling, same round:** `rpc_link.rs::rpc_get_resource_status`'s
`ResourceId` query arm built `ChannelProtocol::from_raw(id)` un-normalized
for a numeric id with no table row — the same fallback shape, on the query
side. Since a connected link's `protocol` is now always normalized (the fix
above), an un-normalized `_PS` query id could never match it in the
`active_candidate` comparison, falsely reporting the resource idle for a
query naming the exact `_PS` id a link is actually connected with. Fixed by
applying the identical `resources::base_protocol_id` normalization to this
query-side candidate; `query_resource_id` (echoed back verbatim in the
response) is unaffected, since it is captured separately from the caller's
literal `id`. The `ResourceName` variant of the same query has no
equivalent numeric-string fallback (`map_protocol_name` only recognizes
fixed aliases, not raw numeric strings), so it does not share this gap.

**Two more Plane B sites found by Codex review, PR #28, the following round
(on commit `3a93cf8`, before the source-level `protocol` fix above had been
reviewed) — both pre-dating that fix, in the sense that they read the raw
`protocol_id`/hardware id directly rather than through `link.protocol`, so
the source-level fix does not cover them:**

- **`events.rs`'s fast-init response header/footer split.**
  `handle_start_comm` passed the raw `protocol_id` -- not
  `base_protocol_id`, which `StartCommParams` already carries into this
  same function's scope (destructured alongside `protocol_id` at the top of
  `handle_start_comm`, and already used a few lines earlier for this
  function's own `apply_params_to_hardware` call) -- to `header_footer_len`,
  whose match only recognizes base K-line ids (`ISO9141`/`ISO14230`). A
  `_PS` id hits the catch-all `(0, 0)` arm, so a pin-selected K-line link's
  fast-init response was delivered as one unsplit blob (the whole frame in
  `data_bytes`) instead of the KWP header/payload/footer split ADR-051/
  ADR-075 expect. Fixed by passing `base_protocol_id` instead.
- **The mock's `IOCTL_FIVE_BAUD_INIT`/`IOCTL_FAST_INIT` handlers never
  checked `channel.pins_assigned`.** Both handlers normalize
  `channel.protocol_id` via `base_protocol_id` to decide *which* protocols
  may run either init IOCTL (ISO9141/ISO14230, `_PS` included, per this
  ADR's original sweep), but neither checked whether the `_PS` channel's
  pins were actually assigned yet — every other I/O IOCTL in this mock
  (`PassThruWriteMsgs`/`ReadMsgs`/`StartMsgFilter`/`StartPeriodicMsg`) gates
  on `pins_assigned` (ADR-156 Decision 2), but 5-baud/fast init communicate
  over the same physical pins and were never included in that gate. Fixed
  by adding the identical `pins_assigned` check to both handlers, in the
  same position (after the channel/protocol validity checks, before any
  other side effect) the existing gate uses elsewhere in this file. Two
  pre-existing tests (`five_baud_init_succeeds_for_ps_k_line_protocols`/
  `fast_init_succeeds_for_ps_k_line_protocols`) connected a `_PS` K-line
  channel and ran the init IOCTL directly, with no pins ever assigned —
  demonstrating the exact gap, once this fix's regression tests proved they
  should have failed; updated to assign pins first.

**A third site, found by Codex review, PR #28, the following round (on
commit `831779c`): `rpc_get_resource_status`'s `status_hw_id` computation
substitutes the module-wide `can_channel_mode`'s hardware mapping (raw
`CAN` for an ISO15765-family resource candidate in `SoftwareIsoTp` mode),
but Pin Selection is a per-link override of that module-wide mode.** Per
this ADR's own "`_PS` links are hardware-ISO-TP only" accepted residual
(above), a pin-selected `ISO15765_PS` link's `software_isotp` is always
`false`, so it stays on the natural ISO15765 hardware identity regardless
of the module's software-ISO-TP setting. On a module configured for
`software-isotp`, querying `GetResourceStatus` for the ISO15765 resource
while such a link is connected compared its `base_hw_protocol_id()`
(ISO15765) against `status_hw_id` (raw `CAN`, from the mode substitution)
— never equal, so bit 0 ("in use") read clear and no held-lock bits were
reported, for a link that plainly occupies that resource. Fixed by also
computing `hardware_native_hw_id` — the same candidate's hardware id
*without* the `can_channel_mode` substitution — and matching a link against
either id, not `status_hw_id` alone; identity for every case where the two
don't diverge (every mode but `SoftwareIsoTp`, and every non-ISO15765-family
candidate). Confirmed via a temporary revert of just this fix (the new
regression test's own failure, then restored) rather than trusting the fix
in isolation.

**A fourth site, found by Codex review, PR #28, the following round (on
commit `9db653f`): `rpc_get_resource_status`'s `hardware_native_hw_id`
fix (the site immediately above) normalized a raw `PROTOCOL_J2610_PS`
query id via `resources::base_protocol_id`, which -- correctly, for every
OTHER Plane B consumer -- collapses all four native SAE J2610 SCI ids onto
one representative (`SCI_A_ENGINE`).** A resource-status query is
different: a raw `PROTOCOL_J2610_PS` query asks about the consolidated
`_PS` id itself, unqualified by any specific variant, so it should match a
connected link using *any* of the four -- but `base_protocol_id`'s single
representative meant a link using a non-representative variant (e.g.
`SCI_B_TRANS`, whose `base_hw_protocol_id()` correctly preserves the exact
variant per this ADR's earlier Correction) was falsely reported idle.
Fixed by adding `resources::is_sci_hw_protocol_id` (`true` for any of the
four native SCI ids) and broadening the `in_use`/`held_lock_mask` match:
when the caller's literal, un-normalized query id (`query_resource_id`,
distinct from the post-normalization `hardware_native_hw_id` -- the same
`SCI_A_ENGINE` value can also arise from querying that variant's own raw
hardware id directly, which must NOT broaden to match every variant) was
exactly `PROTOCOL_J2610_PS`, a link matches if its `base_hw_protocol_id()`
is any SCI variant, not just the query's own single representative.
Confirmed via a temporary revert of just this fix.

**A fifth site, same round, in the mock rather than the service:**
`j2534-0404-mock`'s `IOCTL_SET_CONFIG(CONFIG_J1962_PINS)` handler
unconditionally set `channel.pins_assigned = true` whenever the caller
supplied a `CONFIG_J1962_PINS` config entry at all, including a value of
`0x00000000` -- clause 6.3.3.2's own "no selection performed" sentinel for
the packed bitmask, never produced by `compute_pin_select` (every entry
requires a concrete nonzero pin number). A direct mock client sending that
sentinel value therefore silently marked the channel's pins assigned,
defeating the `ERR_PIN_INVALID` I/O gate this whole mechanism exists to
enforce -- not reachable through the service's own normal operation (which
never produces the sentinel), but a real robustness gap for any other
caller of this mock crate. Fixed by only setting `pins_assigned`/recording
`newly_bound_pins` when the supplied value is nonzero; the `SET_CONFIG`
call itself still succeeds either way (the sentinel is a valid value to
write, it just doesn't count as a real assignment), and a follow-up call
with a real value remains legitimate since nothing was actually bound by
the sentinel call.

# ADR-137: `CP_TesterPresentHandling` Gates Tester-Present Arm/Re-arm (and Disarms Live)

**Date:** 2026-07-27
**Status:** Accepted (ADR-084's "an already-armed CLL whose new resolution disables
tester-present keeps running" precedent is amended by this ADR for the specific case of a live
`CP_TesterPresentHandling` transition to `0` — see Decision)
**Affects:** `j2534-0404-service` service (`ComParamSet::tester_present_handling`,
`TesterPresentState::Disarmed`/`Cleared`, `ResidualTesterPresentDiscard`,
`LogicalLinkState.open_tp_discards`), rpc_primitive (`resolve_tester_present`,
`ResolvedTesterPresent`), events (`handle_start_comm`, `handle_update_param`,
`build_cll_rx_entries`, `bind_frame`, `dispatch_due_tester_present`,
`push_open_tp_discard`), rpc_misc (`PDU_IOCTL_CLEAR_PERIODIC_MSGS` handler),
rpc_link (`DisconnectComLogicalLink`)

## Context

`CP_TesterPresentHandling` (`PARAM_TESTER_PRESENT_HANDLING`, id `0x8006`,
`service_params.rs:40`) has been a fully registered D-PDU ComParam since it was added: it is
`Get`/`SetComParam`-reachable on every protocol family that supports it (`comparam_support.rs`'s
`is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/`is_j1850vpw_param` allowlists), it is a member
of `TESTER_PRESENT_CLASS_PARAMS` (`comparam_support.rs:675`, consumed by
`tester_present_params_differ`), it is named in `names.rs`, and it is seeded with a per-protocol
default in every `comparam_defaults.rs` preset. But no runtime code ever reads its *value*: unlike
every sibling tester-present accessor (`tester_present_data`, `tester_present_interval_us`,
`tester_present_send_type`, `tester_present_req_rsp`, `tester_present_exp_pos_resp`,
`tester_present_exp_neg_resp` — all at `service.rs:560-614`), there was no
`tester_present_handling()` accessor, and `resolve_tester_present`/`ResolvedTesterPresent`
(`rpc_primitive.rs:446-578` / `275-374`) never consulted the param.

ISO 22900-2:2009(E) (the only edition available in this workspace; a 2022-edition revision to
this clause has not been checked) defines `CP_TesterPresentHandling` as the master enable switch
for periodic/idle-triggered tester-present, independent of `CP_TesterPresentTime` (the cadence,
which already has its own `== 0` "disabled" sentinel, `rpc_primitive.rs:501-522`):

- The ComParam table describes `CP_TesterPresentHandling` as the setting that controls whether
  tester-present message generation happens at all, gated on the ComLogicalLink being in state
  `PDU_CLLST_COMM_STARTED`; its value range is `[0;1]`, with per-protocol defaults `ISO_15765_4=0,
  ISO_14230_4=1, ISO_9141_2=1, SAE_J1850_VPW=0, SAE_J1850_PWM=0, ISO_14230_3=1, ISO_15765_3=1`.
- Tester-present messages are only enabled while the ComLogicalLink is in
  `PDU_CLLST_COMM_STARTED`; the spec cross-references `CP_TesterPresentHandling` as the actual
  enabling switch, and specifies that once handling is turned on, the first message goes out
  immediately rather than waiting for the first `CP_TesterPresentTime` cyclic interval to elapse.
- Suspending a ComPrimitive queue also stops any enabled tester-present sending (again gated on
  `CP_TesterPresentHandling`).
- NOTE 1 clarifies that while the CLL is comm-started with handling enabled, changing any of the
  tester-present-related ComParams triggers an immediate send of the next tester-present message,
  ahead of the normal cyclic schedule.

Only two call sites decide whether tester-present is armed today; `dispatch_due_tester_present`
(the per-tick sender, `events.rs:3901+`) only iterates already-`Armed` links and never re-derives
the enable/disable decision, so it is not a third site:

1. `handle_start_comm` (`events.rs:7041-7044`): arms unless `tester_present_data.is_empty() ||
   tester_present_interval_ms == 0`.
2. `handle_update_param`'s live re-arm gate (`events.rs:8250-8262`): re-resolves on a successful
   `CoptUpdateparam` promotion and re-arms only if `resolved.interval_ms > 0` and
   `!same_wire_behavior(old, new)`. Critically, its `Ok(_) => {}` fallthrough (documented at
   `events.rs:8215-8225`) is a **deliberate ADR-084 design choice**: an already-armed CLL whose
   new resolution would disable tester-present (e.g. `CP_TesterPresentTime` promoted to `0`) is
   left running rather than disarmed — "some keep-alive beats none," guarding against an
   *implicit* disable (this service's own `interval == 0` sentinel, or a stray unrelated
   promotion) silently killing a session's keep-alive.

Design-advisor was consulted (this repo's `.claude/README.md` cost-policy rule 3 gate: protocol
interpretation + state-machine design in an area with heavy prior ADR precedent — ADR-083,
ADR-084, ADR-088 and its two amendments, ADR-093, ADR-096, ADR-099 all shape this exact
subsystem) to resolve the design questions below without silently reinterpreting that precedent.

## Decision

### Where the gate lives

Add `ComParamSet::tester_present_handling() -> u32` (`service.rs`, `unwrap_or(0)`, matching every
sibling accessor's convention) and a new `handling_enabled: bool` field on `ResolvedTesterPresent`
(`rpc_primitive.rs`), populated by `resolve_tester_present` from that accessor (`handling ==
1`) — including in the empty-payload short-circuit, so the field is always truthful regardless of
which path resolution took.

**Out-of-range values (e.g. `2`) coerce to disabled, not a resolution error** — `handling == 1`
rather than `handling != 0`. `SetComParam` does not range-check this param any more than it does
`CP_TesterPresentReqRsp`, and `handling_enabled`'s role is the same boolean-gate role
`expects_response` (`CP_TesterPresentReqRsp`) already has (`service.rs`'s existing
`tester_present_req_rsp() == 1` → `expects_response`, `rpc_primitive.rs`), not the
range-validated-enum role `CP_TesterPresentSendType` has (which `resolve_tester_present` rejects
outright for `!= 0 && != 1`, `rpc_primitive.rs`). `CP_TesterPresentHandling` follows its
same-class sibling `CP_TesterPresentReqRsp`'s existing convention rather than
`CP_TesterPresentSendType`'s: an out-of-range handling value silently means "not exactly enabled,"
consistently with how an out-of-range req/rsp value already silently means "no response
expected," not a synchronous or asynchronous error either.

`handling_enabled` is excluded from `same_wire_behavior` (`rpc_primitive.rs:412-419`): it is a
gate on *whether* tester-present runs, not wire content, the same taxonomy that already excludes
`expects_response`/`exp_pos_resp`/`exp_neg_resp`/`target_can_ids`/`p2_max_ms`. Both call sites
branch on it explicitly, before/alongside their existing `interval_ms`/`data.is_empty()` checks,
rather than folding `handling == 0` into `interval_ms = 0` — the two concepts (configured cadence
vs. whether TP is enabled at all) stay independently named, and folding them would not even save a
branch once the asymmetric re-arm-vs-disarm behavior below (§ "Live re-arm — disarm on 1→0") is
accounted for.

- `handle_start_comm` (`events.rs:7041-7044`): the `None` condition gains `||
  !tester_present.handling_enabled`.
- `handle_update_param` (`events.rs:8250-8262`): the existing re-arm match gains a new leading
  arm, `Ok(resolved) if !resolved.handling_enabled => { /* disarm, see below */ }`, ahead of the
  existing `Ok(resolved) if resolved.interval_ms > 0 && !same_wire_behavior(...) => { /* re-arm */
  }` guard and its `Ok(_) => {}` fallthrough.

### Live re-arm: disarm on a live `1 → 0` transition (scoped exception to ADR-084)

Unlike an implicit disable via `CP_TesterPresentTime == 0` (which keeps ADR-084's existing
fallthrough behavior — this ADR does not touch that path), a `CoptUpdateparam` that promotes
`CP_TesterPresentHandling` to `0` while a CLL is `Armed` **disarms it**
(`TesterPresentState::None`, no send, no error event — a successful reconfiguration, a no-op if
already `None`).

Rationale: `CP_TesterPresentHandling` is spec-defined as the *explicit* enable switch (`Value:
[0;1]`), and the spec repeatedly frames a running tester-present as something later state
explicitly stops it (queue suspension halts an enabled tester-present send per the ComParam table
above; `PDU_COPT_STOPCOMM`'s own behavior likewise ends further tester-present sending). Setting
`CP_TesterPresentHandling = 0` via `CoptUpdateparam` is the spec's only in-band mechanism for a
client to stop the keep-alive without a full `CoptStopcomm`/`CoptStartcomm` cycle; leaving that
case in ADR-084's "keep the stale keep-alive running" fallthrough would make that spec-defined
operation permanently unimplementable through this D-PDU surface. ADR-084's original fallthrough
was defensive against *implicit/accidental* disables (this service's own `interval == 0`
sentinel, or a stray unrelated ComParam promotion) — a live, explicit `handling = 0` is neither;
it is the client asking, in-band, for tester-present to stop. ADR-084's behavior for
`interval_ms == 0`, an unrelated promotion, and a resolution `Err` is otherwise unchanged.

A `0 → 1` live transition needs no special-casing: after this change, a CLL can only be `Armed`
with `handling_enabled == true`, so a promotion enabling handling always finds `old_resolved ==
None`, falls through to the existing "no prior armed state" re-arm path, and sends immediately —
matching the spec's immediate-first-send behavior described above.

### `comparam_defaults.rs`: no correction needed

Every preset's existing `PARAM_TESTER_PRESENT_HANDLING` seed was checked against the spec table's
per-protocol defaults and found already correct: `kwp_on_kline_common` (`comparam_defaults.rs:477`)
and `kwp_on_9141_common` (`:651`) = `1` (ISO_14230_3/4, ISO_9141_2 = 1 — `iso_obd_on_k_line`
reuses `kwp_on_kline_common`, so it inherits `1` too, despite having no direct insert of its own);
`iso_14230_3_on_iso_15765_2` (`:831`) = `1`; `iso15765_4_common` (`:900`) = `0` (`ISO_15765_4 =
0`); `iso_15765_3_on_iso_15765_2` (`:989`) = `1`; `j1850_common` (`:1219`) = `0`
(`SAE_J1850_VPW`/`PWM = 0`). `sae_j2190_on_iso_15765_2` (`:1060`) = `1` has no corresponding row in
the spec's ComParam table at all (SAE J2190 is not one of the seven protocol identities the spec
enumerates for this param) — kept at `1`, a repository default matching this preset's sibling
enhanced-diagnostic presets, not a spec-derived value; flagged here rather than silently assumed.

`tester_present_handling()`'s `unwrap_or(0)` fallback (conservative: "no TP unless configured")
only matters for a `ComParamSet` with no seed at all, which — given every real preset above seeds
it directly or via a shared helper — implies an unconfigured `tester_present_data` too, which
already independently yields `TesterPresentState::None`.

### Codex-review fix: the master-disable check must run before any fallible resolution

The first PR review round (Codex) found that the initial implementation computed
`handling_enabled` LAST in `resolve_tester_present`, after the function's other fallible steps
(tester-present message/header construction, `CP_TesterPresentTime` sub-500µs rounding rejection,
`CP_TesterPresentSendType` range validation). A `CP_TesterPresentHandling = 0` promotion combined
with any other invalid tester-present ComParam (e.g. an out-of-range `CP_TesterPresentSendType`)
therefore made the whole function return `Err` before `handling_enabled` was ever read — at
`handle_update_param`, this meant the `Err` arm fired instead of the new disarm arm, leaving a
spurious `PduErrEvtTesterPresentError` and the prior `Armed` keep-alive still running despite the
explicit disable; at `handle_start_comm`, the same ordering could reject `CoptStartcomm` outright
with `INVALID_ARGUMENT` even though handling being off made the rest of the configuration
irrelevant.

Fixed by hoisting the `CP_TesterPresentHandling` check to the very first line of
`resolve_tester_present`: when `!= 1`, the function now returns immediately with a vacuous,
all-disabled `Ok(handling_enabled: false, ...)`, before `can_addressing`/`data`/`interval_ms`/
`send_type` are ever resolved. Since the master switch is off, none of that configuration's
validity matters — nothing downstream reads those fields once `handling_enabled == false`. This
also means `resolve_tester_present` can no longer fail with `Err` purely because of tester-present
configuration that a disabled handling value has made moot; a resolution failure now only ever
means "handling is on, and something it actually needs is invalid." Two new regression tests
(`handling_0_with_invalid_send_type_still_succeeds_at_startcomm`,
`handling_1_to_0_disarms_despite_simultaneously_invalid_send_type`) cover the `CoptStartcomm` and
live-`CoptUpdateparam` cases respectively; two pre-existing validation tests
(`tester_present_time_sub_500us_is_invalid_argument`,
`tester_present_send_type_out_of_range_is_invalid_argument`) needed an explicit
`CP_TesterPresentHandling = 1` added to their setup, since they specifically exercise the
validation this fix now correctly skips when handling is off (a plain ISO15765 CLL's spec default
is `0`).

### Second Codex-review fix: disarm/clear silently dropped an open discard window

The second PR review round (Codex) found that the disarm arm added above (and the pre-existing
`PDU_IOCTL_CLEAR_PERIODIC_MSGS` handler, which has the same shape) unconditionally replaced
whichever `TesterPresentState` it found with a bare `TesterPresentState::None` /
`TesterPresentState::Cleared { resolved, cleared_at }` — silently dropping the `Armed` state's own
`discard_until: Option<DiscardWindow>` (ADR-088/ADR-099) if a `CP_P2Max` response/TX-echo window
from the last arm-time send was still open at that moment. `build_cll_rx_entries` (`events.rs`)
only installs a discard matcher for `TesterPresentState::Armed`, so once the state moved to
`None`/`Cleared`, a delayed ECU response or TX-echo frame from that last send would leak through
to the client as ordinary, unsolicited traffic instead of being discarded — for the remainder of
what should have been a still-active `CP_P2Max` window.

`TesterPresentState::Cleared`'s own doc comment previously claimed there was nothing besides
`resolved` to carry forward from the `Armed` state it replaces ("No hardware resource, interval,
armed_at, last_fired, framed_data, or discard_until to carry") — that claim was itself the bug, not
a reviewed acceptance: `PDU_IOCTL_CLEAR_PERIODIC_MSGS` can race an in-flight send's window exactly
like the live disarm case.

**Fix, as directed by design-advisor** (re-consulted for this exact mechanism):

- A new `ResidualTesterPresentDiscard { window: DiscardWindow, target_can_ids:
  Option<TesterPresentTargetCanIds>, tx_can_id: Option<u32> }` (`service.rs`, near
  `DiscardWindow`) freezes exactly the four things `build_cll_rx_entries`'s `Armed` arm reads to
  build a `TesterPresentDiscard` match (`pos`/`neg` via `window`, `target_can_ids`, `tx_can_id`),
  without carrying the full `resolved`/`framed_data` an `Armed` state needs for a *future* send —
  neither a disarmed nor a cleared CLL has one pending.
- A new `TesterPresentState::Disarmed { residual: Option<ResidualTesterPresentDiscard>,
  disarmed_at: Instant }` variant is the disarm-via-`CP_TesterPresentHandling = 0` counterpart to
  `Cleared`, replacing the bare `TesterPresentState::None` write the disarm arm previously used.
  `disarmed_at` mirrors `Cleared`'s own `cleared_at`, giving `TesterPresentToken` a
  `Disarmed(Instant)` case so post-`.await` staleness rechecks can still distinguish two different
  `Disarmed` instances.
- `TesterPresentState::Cleared` gains the identical `residual: Option<ResidualTesterPresentDiscard>`
  field, populated by `PDU_IOCTL_CLEAR_PERIODIC_MSGS`'s handler (`rpc_misc.rs`) the same way the
  disarm arm populates `Disarmed`'s.
- Both write sites extract `residual` from whichever state is being replaced before overwriting it:
  an `Armed` state with `discard_until: Some(window)` yields `Some(ResidualTesterPresentDiscard {
  window, target_can_ids: resolved.target_can_ids, tx_can_id: <same formula as
  `build_cll_rx_entries`'s own, factored into a shared `tester_present_tx_can_id` helper to avoid
  duplicating it a third time> })`; `Armed` with no open window, `None`/`Disarmed` (disarm arm), or
  `None` (clear handler) all yield `None`; a `Cleared` state being re-disarmed carries its own
  `residual` forward unchanged (the clear-then-disarm ordering this can only be reached through is
  rare but must not panic or silently drop an already-carried residual).
- `build_cll_rx_entries` extends its `match &l.tester_present_state` with a combined `Disarmed { residual, .. } |
  Cleared { residual, .. }` arm, applying the exact same `Instant::now() < residual.window.until`
  liveness filter the `Armed` arm already used, sourced from the residual's own frozen fields
  instead of a live `resolved`/`framed_data`. A residual whose window has expired is filtered out
  the same way an expired `Armed` `discard_until` already was — nothing actively collapses an
  expired residual back to `None`; the next arm/re-arm/reset simply replaces the whole state.

**Why `Disarmed` deliberately does not carry `resolved` the way `Cleared` does**: `Cleared` already
carries a `resolved` baseline for its own, unrelated purpose — the re-arm gate's
`same_wire_behavior` comparison (§ "Where the gate lives" above), so an unrelated later
`CoptUpdateparam` doesn't resurrect a cleared tester-present while a wire-content-changing one still
re-arms it (ADR-093). That must keep working exactly as today. `Disarmed`, by contrast, must carry
**no** `resolved` at all: `handling_enabled` is deliberately excluded from `same_wire_behavior`, so
if a handling-disable transition left a `resolved` snapshot behind as `Disarmed`'s own baseline, a
later `CP_TesterPresentHandling` `0 → 1` promotion would compare its freshly-resolved value against
that stale snapshot, find every other field unchanged, and never re-arm — breaking the spec's
immediate-first-send contract (ADR-084) described above, which this whole ADR exists to implement. `Disarmed` carrying no `resolved` field structurally forces
`handle_update_param`'s re-arm snapshot's `old_resolved` to be `None` on a `0 → 1` promotion out of
`Disarmed` — the same `None` a never-armed `TesterPresentState::None` already produces — which is
what makes `0 → 1` re-enable work correctly after a prior disarm.

Four new tests (`j2534-0404-service/tests/grpc_mock/tester_present_send_type.rs`) cover the fix:
still-open-vs-expired residual pairs for both the live disarm
(`handling_1_to_0_disarm_with_open_window_still_discards_delayed_response` /
`handling_1_to_0_disarm_after_window_expires_delivers_delayed_response`) and
`PDU_IOCTL_CLEAR_PERIODIC_MSGS`
(`clear_periodic_msgs_with_open_window_still_discards_delayed_response` /
`clear_periodic_msgs_after_window_expires_delivers_delayed_response`), plus a regression guard
(`handling_0_to_1_after_disarm_rearms_and_sends_immediately`) confirming a `0 → 1` promotion out of
a genuinely `Disarmed` CLL (not merely never-armed) still sends immediately and re-arms — proving
the "`Disarmed` carries no `resolved`" hazard above does not regress.

### Third Codex-review fix: the disarm arm's own residual-carry match had one missing source arm

The disarm arm's residual-carry `match` (`events.rs`, `handle_update_param`) correctly carried a
residual forward when replacing `Armed` (extracting it from that state's `discard_until`) or
`Cleared` (carrying its own `residual` field forward) — but mapped `TesterPresentState::Disarmed {
.. } => None` for the state-being-replaced case, unconditionally dropping an already-open residual
whenever a SECOND `CoptUpdateparam` landed while `CP_TesterPresentHandling` remained `0` (the
disarm arm's own guard, `Ok(resolved) if !resolved.handling_enabled`, matches on every such
promotion regardless of whether anything else changed — including a promotion that only touches an
entirely unrelated ComParam). A delayed reply/echo still inside the first disarm's residual window
would then leak through as ordinary traffic the moment any second promotion landed while handling
stayed off.

Fixed by folding `Disarmed { residual, .. }` into the same carry-forward arm as `Cleared { residual,
.. }` — a one-line completion of the pattern design-advisor's own second-round decision already
established for `Cleared`, not a new design question: any state already carrying a residual must
propagate it forward when replaced by another `Disarmed`, exactly like `Cleared` already did. A new
test, `handling_1_to_0_repeated_disable_promotion_preserves_open_residual`, disarms once (opening a
residual), then promotes an entirely unrelated ComParam (`CP_Loopback`, this file's established
"unrelated promotion" fixture) a second time while handling stays `0`, and confirms a delayed
response inside the original window is still discarded.

### Fourth Codex-review fix (restructure): open discard windows move out of `TesterPresentState` into a per-CLL list

The fourth PR review round (Codex) found that the second/third fixes' per-variant `residual`
carry-forward (`Cleared::residual`, `Disarmed::residual`) was the wrong abstraction, not merely
missing one more carry-forward arm: a `DiscardWindow`'s lifetime is per-*send* (it opens at that
send and closes at its own `until`, `fired_at + CP_P2Max`), while `TesterPresentState`'s lifetime
is per-*configuration* (it changes on every arm/re-arm/disarm/clear). Forcing per-send data to
survive per-configuration transitions by hand-copying it through each transition arm, one
Codex-review round at a time, was always going to keep surfacing one more missed arm — as it did a
third time (the round-3 fix above). Design-advisor was re-consulted (a fourth time on this exact
mechanism) and gave a concrete restructuring decision rather than another patch.

Two more latent bugs of the exact same class were found in the pre-existing (pre-ADR-137,
ADR-088-era) per-tick sender, `dispatch_due_tester_present` (`events.rs`), whose write-back was a
single-slot overwrite: `*discard_until = send_ok.then(|| DiscardWindow { .. })`:

- (i) A `CoptUpdateparam` that changes only a field excluded from `same_wire_behavior` (e.g.
  `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp`/`CP_P2Max`) never re-arms, so it is
  picked up live by the *next* per-tick send instead (the existing discipline the due-check
  snapshot already re-reads `CP_TesterPresentExpPosResp`/`ExpNegResp`/`CP_P2Max` fresh from Active
  for). That next send's own window used to silently **replace** a still-open prior window with a
  different `pos`/`neg`/`until`, losing the ability to discard a delayed reply to the earlier send.
- (ii) A **failed** send set `discard_until = None` unconditionally, dropping a still-open prior
  window outright — a transient write failure on one tick should never retroactively un-discard a
  still-legitimately-open window from an earlier, successful send.

**Fix, as directed by design-advisor:**

- `LogicalLinkState` gains `open_tp_discards: Vec<ResidualTesterPresentDiscard>` — a per-CLL list,
  independent of `tester_present_state`, that is the sole home for open discard windows going
  forward. `ResidualTesterPresentDiscard` (unchanged shape from the second fix: `{ window:
  DiscardWindow, target_can_ids, tx_can_id }`) simply moves from being embedded in
  `Cleared`/`Disarmed` to living in this list; `Armed.discard_until`, `Cleared.residual`, and
  `Disarmed.residual` are deleted outright. Every other field on all three `TesterPresentState`
  variants (`Armed`'s `resolved`/`interval`/`armed_at`/`last_fired`/`framed_data`; `Cleared`'s
  `resolved`/`cleared_at`; `Disarmed`'s `disarmed_at`) is unchanged — the state-transition
  *semantics* established by rounds 1-3 (which variant is written on which transition, the
  `old_resolved` re-arm-gate derivation including `Disarmed => None`, token identity via
  `armed_at`/`cleared_at`/`disarmed_at`) are unaffected by this restructure; only the discard-window
  *storage* moves.
- All three write sites (`dispatch_due_tester_present`, `handle_start_comm`,
  `handle_update_param`'s re-arm) push into `open_tp_discards`, under the exact same
  `logical_links` lock acquisition and the same re-validation guards (`channel_id`/
  `connect_generation`/token-identity checks) they already used for their `tester_present_state`
  write, instead of threading a `DiscardWindow` through the `TesterPresentState` construction. A
  shared helper, `push_open_tp_discard`, applies the same three-step discipline at every site: (a)
  prune expired entries (`retain(|r| now < r.window.until)`); (b) push a new entry only when the
  send that produced it actually succeeded (never on failure — the direct fix for bug (ii) above);
  (c) apply a defensive cap, `OPEN_TP_DISCARDS_CAP = 8`, dropping the oldest entry first if
  exceeded. The structural bound is "open windows ≤ overlapping `CP_P2Max` spans between P3-gated
  sends," typically 1-2 — the cap should never actually bind in practice, but bounds the list
  unconditionally rather than trusting that invariant to hold under every future change to this
  mechanism.
- The disarm arm (`handle_update_param`) and `PDU_IOCTL_CLEAR_PERIODIC_MSGS`'s handler
  (`rpc_misc.rs`) no longer carry anything forward at all: `open_tp_discards` already lives
  independently of `tester_present_state`, so a plain state write (`Disarmed { disarmed_at }` /
  `Cleared { resolved, cleared_at }`) is correct as-is, and the round-2/3 residual-extraction
  `match` blocks are deleted entirely.
- The read side, `build_cll_rx_entries`, no longer matches on `tester_present_state` to find a
  discard window at all: it filters `LogicalLinkState.open_tp_discards` to `now < window.until`
  and builds a `Vec<TesterPresentDiscard>` from every survivor.
  `CllRxEntry.tester_present_discard` changes shape accordingly, from `Option<TesterPresentDiscard>`
  to `Vec<TesterPresentDiscard>`. `bind_frame`'s step 3 (unchanged rank in the tier scan) now
  discards a frame that matches **any** entry in that list, not "the one discard" — per
  design-advisor: "each entry is an independently elicited send, not a precedence chain," so this
  is an any-match scan, not the first-match-wins discipline the surrounding tier scan otherwise
  uses.
- Teardown: `DisconnectComLogicalLink` (`rpc_link.rs`) now also `.clear()`s `open_tp_discards` at
  the same site, under the same lock, as its `tester_present_state = TesterPresentState::None`
  write — per design-advisor, a full teardown (disconnect, or `DestroyComLogicalLink`, which drops
  the whole `LogicalLinkState` including this field) clears open windows too, unlike a live
  reconfiguration. `CoptStopcomm` (`handle_stop_comm`, `events.rs`) is a deliberate **exception**:
  it resets `tester_present_state` to `None` the same way, but must NOT clear `open_tp_discards`.
  Per design-advisor's explicit decision: ISO 22900-2's `PDU_COPT_STOPCOMM` behavior governs future
  *sends* only, not replies already elicited by sends that already went out — a delayed reply to a pre-stopcomm send is still tester-present garbage that
  should still be discarded, and the window self-expires within its own `CP_P2Max` regardless of
  whether the CLL is still comm-started.

One pre-existing test (`tester_present_reqrsp.rs`'s
`reqrsp_1_mode_0_rearms_and_retargets_on_table_reorder_that_changes_tx_data`) asserted the OLD,
narrower behavior as its second half: that a reorder-triggered re-arm's own fresh send caused the
PRIOR arm-time send's target CAN ID to become deliverable again (not discarded) — a direct artifact
of the rounds 1-3 single-slot design silently dropping the prior window on any Armed→Armed re-arm,
not a reviewed behavioral decision. Updated to assert the new, intended behavior: both the new
target (freshly re-armed) and the old target (from the still-open arm-time-send window) are
discarded for as long as each window individually stays open, matching every other window in
`open_tp_discards` — a delayed ECU reply to a request this CLL actually transmitted is tester-present
noise regardless of what the CLL is newly configured to target.

Six new/extended tests (`j2534-0404-service/tests/grpc_mock/tester_present_send_type.rs`) cover this
restructure: `handling_1_to_0_then_0_to_1_with_changed_signature_still_discards_pre_disable_window`
and `clear_periodic_msgs_then_rearm_with_changed_signature_still_discards_pre_clear_window` (the
`Disarmed`/`Cleared` re-arm variants of the round-4 finding — a delayed reply matching the
pre-disable/pre-clear send's ORIGINAL signature is still discarded after a following re-arm whose
own fresh send carries a different signature);
`continuously_armed_unrelated_field_promotion_still_discards_prior_send_window` (the direct
`dispatch_due_tester_present` bug (i) repro: no `TesterPresentState` transition at all, just two
per-tick sends with different live-read `CP_TesterPresentExpPosResp` values) and its expiry
companion, `continuously_armed_unrelated_field_promotion_windows_expire_then_delivers`. Unit-level
coverage of `push_open_tp_discard` itself (`events.rs`'s `bind_frame_tests` module) confirms the
prune-then-push discipline directly: `push_open_tp_discard_prunes_expired_then_pushes`,
`push_open_tp_discard_none_prunes_but_never_drops_a_still_open_prior_entry` (bug (ii)'s direct
regression guard — an entry is never dropped by `entry = None`, i.e. a failed send); plus
`bind_frame`'s any-entry-match semantics, `tester_present_discard_matches_any_entry_not_just_the_first`.
(The original round included a numeric-cap eviction test here, `push_open_tp_discard_drops_oldest_once_at_cap`
— superseded by the fifth-round fix immediately below, which removes the cap entirely.)

**Known coverage gap, not closed by this round:** an end-to-end integration test reproducing bug
(ii) (a genuinely *failed* `PassThruWriteMsgs` call preserving a still-open prior window) was not
written. **Update:** the underlying mock-infrastructure blocker this note originally cited
(`PassThruWriteMsgs` having no injectable failure mode at all, unlike
`__mock_set_fast_init_error`/`__mock_set_stop_filter_error`/etc. for other native calls) no longer
exists — a `__mock_set_write_msgs_error` hook was added (mirroring those same precedents) to close
a different, unrelated RC21/RC23 test-coverage gap (formerly tracked in
`j2534-0404-service/docs/implementation-notes.md`'s backlog, now closed and removed from it). Bug
(ii)'s own end-to-end test was not written as part of that unrelated fix and remains open here; the
unit-level `push_open_tp_discard_none_prunes_but_never_drops_a_still_open_prior_entry` test above
still covers the same logic at the `push_open_tp_discard` level instead. Writing bug (ii)'s own
end-to-end test using the now-available hook is still a distinct piece of work, out of scope for
this fix; nothing about the tester-present discard-window mechanism itself changed.

### Fifth Codex-review fix: the defensive numeric cap could evict an unexpired entry

The round-4 restructure's `push_open_tp_discard` bounded `open_tp_discards` with a small numeric
cap (`OPEN_TP_DISCARDS_CAP = 8`), FIFO-dropping the oldest entry once at capacity — reasoned at
design time as "the structural bound is ≤1-2 open windows in practice, so 8 should never actually
bind." Codex found a concrete, realistic counterexample: a 5-second `CP_P2Max` with a 300ms
`CP_TesterPresentTime` interval and a `CoptUpdateparam` changing only
`CP_TesterPresentExpPosResp` (a `same_wire_behavior`-excluded field, so no re-arm — each per-tick
send with a *different* live signature pushes a distinct new entry) accumulates more than 8
distinct open windows well before the first one's own `CP_P2Max` deadline — the cap then evicted
the oldest, still-unexpired entry to make room, silently reopening the exact leak this whole
mechanism exists to close (a delayed reply matching the evicted signature would be delivered as
ordinary traffic instead of discarded).

Fixed by replacing the numeric cap with **deduplication by signature**: on push,
`push_open_tp_discard` now first checks whether an existing (unexpired) entry already has the
identical matching signature (`pos`/`neg`/`target_can_ids`/`tx_can_id` all equal — everything but
`until`); if so, it extends that entry's `until` to the later of the two deadlines instead of
appending a duplicate. This bounds the list for the overwhelmingly common case (repeated
identical-signature sends — most `CoptUpdateparam`s change nothing tester-present-relevant, and a
steady-state periodic/idle-triggered sender's own signature is stable between re-arms) without
ever having to choose between "drop an unexpired entry" and "grow unbounded." No cap remains: the
list's actual size is bounded by the number of *distinct* signatures a client causes within one
`CP_P2Max` span, which is unusual but never something this function may respond to by discarding a
still-open, distinctly-signed window — correctness over a defensive bound that has no value it can
safely take.

`push_open_tp_discard_drops_oldest_once_at_cap` (the round-4 cap test) is replaced by
`push_open_tp_discard_same_signature_extends_existing_entry_in_place` (same-signature push extends
in place, list does not grow) and `push_open_tp_discard_never_evicts_distinct_unexpired_entries`
(20 distinct-signature pushes — well past the old cap of 8 — all survive).

## Consequences

- Clients gain the spec's only in-band mechanism to stop a running tester-present keep-alive
  without a full stop/start cycle, closing a real spec-conformance gap: before this ADR,
  `SetComParam(CP_TesterPresentHandling, 0)` + `CoptUpdateparam` was silently a no-op on an armed
  CLL.
- A live `CP_TesterPresentHandling` disarm has no P3-gate wait (unlike the re-arm path's
  non-cancellable `wait_for_p3_gap`, ADR-084) — disarming never transmits, so there is nothing to
  gate.
- ADR-084's "keep-alive beats none" fallthrough remains the governing behavior for every other
  implicit-disable case (`CP_TesterPresentTime == 0`, an unrelated promotion, a resolution `Err`);
  this ADR narrows it by exactly one explicit case, not a general reopening.
- New tests are required (per the design-advisor brief that shaped this decision): (1)
  `CoptStartcomm` with `handling = 0` and non-empty data/interval → `TesterPresentState::None`;
  (2) an `Armed` CLL, `CoptUpdateparam` promoting `handling = 0` → disarmed, no frame sent, no
  error event; (3) `handling` promoted `0 → 1` on a previously-unarmed, comm-started CLL →
  immediate send + arm; (4) an unrelated (`CP_Loopback`-only) promotion on an armed CLL still
  no-ops (regression guard — `handling_enabled` must not spuriously participate in the "did
  anything relevant change" comparison beyond the new leading match arm).

# ADR-084: CP_TesterPresentSendType=1 Sends Its First Frame Immediately on Arm/Re-arm

**Date:** 2026-07-14
**Status:** Accepted (mode-0 exclusion superseded by ADR-093 — see the strikethrough notes below;
everything else in this ADR is unchanged and now applies to mode 0 too; the "keep-alive beats
none" fallthrough this ADR's `handle_update_param` re-arm gate depends on is amended by ADR-137
for the specific case of a live `CP_TesterPresentHandling` transition to `0`, which now disarms
instead of falling through — every other implicit-disable case this ADR covers is unchanged)
**Affects:** `j2534-0404-service` service, events (`handle_start_comm`, `handle_update_param`,
`dispatch_due_idle_tester_present`), rpc_primitive (`resolve_tester_present`,
`ResolvedTesterPresent`), `LogicalLinkState`

## Context

ADR-083 gave `CP_TesterPresentSendType = 1` (idle-triggered, software-driven tester-present)
real effect: `dispatch_due_idle_tester_present` fires a one-shot send once
`CP_TesterPresentTime` has elapsed since `armed_at` (or the last send/bus activity) with no
other bus traffic. That means the *first* tester-present frame for a mode-1 CLL never goes out
until a full `CP_TesterPresentTime` after `CoptStartcomm` completes — plausible when read
literally against ISO 22900-2's description of this mode (transmit only after the bus has been quiet for `CP_TesterPresentTime`), but not what this deployment needs: some ECU sessions expect a keep-alive at (or
immediately after) session start, not only after the first idle window has fully elapsed.

The requirement, confirmed with the requester before implementation (three points were
genuinely ambiguous and were resolved explicitly rather than assumed):

- ~~Applies to `CP_TesterPresentSendType = 1` only. `CP_TesterPresentSendType = 0`
  (`PassThruStartPeriodicMsg`, hardware-autonomous periodic) is unaffected everywhere — the
  requester holds that the J2534 spec already mandates an adapter's first periodic tick at
  start, so no service-side change is warranted there (and ADR-083 already documents that this
  service has no visibility into individual periodic ticks regardless).~~ **Superseded by ADR-093:
  mode 0 no longer uses `PassThruStartPeriodicMsg` at all and now shares this exact immediate-send
  branch with mode 1.**
- Two trigger points count as "became active": `CoptStartcomm`'s own arm, and a live
  `CoptUpdateparam` that promotes tester-present-affecting ComParams to Active while the CLL's
  `comm_started` is already `true` — a session already running that gets reconfigured should
  also see the immediate-send contract, not just a fresh `CoptStartcomm`.

Two design constraints came from the existing subsystem, not from this ADR's own choices:

- `resolve_tester_present` (ADR-067) only ever ran once, synchronously, at `StartComPrimitive`
  call time. There was no precedent for resolving tester-present again later against a live
  Active set, so the `CoptUpdateparam` trigger point needed new machinery, not reuse of an
  existing "live resolution" path (none existed).
- The existing one-shot-send mechanics (`dispatch_due_idle_tester_present`) already carry
  several race-safety patterns ADR-083 spent multiple Codex-review rounds establishing (arm
  identity guards, `count_as_bus_activity = false` to prevent sibling-CLL starvation, teardown
  re-validation). Any new call site reusing "send one idle-mode frame" needed to inherit that
  rigor rather than re-derive it independently.

## Decision

### Shared one-shot-send helper

`send_idle_tester_present_once` (`events.rs`) extracts `dispatch_due_idle_tester_present`'s send
body (`transmit_request` with `count_as_bus_activity = false`, `TxGapState` stamp on success,
`PduErrEvtTesterPresentError` on failure) into a function shared by three call sites: the
existing per-poll-tick dispatch, `handle_start_comm`'s mode-1 arm, and `handle_update_param`'s
new re-arm. It returns the post-send instant as `Ok`/`Err` so every caller can still stamp
`last_fired` on both outcomes (ADR-083's established policy: a failed send backs off a full
interval rather than retrying every ~10ms poll tick).

### `handle_start_comm`: send before arming, non-cancellable

The mode-1 branch now runs a non-cancellable `wait_for_p3_gap` (mirroring the mode-0 branch's
own gate immediately above it, and its "past the point of no return" rationale — by this point
`run_protocol_init` has already put a real ECU handshake on the wire), re-validates the CLL is
still on this physical channel (the same `still_on_this_channel` + first-wins-`PduCopstCancelled`
pattern the mode-0 branch already uses), sends via `send_idle_tester_present_once`, and only then
constructs `TesterPresentState::Idle` with `armed_at: fired_at, last_fired: Some(fired_at)`
(previously `armed_at: Instant::now(), last_fired: None`). Step 3's existing single-critical-
-section channel-identity re-check and write-back (`comm_started`/`tester_present_state`) is
unchanged and covers this new path for free.

A new `LogicalLinkState.tester_present_base_tx_flags: u32` field, stamped at that same Step 3
write-back from a new `ResolvedTesterPresent.base_tx_flags` field, persists the client's
`base_tx_flags` from `CoptStartcomm`'s own `cop_ctrl_data` — needed because a later
`CoptUpdateparam`'s own `cop_ctrl_data` carries no TxFlags meaningful to a persistent
tester-present.

### `handle_update_param`: diff-gated live re-arm

On a successful Working→Active promotion, `handle_update_param` now — within the same lock
scope as the `link.active = params` write, so no `.await` sits between the promotion and the
snapshot it gates on — captures `channel_id`/`comm_started`/`protocol`/`software_isotp`/
`tester_present_base_tx_flags`, a cheap `TesterPresentToken` (`None` / `Idle(armed_at)` /
`Periodic`) identifying the CLL's current tester-present state, and (when currently `Idle`) a
clone of its stored `resolved: ResolvedTesterPresent`.

The re-arm gate, evaluated after the lock is released:

1. ~~Skip entirely if `channel_id` no longer matches this channel, `comm_started` is `false`, or
   the token is `Periodic` — mode 0 is out of scope everywhere, including here; an already-
   -running `Periodic` is never stopped/restarted/re-intervaled by this hook.~~ **Superseded by
   ADR-093: the `Periodic` token and its exclusion no longer exist — skip only on `channel_id`
   mismatch or `comm_started == false`; mode 0 now re-arms through this same gate.**
2. Skip `resolve_tester_present` entirely (not just the send) when the token is `None` and the
   newly-promoted set's tester-present payload is still empty — a CLL that never configured
   tester-present and still hasn't must not be affected by a stray out-of-range
   `CP_TesterPresentSendType` value sitting unused in the Active set (`SetComParam` does not
   range-check it; `resolve_tester_present` validates `send_type`/interval unconditionally, even
   for an empty payload). Skipping the call avoids a spurious `PduErrEvtTesterPresentError` on
   every future promotion for a CLL tester-present was never meant to affect.
3. Otherwise, resolve tester-present from the newly-promoted Active set. If the result is
   `Ok` with `send_type == 1`, non-zero interval, and it describes different on-wire behavior
   than what's currently armed (`ResolvedTesterPresent::same_wire_behavior`, comparing `data`/
   `interval_ms`/`tx_flags`/`isotp_framing`/`send_type`/`can_functional` — deliberately excluding
   `base_tx_flags`, which is bookkeeping, not wire content) — or the CLL had no tester-present
   armed at all (token `None`, transitioning to enabled) — proceed exactly like
   `handle_start_comm`'s new path: non-cancellable P3 gate, re-validate via the same token
   (channel/`comm_started`/state-identity unchanged), send, then write back a freshly-armed
   `Idle` under the same re-validated guard.
4. If the resolution is unchanged from what's currently armed, `Ok` but mode 0/disabled, or the
   CLL had no tester-present armed and the newly-promoted payload is still non-empty-but-
   -unresolvable in a way caught by step 2's guard not applying — leave `tester_present_state`
   untouched. An `Err` result (a genuine resolution failure on a CLL that does have real
   tester-present configured or armed) emits `PduErrEvtTesterPresentError`.

**Why the diff gate (step 3's "different on-wire behavior" check) exists — this was not part of
the original implementation and was added after review:** without it, *any* successful
`CoptUpdateparam` on a comm-started, mode-1 CLL — including one promoting a completely unrelated
ComParam such as `CP_Loopback` — re-resolved and re-armed unconditionally, re-sending a frame and
resetting the idle clock every time. Confirmed by a repro (`SetComParam(LOOPBACK, 1)` +
`CoptUpdateparam` on an already-armed mode-1 CLL produced a second, spurious frame) and pinned by
`send_type_1_unrelated_updateparam_does_not_resend_or_rearm`
(`tests/grpc_mock/tester_present_send_type.rs`); the positive case — a `CoptUpdateparam` that
*does* change tester-present's resolved message — is pinned by
`send_type_1_relevant_updateparam_resends_and_rearms`. Beyond the extra frame, the unconditional
version also stalled the whole physical channel's TxItem queue (every other CLL's queued COPs)
for up to a full P3 gap on *any* ordinary param update, via the new non-cancellable
`wait_for_p3_gap` — ordinary `CoptUpdateparam` traffic previously had no post-promotion wait at
all.

## Consequences

- Mode-1 tester-present's first frame now goes out synchronously as part of `CoptStartcomm`
  (before `PduCopstFinished`) instead of up to `CP_TesterPresentTime` later — a deliberate,
  requester-confirmed divergence from a literal reading of ISO 22900-2's "idle for
  `CP_TesterPresentTime`" wording for the *first* frame only; every subsequent send is still
  genuinely idle-triggered.
- `TesterPresentState::Idle.last_fired` is `Some` from the instant the state is constructed —
  there is no longer a window where a CLL is armed but has never sent. Existing consumers that
  matched on `last_fired.is_none()` to mean "never armed" would now be wrong to do so; none exist
  in this codebase today (`dispatch_due_idle_tester_present`'s own anchor logic,
  `last_fired.unwrap_or(armed_at)`, is unaffected since `armed_at` and the immediate `last_fired`
  are the same instant on construction).
- `resolve_tester_present` is no longer called from exactly one place. Its doc comment is
  updated; any future reader relying on "never re-derived here" (a real invariant elsewhere in
  this codebase, e.g. ADR-067's `CoptSendrecv`/`CoptStartcomm` call-time binding) must check
  which call site they mean.
- **Accepted limitation, not a further fix:** `handle_update_param`'s new P3 gate is
  non-cancellable, extending `handle_start_comm`'s mode-0 "past the point of no return"
  rationale to a case where the underlying premise is weaker — the CLL already has a (possibly
  stale but valid) `tester_present_state` before the update, so skipping the re-arm would not
  leave the ECU session with literally no keep-alive, unlike mode-0's original justification.
  Concretely: a `CancelComPrimitive` on an ordinary `CoptUpdateparam` COP can now be blocked for
  up to one P3 gap during the new re-arm's wait, where before this ADR `CoptUpdateparam` had no
  post-promotion wait at all. This is judged acceptable (bounded by one P3 gap, and only on the
  rarer path where tester-present's own resolution actually changed) rather than reopening
  `wait_for_p3_gap`'s cancellation semantics for this one call site.
- The diff gate (`same_wire_behavior`) requires `IsoTpFraming` and `SoftIsoTpFraming` to derive
  `PartialEq`/`Eq` (previously `Debug, Clone, Copy` only) — both are plain `bool`/`u8`/enum
  fields, so this is a free, behavior-preserving addition.
- The new `handle_update_param` branch (~150 lines) was completely untested before this ADR's
  review pass: every pre-existing `CoptUpdateparam` test in this suite promotes before
  `CoptStartcomm`, so `comm_started` was always `false` at promotion time in every prior test.
  Two new tests exercise it directly (see Decision above); no other test in the suite currently
  reaches this branch's `Ok(_)` mode-0/disabled no-op arm or its `Err` arm.

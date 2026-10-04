# ADR-161: `PDU_IOCTL_RESET` Generation-Scoping Across Its Post-Snapshot Phases

**Date:** 2026-08-06
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_reset`), `j2534-0404-service/src/service/events.rs` (`cancel_held_tx_items`)

## Context

ADR-147's eighth amendment fixed a race in `ioctl_resume_tx_queue` where a same-`cll_handle`
disconnect+reconnect completing between that RPC's `connect_generation` capture and its later
mutation could clear a brand-new session's suspend flags and misdirect a wake to the old session's
TX queue. That amendment's "Not extended" paragraph deliberately deferred an equivalent, confirmed
bug in `PDU_IOCTL_RESET` (`ioctl_reset`, `rpc_misc.rs`), on the grounds that resolving it required a
design decision this ADR now makes.

`ioctl_reset` operates module-wide: it snapshots every currently-connected CLL's identity in one
`logical_links` lock acquisition, then runs three further phases against that snapshot, each
separated from the others (and from the snapshot) by `.await` points where a concurrent
disconnect+reconnect of the same `cll_handle` can complete:

1. **Hardware teardown** — per target, stops each tracked client message filter
   (`PassThruStopMsgFilter`) and clears the RX/TX buffers (`PassThruClearRxBuf`/`ClearTxBuf`) via the
   snapshotted `channel_id`.
2. **TX-item cancellation** — calls `events::cancel_held_tx_items(cll_handle, reset_suspended: true)`,
   which re-acquires `logical_links` itself, looks up `cll_handle` fresh (not gated on any
   generation), unconditionally clears `tx_suspended_by_ioctl`/error-suspension, and drains+cancels
   every held `tx_held` item with `PduCopstCancelled`.
3. **Filter-tracking writeback** — re-acquires `logical_links` and rewrites `client_filters` per
   target from the teardown phase's success/failure results.

If the same `cll_handle` disconnects and reconnects between the snapshot and any of these phases,
each phase acts on the RECONNECTED session's live `LogicalLinkState`, not the one RESET actually
observed: phase 2 destructively cancels the new session's own already-queued work and emits a
spurious `PduCopstCancelled` to a client that never asked for it; phase 1 risks touching a numeric
`ChannelId` the vendor DLL has since recycled for an unrelated physical channel (ADR-101 Decision
§E already documents this reuse); phase 3 can silently drop tracking for a live filter the new
session installed.

The deferred design question: should `PDU_IOCTL_RESET`, despite being module-wide, be scoped per
CLL to the specific connection generation it snapshotted — or is it intentionally meant to act on
"whatever is live right now" at each phase?

## Decision

**`PDU_IOCTL_RESET` linearizes at its `targets` snapshot.** Every post-snapshot phase is gated on
`connect_generation` captured in that snapshot; a target whose live generation has since advanced
is skipped by that phase entirely, as if `PDU_IOCTL_RESET` had completed an instant before the
reconnect. This is the only framing under which RESET's own observation (the snapshot) and its
mutations (the three phases) describe a single consistent point in time — reaching forward to
mutate a session RESET never observed is not a valid serialization of "RESET happened" and "the
client reconnected" in either order, regardless of which one a client would consider to have
"really" happened first. It is also this codebase's established answer at every other cross-`.await`
clear site (ADR-086; ADR-147's own eighth amendment; the `CoptUpdateparam` promotion path).

Mechanism, per phase:

- **Snapshot (`ResetTarget`).** Add `connect_generation` alongside the existing `cll_handle`/
  `channel_id`/`filters`/`rx_buf` fields, read in the same single `logical_links` acquisition.

- **Phase 1 (hardware teardown) additionally requires serializing against shared-channel joins,
  not just against the target's own reconnect.** `api`-first ordering (reordering the lock
  acquisition to `api` first, `logical_links` second — the sanctioned nesting this crate already
  uses elsewhere, e.g. `rpc_link.rs`'s connect/disconnect paths) closes the window against the
  target's own channel being torn down or recreated, since every `PassThruConnect`/
  `PassThruDisconnect` also goes through `api`. It does NOT close the window against a SIBLING CLL
  joining the target's already-open, shared physical channel (ADR-023/ADR-156's channel-sharing
  model): joining an existing `SharedChannel` bumps `ref_count` and calls `finalize_connected_link`
  (which publishes the joining CLL's own fresh `connect_generation`) entirely under
  `shared_channels`, never touching `api` at all — there is no fresh `PassThruConnect` to serialize
  on when the channel already exists. A join completing in this window means Phase 1's
  channel-wide `PassThruClearRxBuf`/`ClearTxBuf` would wipe a session RESET never observed, in
  direct violation of this ADR's own linearization decision — with no lock contention at all,
  since checking only the snapshotted target's own fields says nothing about a sibling.

  The fix serializes against both hazards together, grouped per physical channel rather than per
  target (multiple `ResetTarget`s can share one `channel_id`, the exact scenario this closes):
  acquire `shared_channels` first (outermost of the three per ADR-080 — the same lock
  `disconnect_com_logical_link`'s filter-teardown/ref_count sequence already holds for this exact
  reason), then `api`, then a brief `logical_links` critical section that rechecks each of this
  channel's targets' own `connect_generation`/`channel_id` as before. Per-CLL filter stops still
  run for every target that matches; the channel-wide RX/TX buffer clears run at most once per
  channel, and only when the channel's occupancy is unchanged since the snapshot (below) —
  skipping the clears (never the filter stops) when it isn't, since clearing would destroy a
  session RESET never observed.

  **The staleness signal for channel occupancy is a per-`SharedChannel` occupancy epoch, not a
  scan of `LogicalLinkState` for generation mismatches.** A `u64` epoch, stamped from one
  service-wide monotonic counter, is recorded on `SharedChannel` at creation and re-stamped on
  every `ref_count` increment — the primary-connect join (`finalize_connected_link`) and the
  UUDT-companion join (`ensure_uudt_companion_channel`) alike, both already inside a
  `shared_channels` critical section, so no source change is needed to make either publish the
  epoch. A first implementation of this occupant check instead scanned live `LogicalLinkState`s for
  a `connect_generation` not present in the snapshot; two further review rounds found this
  insufficient: the UUDT-companion join attaches a CLL to a shared channel via
  `link.uudt_channel_id` without ever changing `connect_generation` at all -- true independent of
  lock timing, and reason enough on its own that a `logical_links` generation scan could never be
  race-free (it also, at the time, published that field only *after* releasing `shared_channels`;
  see the dedicated-follow-up Correction note below for why that second reason no longer applies).
  `ref_count`-increment is the one choke point every occupancy
  change must pass through, under `shared_channels`, regardless of mechanism — including one not
  yet written — which is what makes the epoch a structurally complete signal rather than another
  per-path enumeration.

  RESET's snapshot acquires `shared_channels` then `logical_links` together (still the sanctioned
  order) and additionally records each live `SharedChannel`'s `(channel_id, occupancy_epoch)`.
  Phase 1's per-channel critical section (still `shared_channels` → `api` → brief `logical_links`)
  compares the live `SharedChannel`'s current epoch against the snapshotted one for that
  `channel_id`; a mismatch (or the entry no longer existing) skips the channel-wide clears.
  Decrements deliberately do NOT bump the epoch: releasing an occupant cannot introduce a session
  RESET never observed, and every occupant still attached predates the snapshot — mirroring how a
  plain disconnect is already treated by the per-target recheck. A leave-and-rejoin, or a
  numerically-recycled `channel_id` on a torn-down-and-recreated entry, still mismatches, because
  the epoch is drawn from one global counter, never reset per-channel. Holding `shared_channels`
  across Phase 1's read closes the window against a join *completing* during it; the epoch
  comparison catches one that already completed *before* Phase 1 reached this channel. Both are
  necessary; neither alone is sufficient.

  **A join that ROLLS BACK (the CLL is destroyed after `ref_count` is bumped but before the
  publish that would make it a real occupant) must restore the pre-join epoch, not leave it
  elevated — via compare-and-restore, not a blind restore.** Each join site captures its own
  `(stamped, previous)` pair when it stamps the epoch; its rollback path restores `previous` only
  if the live epoch still equals its own `stamped` value — i.e. only if no OTHER join has stamped a
  newer epoch in between. At the time this ADR was written, this was load-bearing specifically for
  the UUDT-companion path, where the join's own `ref_count` bump and its rollback were two separate
  `shared_channels` critical sections (the bump inside `ensure_uudt_companion_channel`, the rollback
  inside `release_shared_channel_ref`) with a real gap between them a sibling join could land in: a
  blind (non-compared) restore there would erase that sibling's evidence, producing exactly the
  unsafe-clear outcome this whole mechanism exists to prevent — worse than the bug it was fixing.
  (The primary-connect path's own stamp and rollback share one continuously-held `shared_channels`
  guard, so a blind restore would happen to be safe there alone, but a mechanism safe on only one of
  two paths is the wrong shape; compare-and-restore is applied uniformly at both.) A dedicated
  follow-up later closed that gap for the UUDT-companion path too (see the Correction note below),
  so compare-and-restore is no longer load-bearing on either path today — but it is retained
  unchanged as defense-in-depth, and the reasoning above still explains why it was applied uniformly
  rather than only where it was strictly necessary at the time. Global counter uniqueness (no epoch value is
  ever reallocated) is what makes the equality compare a genuine proof of "no other stamp is live,"
  not merely a heuristic. Without this rule, a rolled-back join leaves its channel's epoch
  permanently elevated relative to any RESET snapshot taken before the failed join, causing that
  RESET to skip the channel-wide clears even though no new occupant survived — safe-direction (a
  hygiene shortfall, never data loss), but avoidable, and cheap enough to fix and unit-test directly
  against the epoch predicate rather than carry as a residual.

  **Accepted residual at the time this ADR was written — overlapping rollbacks completing in FIFO
  order strand the epoch at an intermediate, dead value instead of fully unwinding (see the
  Correction note below: a later dedicated follow-up made the scenario below structurally
  unreachable, not merely safe).** Example: a join stamps E1 over E0; a
  second, overlapping join stamps E2 over E1; the FIRST join then rolls back (compares its own
  stamp E1 against the live value E2 — no match, so it correctly declines to restore, per the
  compare-and-restore rule above); the SECOND join then rolls back (compares E2 against live E2 —
  matches, restores to E1). Final state: E1, not the true pre-either-join value E0. This is
  provably safe-direction, not merely assumed so: `occupancy_epoch` values are unique monotonic
  allocations, never reallocated, so a stranded value like E1 can only ever match a RESET snapshot
  taken WHILE E1 was genuinely live (i.e. after the first join's stamp, before its own rollback) —
  and at that instant the channel's real occupant set was exactly "whatever predates E1," every
  member of which the snapshot legitimately observed. Every other snapshot value simply mismatches
  E1 and skips, same as any other stale epoch. The strand is also self-healing: the next RESET's
  snapshot records the stranded value itself, so only a RESET whose snapshot-to-Phase-1 span
  straddles both the join and its rollback is ever affected. This scenario additionally requires
  the FIRST-rolled-back join to be a UUDT-companion join specifically — a primary-connect join's
  rollback runs under the same `shared_channels` guard held continuously since its own stamp, so
  its compare can never decline — making the full trigger three independent rarities compounding
  (two overlapping UUDT-companion joins on one channel, both destroyed before publishing, rolled
  back in that specific order, straddled by a module-wide RESET) for one skipped hygiene clear.

  A lossless fix was evaluated and rejected as disproportionate: tracking only in-flight
  (stamped-but-unpublished) joins per channel and removing an entry on EITHER outcome (success or
  rollback) would close this, but the UUDT-companion success path publishes `uudt_channel_id` after
  releasing `shared_channels` and inside a `logical_links` section — removing its pending-join entry
  there would mean acquiring `shared_channels` after `logical_links`, violating this codebase's
  `shared_channels`-outermost ordering (ADR-080), or restructuring `ensure_uudt_companion_channel`
  to publish under one continuously-held `shared_channels` guard (rejected earlier in this same ADR
  for the unrelated pre-existing atomicity gap, for the same reason: a delicate restructure of a
  different function, out of scope here). Buying a lossless fix would add new concurrency surface to
  the exact mechanism three review rounds have already found subtle gaps in, to eliminate a residual
  that is benign, self-healing, and requires three compounding rarities to trigger. Pinned by the
  `rollback_does_not_erase_a_surviving_siblings_newer_epoch` test, which asserts the E1-not-E0
  landing explicitly rather than merely asserting "no erasure."

  **Correction (dedicated follow-up, closing the `ensure_uudt_companion_channel` backlog item this
  ADR recorded in its Consequences section):** the *simpler* of the two restructures this paragraph
  discusses — publishing under one continuously-held `shared_channels` guard, not the rejected
  lossless in-flight-tracking alternative — was completed as its own change, exactly as this ADR's
  Consequences section anticipated. `ensure_uudt_companion_channel` now holds `shared_channels`
  continuously from its `ref_count`/epoch bump through either the `uudt_channel_id` publish or (via
  `rollback_channel_join`'s already-locked-guard variant) the destroy-while-joining rollback, the
  same shape `finalize_connected_link` already had for the primary-connect join. This closes both
  the underlying hard-error/occupant-sweep gap described in the Consequences section below, and, as
  a side effect, makes the "Accepted residual — overlapping rollbacks completing in FIFO order"
  scenario above structurally unreachable, not merely safe: that scenario needed two UUDT-companion
  joins on the SAME channel to overlap (the second stamping E2 while the first's own bump-to-rollback
  span is still open), which required the bump and rollback to be two separate `shared_channels`
  critical sections with a released-lock gap between them for a second join to land in. Since
  `shared_channels` is one mutex over the whole map, a second join -- to this channel or any
  other -- can no longer even begin its own bump until a call already inside
  `ensure_uudt_companion_channel` has fully resolved (published or rolled back) and dropped `chans`;
  the two-overlapping-joins precondition this scenario's whole three-rarities compounding was built
  on no longer exists for either join path. The compare-and-restore mechanism itself is unchanged and
  untouched by this follow-up: it remains correct, now serving as defense-in-depth rather than as the
  load-bearing protection this paragraph originally described.

  Checking `channel_id` in addition to `connect_generation` for the per-target recheck still
  matters: a plain disconnect with no reconnect leaves the generation unchanged but `channel_id`
  becomes `None`, and a reconnect can in principle land the same recycled numeric ID on a different
  generation.

  The RX-buffer clear (`rx_buf.items.clear()`) is gated separately, on `connect_generation` alone,
  inside a `logical_links` critical section (no `channel_id` check — an unconnected CLL legitimately
  has `channel_id: None` and its queue is still cleared today, a behavior this ADR preserves): the
  `rx_buf` `Arc` persists unchanged across reconnect, so clearing it unconditionally would wipe a
  new session's already-queued events even without a channel-identity race.

- **Phase 2 (TX-item cancellation).** `events::cancel_held_tx_items` gains an
  `expected_generation: Option<u64>` parameter. Inside its existing single `logical_links` critical
  section, a `Some(g)` that disagrees with the live `connect_generation` skips the drain/cancel/flag
  clear entirely — check and mutation stay atomic in one acquisition, the same shape as ADR-147's
  eighth amendment. `ioctl_reset` passes the snapshot generation; `ioctl_clear_tx_queue` and
  `cancel_link_cops` (the function's other two call sites) pass `None`, since neither has a
  cross-`.await` staleness window of its own (single acquisition, or the link is already being torn
  down).

- **Phase 3 (filter-tracking writeback).** Skip a target entirely on generation mismatch inside the
  final `logical_links` acquisition. A reconnected link's `client_filters` originates from
  `pending_client_filters` at connect time, not from anything RESET's snapshot could have tracked,
  and the old session's filters were already drained and stopped by its own disconnect path — so
  skipping orphans nothing.

  While implementing this phase, a second, independent bug in the SAME critical section was found
  and fixed in this pass: the pre-existing writeback used `client_filters.insert`/`.remove` keyed
  only by filter number, which — even on a generation MATCH — deletes tracking for a live filter a
  client installed via `PDU_IOCTL_START_MSG_FILTER` between the snapshot and this phase, if it
  happens to reuse a filter number this phase's snapshot also covered. Fixed by making the writeback
  set-difference-based (retain `live_ids − snapshot_ids`, then add back any ids the teardown phase
  recorded as failed-to-stop) instead of unconditional insert-or-remove. The generation gate is kept
  regardless — a DLL-allocated `MessageFilterId` can also be recycled across a reconnect, so a
  cross-generation set-difference would still be unsound.

A CLL created after the snapshot is untouched by this RESET call by construction (it isn't in
`targets`) — this is intended, not an oversight, and follows directly from the same linearization
argument: RESET cannot reach forward to affect a CLL it never observed.

This is a fresh ADR rather than a ninth amendment to ADR-147: only phase 2 touches that ADR's actual
subject (TX-suspend sources); phases 1 and 3 are channel-identity and filter-tracking correctness,
outside its scope. ADR-147's own eighth-amendment "Not extended" paragraph is updated with a
one-line pointer to this ADR (a factual cross-reference — nothing in ADR-147's Decision is
superseded).

## Consequences

- A CLL that reconnects mid-`PDU_IOCTL_RESET` keeps its own already-queued TX items, its own
  suspend-flag state, and its own live message filters exactly as if RESET had completed
  microseconds earlier — no data loss, no spurious cancellation notification, no orphaned filter
  tracking.
- Phase 1's hardware teardown now holds `shared_channels`, then `api`, for the duration of the
  generation/channel/epoch check plus the teardown calls themselves, per channel — a bounded,
  per-channel lock hold matching precedent already established by `disconnect_com_logical_link`'s
  and `handle_channel_hard_error`'s comparable (or longer) holds of the same lock, not a new
  lock-ordering edge or a module-wide hold. `SharedChannel` gains an `occupancy_epoch: u64` field
  and the service gains one more service-wide monotonic counter (alongside the existing
  `next_connect_generation`), both stamped only at sites already inside a `shared_channels`
  critical section.
- **Accepted residual — a post-snapshot join landing before Phase 1 reaches that channel skips the
  channel-wide buffer clears entirely** (filter stops for the matching target(s) still run). This
  leaves some stale adapter-buffer frames behind for the observed, generation-matching target(s) on
  that channel — a benign hygiene shortfall, not data loss or misrouting, and strictly preferable to
  the alternative of destroying the unobserved joiner's traffic.
- **Accepted, pre-existing behavior, not introduced by this ADR — companion-only channels are
  outside Phase 1's clear scope.** `ResetTarget`'s snapshot (and the channel grouping built from it)
  is keyed by each target's own `channel_id`, never by a `uudt_channel_id` companion — a physical
  channel serving only UUDT companions, with no raw-CAN primary occupant in the snapshot, was never
  buffer-cleared by RESET before this ADR either. Recorded here so a future audit does not re-file
  it as a gap this ADR should have closed.
- **A separate, pre-existing bug found while tracing this mechanism — fixed by a dedicated
  follow-up, as the entry recorded in `j2534-0404-service/docs/implementation-notes.md`'s backlog
  anticipated**: `ensure_uudt_companion_channel` used to bump `ref_count` under `shared_channels`,
  then release that guard before re-acquiring `logical_links` to publish `uudt_channel_id` —
  unlike `finalize_connected_link`'s deliberately-held-through-both-steps pattern for the primary
  join. This occupancy epoch's own correctness never depended on that publish timing (the epoch is
  stamped at the `ref_count` bump itself, already inside `shared_channels`), but a hard error
  landing on the channel in that same gap could have let `handle_channel_hard_error`'s occupant
  sweep (which matches on `uudt_channel_id`) miss the joining CLL, which would then publish a
  companion onto a channel already marked dead. Fixed by holding `shared_channels` through the
  publish and using `rollback_channel_join`'s already-locked-guard variant (instead of
  `release_shared_channel_ref`, which would now self-deadlock) on the destroy-while-joining
  rollback path, mirroring `finalize_connected_link`'s borrowed-guard pattern.
- The generation-and-set-difference writeback in phase 3 fixes a same-generation bug (untracked live
  filter on filter-number reuse) that exists independently of this ADR's core race; it is fixed here
  because it lives in the exact code this ADR is already restructuring.
- **Accepted residual — untestable interleaving, narrowed by the epoch's unit-testability.** As with
  ADR-147's eighth amendment, no hook in this test harness can pause an in-flight `ioctl_reset` at a
  specific cross-`.await` point to inject a concurrent reconnect or channel join deterministically.
  `cancel_held_tx_items`'s `expected_generation` parameter and Phase 1's occupancy-epoch predicate
  are both directly unit-testable in isolation against a bare `shared_channels`/`logical_links` map
  (mismatched generation/epoch → assert the guarded mutation is skipped; a decrement alone → assert
  it is NOT treated as a mismatch) and both are required in this PR; the remaining lock-ordering and
  critical-section-boundary properties are verified by code inspection plus an `edge-case-hunter`
  pass, matching the eighth amendment's own precedent.
- Links created after `ioctl_reset`'s snapshot are untouched by that RESET call. This is intended
  behavior under this ADR's linearization framing, recorded here so a future audit does not re-file
  it as a gap.

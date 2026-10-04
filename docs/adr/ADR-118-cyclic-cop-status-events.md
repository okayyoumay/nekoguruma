# ADR-118: Cyclic CoptSendrecv Status Events — EXECUTING/WAITING on Every Cycle Boundary

**Date:** 2026-07-23
**Status:** Accepted
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`handle_send_recv`, `dispatch_tx_item`, `SendRecvCycle`)
- `j2534-0404-service/src/service.rs` (`TxItem::SendRecv`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`StartComPrimitive`)

## Context

ISO 22900-2:2009(E) §9.2.6.2.3 (line 646) and §9.4.17.2.2 b)/c)/d) (lines
2255-2265) require a `PDU_COPST_WAITING`/`PDU_COPST_EXECUTING` status
**event** — not merely a queryable status — on every EXECUTING→WAITING and
WAITING→EXECUTING transition of a periodic (cyclic) `CoptSendrecv`
ComPrimitive, between cycles. §9.4.17.2.2 d) additionally makes clear that no
such WAITING event precedes the final FINISHED transition — the last cycle
goes straight from EXECUTING to FINISHED.

ADR-117 fixed the *polling* side of this (`GetStatus(cop)` now correctly
distinguishes `PDU_COPST_IDLE` from `PDU_COPST_WAITING`, closing conformance
finding B26) but explicitly deferred the *event* side as finding **A2-24**,
out of scope for that ADR: prior to this ADR, `handle_send_recv` emitted
`PduCopstExecuting` only once, guarded by `!is_continuation` (i.e. only on a
COP's very first cycle), and no code path ever emitted `PduCopstWaiting` as
a `SubscribeEvent` notification at all — only `GetStatus` polling reflected
a cyclic COP resting between cycles. A `SubscribeEvent`/notification-driven
client (as opposed to a client that polls `GetStatus`) never saw the
cyclic EXECUTING↔WAITING pair the spec requires.

## Decision

Two insertion points, both in `j2534-0404-service/src/service/events.rs`:

1. **WAITING→EXECUTING (cycle start).** `handle_send_recv` now emits
   `PduCopstExecuting` at the top of every cycle — the first cycle and every
   subsequent continuation cycle alike. The `is_continuation` field that
   previously guarded this emission (suppressing it on cycle 2+) was the
   bug's own embodiment, so it is removed entirely — from `SendRecvCycle`,
   the continuation constructor, both `TxItem::SendRecv` match arms,
   `TxItem::SendRecv` itself (`service.rs`), and its sole constructor
   (`rpc_primitive.rs`). There is only one emitter per cycle (this call), so
   removing the guard carries no double-emit risk.

   The emission is **atomically gated on the COP still being live in
   `primitives`** (post-commit fix, Codex P1 finding), the same pattern and
   deadlock argument as insertion point 2's WAITING gate below:

   ```rust
   {
       let prims = ctx.primitives.lock().await;
       if !prims.contains_key(&cop_handle) {
           return None;
       }
       send_cop_status(&ctx.subscriptions, cll_handle, cop_handle,
           PduComPrimitiveStatus::PduCopstExecuting).await;
   }
   ```

   No lock is held anywhere earlier in `handle_send_recv` at the point this
   runs, so without this gate a concurrent `DisconnectComLogicalLink` (via
   `cancel_link_cops`) could remove the `primitives` entry and emit
   `PduCopstCancelled` between `should_skip_cancelled_item`'s dequeue-time
   pass and this emission, producing a terminally-inverted
   `WAITING → CANCELLED → EXECUTING` sequence. Holding the `primitives` guard
   across the `contains_key` check and the `send_cop_status` call makes the
   two one atomic critical section with respect to every CANCELLED-emitting
   path, exactly as insertion point 2 already does for WAITING: whichever
   side wins the `primitives` lock determines the order, so EXECUTING can
   never be observed after CANCELLED for the same COP. A COP already absent
   from `primitives` means a concurrent disconnect already cancelled it and
   emitted its terminal status — returning `None` here bails before touching
   hardware, with nothing left to clean up. S1's existing staleness recheck,
   just below, is unchanged and needs no change: it still handles the
   ordinary case where a disconnect lands *after* this gate's atomic
   check-and-emit (i.e. after EXECUTING has already validly gone out) —
   `still_on_this_channel` goes false, and its own
   `primitives.remove(...).is_some()` gate correctly finds the entry already
   gone (removed by the disconnect) and suppresses a duplicate CANCELLED,
   exactly as before this fix. What this gate closes is only the narrower
   window this ADR's Consequences section describes: a disconnect landing
   *before* the atomic check-and-emit, which previously let a spurious
   EXECUTING through after CANCELLED had already fired. This is the same
   deadlock-free edge already established for insertion point 2 below
   (`primitives -> subscriptions`, acyclic, see that analysis).

2. **EXECUTING→WAITING (cycle end).** `dispatch_tx_item`'s tail, immediately
   after `*ctx.executing_cop.lock().await = None;` and before the existing
   no-continuation cleanup block, gained (as amended by the round-6 fix
   below):

   ```rust
   if continuation.is_some() {
       let mut links = ctx.logical_links.lock().await;
       let was_cancelled = links.get_mut(&item_cll_pre)
           .map(|l| l.cancelled_cops.remove(&item_cop))
           .unwrap_or(false);
       let mut prims = ctx.primitives.lock().await;
       if was_cancelled {
           continuation = None; // never park the cancelled cycle
           if prims.remove(&item_cop).is_some() {
               send_cop_status(&ctx.subscriptions, item_cll_pre, item_cop,
                   PduComPrimitiveStatus::PduCopstCancelled).await;
           }
       } else if prims.contains_key(&item_cop) {
           send_cop_status(&ctx.subscriptions, item_cll_pre, item_cop,
               PduComPrimitiveStatus::PduCopstWaiting).await;
       }
       // `links` and `prims` guards drop here, before anything below this
       // block runs.
   }
   ```

   `continuation.is_some()` is true exactly when `handle_send_recv` produced
   a `CycleContinuation` (more cycles remain) — verified to be the single
   construction site in the function (the `remaining == 0` branch emits
   `PduCopstFinished` and returns `None` instead; every other early return in
   `handle_send_recv` also returns `None`, for a stale channel, cancellation,
   TX failure, or ADR-100 tier-2 detachment — never a `CycleContinuation`).
   So this gate never fires on the FINISHED path, the stale-channel
   CANCELLED path, or a tier-2 detachment.

   This is now a **three-way atomic decision**, not the original two-way
   `contains_key`-then-emit (round 6, explicit-cancel-vs-WAITING fix — see
   Consequences): (a) `cancelled_cops` set — an explicit
   `CancelComPrimitive` landed since `handle_send_recv`'s own S4 recheck
   already ran and found nothing; the continuation is dropped (never parked)
   and CANCELLED is emitted instead of WAITING; (b) `cancelled_cops` clear
   and the COP still present in `primitives` — the ordinary live case,
   WAITING is emitted; (c) neither — a concurrent teardown
   (`cancel_link_cops`/`cancel_held_tx_items`/`should_skip_cancelled_item`)
   already removed the entry and emitted its own CANCELLED, so nothing is
   emitted here. `logical_links` is acquired first and held across the
   entire decision (through the `primitives` lock and the send), because
   `rpc_cancel_com_primitive`'s `cancelled_cops` insert happens under
   `logical_links` too — holding it here makes the three-way outcome exact,
   not merely likely: a cancel that completed before this gate runs is
   guaranteed visible (case a), and a cancel still blocked on this held lock
   is guaranteed to complete strictly after this gate's WAITING send (the
   ordinary, by-design "cancelled while parked in WAITING" case: `GetStatus`
   already reports Cancelled immediately via `cancelled_cops` in that case;
   only the async event defers to the next dequeue, which is intentional,
   documented behavior).

   The `primitives` lock guard is still **held across the `send_cop_status`
   call** in both the CANCELLED and WAITING arms (post-commit fix, Codex P1
   finding, unchanged by round 6), keeping it atomic with respect to every
   other CANCELLED-emitting path (`cancel_link_cops`, `cancel_held_tx_items`,
   `should_skip_cancelled_item`) — each of those removes the `primitives`
   entry before emitting CANCELLED. Whichever side wins the `primitives`
   lock determines the order: if a remover wins, case (c) applies and WAITING
   never fires; if this code wins, the remover (and its CANCELLED emission)
   is forced to wait until this gate's send completes. So WAITING can never
   be observed after CANCELLED for the same COP — this closes both the
   disconnect residual originally accepted below and (as of round 6) the
   explicit-cancel residual (see Consequences).

   Lock order here is **`logical_links -> primitives -> subscriptions`**
   (round 6; the original fix was `primitives -> subscriptions` only). This
   is safe from a lock-ordering standpoint: consistent with both of this
   crate's documented hierarchies (ADR-080's `shared_channels ->
   {logical_links, api}` and ADR-115's `logical_links -> subscriptions ->
   queue`) — no other call site in this crate acquires `logical_links` while
   already holding `primitives`, and no site holds `subscriptions` while
   acquiring either. `primitives` itself still sits outside both documented
   hierarchies otherwise, so this is a new, acyclic three-lock chain with no
   reverse edge anywhere in the crate — no deadlock risk. `send_cop_status`
   itself does one uncontended lock acquisition plus a non-blocking
   unbounded-mpsc send, so the extra hold time is negligible.

No WAITING event is ever emitted before the final FINISHED transition, per
spec d) — the `remaining == 0` branch in `handle_send_recv` returns `None`
(no continuation), so insertion point 2 above never fires for the last
cycle. Event ordering between the two insertion points — WAITING always
strictly before the next cycle's EXECUTING — is structurally guaranteed, not
merely likely: `schedule_continuation` (which parks or re-enqueues the next
cycle) runs after `dispatch_tx_item` returns, in the same poll-task
iteration, so there is no cross-task race between the pair; the next cycle
cannot dispatch (and therefore cannot emit its own EXECUTING) until this
cycle's WAITING emission has already completed.

3. **S4 recheck now also drains `cancelled_cops` (pre-commit edge-case fix,
   same PR).** `handle_send_recv`'s existing S4 post-receive-phase recheck
   (ADR-086) re-validates `still_on_this_channel` after the receive phase
   returns, but originally did not consult `cancelled_cops` at all. This was
   a real gap, not merely a theoretical one: insertion point 2's own
   `primitives.contains_key` guard only yields to a *disconnect* racing this
   code (`cancel_link_cops` removes the `primitives` entry before emitting
   CANCELLED) — it does nothing for an explicit `CancelComPrimitive`, which
   deliberately leaves the `primitives` entry in place (`rpc_cancel_com_primitive`
   only ever inserts into `cancelled_cops`; only `should_skip_cancelled_item`
   and `cancel_link_cops` remove from `primitives`). Worse,
   `ReceivePhaseOutcome::CycleComplete`'s `Matched` arm (inside
   `wait_for_expected_response`'s inner loop) returns the instant a cycle's
   `num_receive_cycles` match quota is hit, structurally bypassing that same
   loop's own bottom `(was_cancelled, is_stale)` check for that exact pass —
   the one other place in this file that would otherwise have drained
   `cancelled_cops` and caught the cancellation. So a `CancelComPrimitive`
   landing anywhere before S4 runs (including concurrently with the
   cycle-completing response) was invisible to both checkpoints, and
   insertion point 2 would emit a spurious `PduCopstWaiting` for a
   continuation cycle that would never run.

   The fix mirrors the exact `(was_cancelled, is_stale)` idiom already used
   at `wait_for_expected_response`'s own loop bottom and at `handle_delay`'s
   per-tick check: S4 now drains `cancelled_cops` in the same
   `logical_links` lock acquisition it already used for the staleness check,
   checked *before* staleness (matching the loop-bottom idiom's ordering).
   A positive drain takes a **first-wins gate through `primitives`**:
   `ctx.primitives.lock().await.remove(&cop_handle).is_some()` guards the
   `send_cop_status(.., PduCopstCancelled)` call, mirroring the idiom already
   used by `cancel_link_cops`/`cancel_held_tx_items`/
   `should_skip_cancelled_item`. (An earlier version of this fix emitted
   unconditionally here, reasoning that `cancelled_cops` has exactly one
   reader so a positive drain is always this cycle's own first chance to
   observe it — true, but irrelevant: it does not stop a *different*
   emitter, `cancel_link_cops` running from a concurrent
   `DisconnectComLogicalLink`/`DestroyComLogicalLink`, from independently
   removing the same COP from `primitives` and emitting its own
   `PduCopstCancelled` for it first, which would have produced two terminal
   CANCELLED events for one COP — a Codex P2 finding on the review round
   that landed this ADR, fixed by adding the gate.) Either way this cycle
   returns `None` (no continuation), so insertion point 2 never fires for
   this cycle.

   This closes an invariant that now holds crate-wide: every
   `PduCopstCancelled` emission site in `events.rs` is either inside
   `cancel_link_cops` itself (the canonical teardown emitter, which scans
   and removes from `primitives` before emitting, so it needs no further
   gate) or is gated by a `primitives.remove(...).is_some()` first-wins
   check immediately guarding it — so two independent code paths can never
   both emit CANCELLED for the same COP. The same review pass found and
   closed the identical gap at three pre-existing sibling sites that shared
   S4's original (incorrect) "unconditional is fine" reasoning, plus two
   further sites the sweep turned up while confirming the invariant:
   - `handle_delay`'s `delay_cancelled` arm (the branch S4's original
     comment cited as its precedent) — same first-wins `primitives.remove`
     gate added.
   - The `TxFailure::Cancelled` (ISO-TP FC-wait) path in
     `handle_send_recv`'s transmit-retry match — same gate added.
   - `should_skip_cancelled_item`'s explicit-cancel branch — same gate
     added (its doc comment's stale "notify-first" description was also
     corrected to describe the leave-in-place-then-first-wins-remove
     behavior it actually has).
   - `wait_for_expected_response_inner`'s retransmit-on-`TxFailure::Event`
     arm's own nested `TxFailure::Cancelled` case — same gate added.
   - `wait_for_expected_response_inner`'s own per-pass loop-bottom
     `was_cancelled` check (the very idiom this S4 fix mirrors) — same gate
     added.

   **Amendment (guard held across the send, not just the remove — Codex P2
   finding, second review round).** The six gates above, as first landed,
   each evaluated `primitives.lock().await.remove(...).is_some()` as a bare
   expression: the lock guard dropped the instant that expression finished,
   *before* the gated `send_cop_status(.., PduCopstCancelled)` call ran. That
   reopened exactly the visibility gap `rpc_cancel_com_primitive` deliberately
   avoids by leaving the `primitives` entry in place until dispatch
   (`rpc_primitive.rs:1670-1673`): in the window between the lock-guarded
   remove and the (now-unguarded) send, a concurrent `GetStatus` — whose
   absent-entry-means-Finished logic lives at `rpc_primitive.rs:1770-1847` —
   could observe the entry already gone and report `PduCopstFinished` for a
   COP whose terminal `PduCopstCancelled` `SubscribeEvent` had not yet been
   dispatched, i.e. before the cancellation was "real" from the event
   stream's perspective. All seven explicit-cancel emission sites (the six
   above plus `rpc_cancel_com_primitive`'s own StopComm-held-item branch,
   `rpc_primitive.rs:~1720`, which had no first-wins gate at all and instead
   still used the old notify-then-remove ordering) now bind the lock guard to
   a variable and hold it across both the `remove` and the conditional
   `send_cop_status` `.await`, so the critical section covers the full
   remove-and-notify sequence, not just the remove. The crate-wide invariant
   this establishes: **a `primitives` entry for an explicitly-cancelled COP
   is never observed absent, under the lock, before its terminal
   `PduCopstCancelled` event has been sent.** Deadlock-safety is unchanged
   from the acyclic `primitives -> subscriptions` argument above — nothing
   else is held at any of these seven call sites, so widening the critical
   section to include the `.await` inside it introduces no new lock edge.

## Alternatives Considered

1. **Emit WAITING inside `handle_send_recv`'s continuation branch instead of
   `dispatch_tx_item`'s tail.** Rejected: `handle_send_recv`'s continuation
   branch runs before `*ctx.executing_cop.lock().await = None;` (which lives
   in the caller, `dispatch_tx_item`). Emitting WAITING there would report
   WAITING via `SubscribeEvent` while a concurrent `GetStatus` poll would
   still read `PduCopstExecuting` (via `executing_cop`) until `executing_cop`
   is actually cleared — inverting the two data sources' agreement that
   ADR-117 established. Emitting at `dispatch_tx_item`'s tail, after the
   `executing_cop` clear, keeps both sources consistent.
2. **Add a new `CopEntry` field to track the cycle boundary.** Rejected as
   redundant: `CycleContinuation`'s `Option`-ness (returned by
   `handle_send_recv`) already reifies exactly that boundary — "does another
   cycle follow this one" — with no need for a second, independently
   maintained piece of state that could drift out of sync with it.

## Consequences

- Closes conformance-audit finding A2-24
  (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`): a
  `SubscribeEvent` subscriber now sees the same EXECUTING↔WAITING pair
  around cyclic cycle boundaries that `GetStatus` polling has reported
  correctly since ADR-117.
- **Explicit-cancel race (found in pre-commit edge-case review, fixed by
  insertion point 3 above; residual window CLOSED by round 6, see below):**
  the originally-reported form of this bug — a `CancelComPrimitive` landing
  anywhere from cycle start up through the cycle-completing response's own
  match evaluation (including the `ReceivePhaseOutcome::CycleComplete`
  `Matched`-arm bypass) — is fully closed: S4 now drains `cancelled_cops`
  before insertion point 2 can ever be reached, for every case where the
  cancel's `cancelled_cops.insert` (in `rpc_cancel_com_primitive`) completes
  before S4's drain runs.
  **Round-6 closure (previously an accepted residual, now fixed — this is
  the "second `cancelled_cops` recheck at insertion point 2" the paragraph
  below originally dismissed as redundant):** S4's drain and insertion point
  2's `primitives.contains_key` check were not the same critical section —
  `*ctx.executing_cop.lock().await = None;` and a fresh
  `primitives.lock().await` acquisition sat between them, with no further
  `cancelled_cops` recheck. A `CancelComPrimitive` landing in that specific
  few-lock-acquisitions window (after S4's drain, before insertion point 2's
  check) produced one spurious `PduCopstWaiting`, with the correcting
  `PduCopstCancelled` not landing until the now-doomed continuation cycle was
  next dequeued and hit `should_skip_cancelled_item`'s own `cancelled_cops`
  check — up to `cycle_time_ms` later (a `u32` milliseconds field, so
  potentially minutes in the worst case, not a brief transient as originally
  assessed). A Codex review-round finding on this PR identified that this
  window was wider and more consequential than the "redundant... for every
  case that matters in practice" dismissal below originally judged, and
  design-advisor confirmed a deadlock-free fix: insertion point 2 (Decision
  §2 above) now drains `cancelled_cops` itself, atomically with the
  `primitives` check, under `logical_links` held across the send. **This
  specific finding proved the original "redundant" dismissal wrong** — the
  second recheck was not redundant with S4; it closes a gap S4 structurally
  cannot, because S4 runs strictly before this window opens. The paragraph
  immediately below is retained as a historical record of that (incorrect)
  original reasoning; it no longer describes current behavior.
  ~~This is an accepted transient-ordering window, narrow (a handful of
  uncontended lock acquisitions with no intervening `.await` on I/O or a
  timer) — not a design gap requiring a different fix, and not closable
  without either merging S4 and insertion point 2 into one critical section
  (a larger restructuring, out of scope for this fix) or adding a second
  `cancelled_cops` recheck at insertion point 2 (redundant with S4 for every
  case that matters in practice).~~ Unaffected by the disconnect-race fix
  below: its worst case is already WAITING-then-CANCELLED, the only allowed
  order — not the inverted, disallowed order that fix closes.
- **Round-6 testing infeasibility.** Forcing a `CancelComPrimitive` to land
  deterministically inside the specific S4-drain-to-insertion-point-2 window
  (a handful of uncontended lock acquisitions with no intervening `.await` on
  I/O or a timer) has the same infeasibility as the disconnect-race and
  explicit-cancel-vs-cycle-completion races documented elsewhere in this
  file: it would require a mid-dispatch test hook (e.g. a way to pause
  `dispatch_tx_item` between `*ctx.executing_cop.lock().await = None;` and
  the `logical_links` acquisition) that this harness does not have. No new
  test machinery was added for it. The existing
  `sendrecv_cyclic_cop_explicitly_cancelled_while_waiting_then_fresh_cop_reports_idle`
  test already covers the sibling, non-racing "cancelled while parked in
  WAITING" path (case where the cancel lands after insertion point 2's send
  has already completed) — the round-6 fix does not change that path's
  behavior, only the narrower window immediately preceding it.
- **Disconnect race closed, WAITING (post-commit fix, Codex P1 finding).**
  The `contains_key`-check-then-`send_cop_status` sequence at insertion
  point 2 is now one atomic critical section under the `primitives` lock
  (see Decision above), so a disconnect's `PduCopstCancelled` can no longer
  land between the check and the emission — the previously accepted
  disconnect residual (a spurious `PduCopstWaiting` delivered after the COP
  was already logically cancelled) is fully closed, not merely narrowed. The
  racing interleaving this fix closes is not deterministically reproducible
  on this crate's `current_thread` mock-harness test runtime — same class of
  narrow-window infeasibility as the one documented at
  `rpc_primitive.rs:1935-1939` (`reconcile_stale_cll_subscription`) — so
  correctness rests on the lock-ordering (happens-before) argument in the
  Decision section above, not on a new regression test. The existing
  non-racing disconnect case in `tests/grpc_mock/cop_ctrl_cycles.rs`
  (`sendrecv_cyclic_cop_cancelled_by_disconnect_then_reconnect_starts_fresh_cop_idle`)
  remains the coverage for the ordinary, non-racing disconnect path.
- **Disconnect race closed, EXECUTING (second post-commit fix, Codex P1
  finding).** The same class of race existed at insertion point 1: a
  concurrent `DisconnectComLogicalLink` (via `cancel_link_cops`) could
  remove the `primitives` entry and emit `PduCopstCancelled` after
  `should_skip_cancelled_item` passed at dequeue time but before
  `handle_send_recv`'s unconditional cycle-start EXECUTING emission ran (no
  lock was held at that point), producing a terminally-inverted
  `WAITING → CANCELLED → EXECUTING` sequence. The fix is the same pattern as
  the WAITING closure above: insertion point 1's `contains_key` check and
  its `send_cop_status` call are now one atomic critical section under the
  `primitives` lock (see Decision §1 above), gated to bail (return `None`,
  no continuation) if the COP is already gone. As with the WAITING fix, S1's
  existing staleness recheck (`still_on_this_channel`, just below insertion
  point 1) needs no change — its `primitives.remove(...).is_some()` gate
  continues to correctly suppress a duplicate CANCELLED for the ordinary,
  non-racing case where a disconnect lands after this gate's own
  check-and-emit has already completed; a post-disconnect parked
  continuation that would previously have reached S1 with the entry already
  gone now instead bails one step earlier, at insertion point 1, with the
  same zero-further-event outcome S1 previously produced — not a behavior
  change for any currently-tested path. The racing interleave itself remains
  non-deterministically reproducible on this crate's `current_thread`
  mock-harness test runtime, same infeasibility class as the WAITING closure
  above and as `rpc_primitive.rs:1935-1939`
  (`reconcile_stale_cll_subscription`) — so correctness again rests on the
  lock-ordering (happens-before) argument in Decision §1, not on a new
  regression test. Existing non-racing tests in
  `tests/grpc_mock/cop_ctrl_cycles.rs` are unaffected by this change.
- Time == 0 cyclic COPs (the spec note under Table 6, §9.2.6.2.3) may emit
  rapid EXECUTING/WAITING pairs on successive TX-queue passes when
  `cycle_time_ms == 0` re-enqueues each follow-up cycle immediately — this is
  spec-required behavior given that configuration, not a bug introduced by
  this ADR.
- `tests/grpc_mock/cop_ctrl_cycles.rs` pins the `SubscribeEvent`-visible
  sequence in five cases: `sendrecv_two_cycle_cop_walks_idle_executing_waiting_executing_finished`
  (extended) asserts a 2-cycle cyclic COP emits exactly
  `Executing, Waiting, Executing, Finished`;
  `sendrecv_three_cycle_cop_pins_full_waiting_executing_sequence` (new)
  asserts a 3-cycle cyclic COP emits exactly
  `Executing, Waiting, Executing, Waiting, Executing, Finished`, covering the
  second WAITING→EXECUTING pair the 2-cycle test cannot reach;
  `sendrecv_single_cycle_cop_emits_executing_then_finished_no_waiting` (new)
  asserts a single-cycle (non-repeating) COP emits exactly
  `Executing, Finished`, with no WAITING;
  `sendrecv_cyclic_cop_cancelled_by_disconnect_then_reconnect_starts_fresh_cop_idle`
  (extended) asserts a cyclic COP parked in WAITING and then disconnected
  emits `Executing, Waiting, Cancelled` — the non-racing disconnect case (see
  the disconnect-race bullet above for the racing case, which remains
  untestable on this runtime); and
  `sendrecv_cancel_racing_the_cycle_completing_response_never_emits_waiting`
  (new) asserts a `CancelComPrimitive` that lands before the cycle-completing
  response is even injected (landing before S4's drain — see insertion point
  3) produces exactly `Executing, Cancelled`, with no WAITING — the
  explicit-cancel race this edge-case-review fix closes; the test orders
  `CancelComPrimitive` (awaited to completion) before `inject_rx` so this
  ordering is guaranteed by construction rather than by winning a timing
  race against the poll task's RX-tick cadence.
- ADR-117's own Consequences section marks its A2-24 bullet as closed by this
  ADR.
- **Surviving residual (accepted, same class as ADR-021's original
  acceptance): `should_skip_cancelled_item`'s drain-to-lock window.**
  `should_skip_cancelled_item`'s explicit-cancel branch reads
  `cancelled_cops` (draining it) in one `logical_links` lock acquisition,
  then separately acquires the `primitives` lock to remove the entry and
  emit CANCELLED. Between those two acquisitions — after the drain has
  already decided this call is the one that owns the cancellation, before
  `primitives` is locked — a concurrent `GetStatus` could still observe a
  transient Executing/Waiting-ish read instead of Cancelled, since neither
  `cancelled_cops` membership (already drained) nor the `primitives` removal
  (not yet performed) reflects the pending cancellation in that instant. This
  is the guard-held-across-notify fix's own scope boundary, not a gap it
  introduces: the fix's crate-wide invariant is stated in terms of what a
  reader can observe once the `primitives` lock is held for a given COP, and
  this window sits strictly before that lock is ever acquired for this call.
  It is the same class of transient-visibility window ADR-021 originally
  accepted for a different reason (a concurrent `GetStatus` briefly seeing a
  stale value, bounded there by the CLL reporting Offline anyway through a
  separate mechanism) — narrow, not widened by this fix, and not treated as
  a defect requiring further code change here.
- **Broader implicit-teardown-path class — closed by ADR-128.** The
  guard-held-across-notify treatment above was applied only to the seven
  *explicit*-cancel `PduCopstCancelled` emission sites (six in `events.rs`
  plus `rpc_cancel_com_primitive`'s StopComm-held-item branch in
  `rpc_primitive.rs`); a design-advisor sweep found roughly a dozen further
  sites with the same lock-released-before-send shape on *implicit*
  teardown paths, left unfixed by this PR (A2-24). ADR-128 (2026-07-24, the
  very next day) closed this class crate-wide via its shared
  `emit_terminal_if_live` helper (`events_event_senders.rs`), which holds
  one `primitives` guard continuously across the removal and the
  notification `.await` the same way this ADR's fix does — every site named
  above now routes through it or an equivalent hand-rolled single-critical-
  section form: `cancel_held_tx_items`, the `handle_send_recv`/
  `wait_for_expected_response_inner` staleness-bail sites, and
  `handle_delay`'s own staleness recheck all call `emit_terminal_if_live`
  directly; `cancel_link_cops`'s batch remove-then-emit loop instead holds
  its own continuous `primitives` critical section across the equivalent
  batch remove-and-notify sequence (it operates on every COP for a CLL in
  one pass, not a single `cop_handle`, so it cannot call the single-cop
  helper as-is).

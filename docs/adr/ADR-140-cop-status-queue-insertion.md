# ADR-140: `send_cop_status` Queue Insertion — Resolve-Before-`primitives` Pattern

**Date:** 2026-07-28
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`J2534Service::logical_links`
             doc comment, `StatusEvent::Cop`, `CllEventQueue`, `CllQueueTarget`,
             `resolve_queue_target`, `LogicalLinkState`),
             `j2534-0404-service/src/service/events.rs` (`send_cop_status`,
             `emit_terminal_if_live`, `cancel_link_cops`,
             `should_skip_cancelled_item`, `dispatch_tx_item` and its other
             direct `send_cop_status` callers, `deliver_or_enqueue`,
             `send_cll_status`, `send_error_event`, `poll_rx_inner`/
             `CllRxEntry`, `handle_start_comm`),
             `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_cancel_com_primitive`, `rpc_get_event_item`),
             `j2534-0404-service/src/service/rpc_misc.rs`
             (`ioctl_set_buffer_size`, `ioctl_set_event_queue_properties`),
             `j2534-0404-service/src/service/rpc_link.rs` (`CllEventQueue`/
             `LogicalLinkState` construction sites)

## Context

`send_cop_status` previously notified only a live `SubscribeEvent`
subscriber directly — unlike its sibling `send_cll_status`, it was never
enqueued into the per-CLL `rx_buf` queue that `GetEventItem` pollers read.
This was a known gap: a P2 follow-up left open when `send_cll_status` itself
was converted to the queue (ADR-105), and closed halfway — `StatusEvent::Cop`
existed in the `CllQueueItem` enum and the `GetEventItem` poll-side
conversion already handled it, both marked/left unused pending this design.

The blocker was concurrency, not missing plumbing. This crate's documented
lock hierarchy is `logical_links -> primitives`, and `send_cop_status`'s
~52 call sites (45 funneled through `emit_terminal_if_live`, plus roughly a
dozen direct callers) hold `primitives` locked across the call by design —
ADR-128 requires the `primitives` removal and `terminal_cops.record` to
share one continuous critical section (the A2-23 fix). Mirroring
`send_cll_status`'s own pattern — look up `logical_links` inside the
function to fetch the CLL's `rx_buf` — would have `send_cop_status` acquire
`logical_links` while a caller already holds `primitives`, inverting the
documented order at every `emit_terminal_if_live`-routed site. At one
site specifically, `dispatch_tx_item`'s WAITING/CANCELLED tail, the caller
already holds *both* `logical_links` and `primitives` before calling
`send_cop_status` — an internal `logical_links.lock()` there would
self-deadlock outright (`tokio::sync::Mutex` is not reentrant).

## Decision

Resolve the queue target from `logical_links` *before* any caller acquires
`primitives`, and pass it in as a parameter — never let `send_cop_status`
acquire `logical_links` itself.

- `CllQueueTarget` (`service.rs`) bundles the per-CLL `rx_buf`
  (`Arc<Mutex<CllEventQueue>>`) only. Queue-policy fields
  (`event_queue_cap`, `event_queue_mode`, `result_buffer_limit`) do **not**
  travel in this snapshot — they live on `CllEventQueue` itself and are read
  fresh under the queue's own lock at push time, the same pattern ADR-115
  round 6 established for `live_sender` (see Consequences below for why an
  earlier draft of this ADR carried them in the snapshot, and why that was
  wrong).
- `resolve_queue_target(logical_links, cll_handle) -> Option<CllQueueTarget>`
  performs a standalone `logical_links` lookup — a lock acquired and
  released, never held across the call — and must run before the caller
  touches `primitives`.
- `send_cop_status` gains a `queue_target: Option<&CllQueueTarget>`
  parameter. The existing `terminal_cops.record(...)` phase for terminal
  statuses is unchanged — still unconditional, still the first thing the
  function does, still covered by whatever `primitives` guard the caller
  holds across the whole call (ADR-128's atomicity boundary is untouched).
  Given `Some(target)`, the function then enqueues via `deliver_or_enqueue`
  using the resolved `rx_buf`/params, *without* touching `subscriptions` at
  all (`CllEventQueue::live_sender`, read fresh under the queue's own lock
  inside `deliver_or_enqueue`, is the single source of truth for live
  delivery — the same argument `send_cll_status`'s own doc comment already
  makes). Given `None` (no `LogicalLinkState` for this CLL — a destroy-path
  race), it falls back to the previous direct-only `subscriptions` send,
  mirroring `send_cll_status`'s identical no-`LogicalLinkState` fallback.
- `emit_terminal_if_live` gains a `logical_links` parameter and resolves the
  queue target first, then acquires `primitives` and — only on a winning
  removal — calls `send_cop_status` with the already-resolved target. This
  covers all 45 sites that funnel through it with one change each (an added
  argument at the call site, not a lock-order change, since none of those 45
  sites held `logical_links` beforehand — verified by review).
- The dozen-plus direct callers fall into two shapes: sites that never
  touched `logical_links` before (`cancel_link_cops`, `dispatch_tx_item`'s
  EXECUTING gate and write-error-cancel path) now call
  `resolve_queue_target` immediately before their `primitives` acquisition;
  sites that already acquire-then-release `logical_links` earlier in the
  function for an unrelated reason (`should_skip_cancelled_item`,
  `rpc_cancel_com_primitive`) capture the `CllQueueTarget` from that same
  already-open borrow instead of a second lock round-trip.
  `dispatch_tx_item`'s WAITING/CANCELLED tail — the one site that holds
  `logical_links` and `primitives` together across the call — captures the
  target from the same `logical_links.get_mut` borrow it already uses for
  its `cancelled_cops` check, and reuses it for both the CANCELLED and
  WAITING branches sharing that block.

**Rejected: deferring the notify/enqueue until after `primitives` is
released** (splitting `send_cop_status` into a must-run-under-`primitives`
record phase and a can-run-after-release notify phase). The `primitives`
guard is not just the A2-23 atomicity boundary — it is the sole serializer
of terminal-vs-non-terminal emission order (ADR-118's invariant that
EXECUTING/WAITING can never be observed after CANCELLED for the same COP
depends on every emitter sending while still holding `primitives`).
Deferring emission past the guard's release reopens exactly the
stale-status-ordering bug ADR-118 round 6 fixed: a WAITING emission could be
queued for later delivery, a concurrent CANCELLED emission (holding its own,
later, `primitives` critical section) could send first, and the deferred
WAITING would then land after CANCELLED.

**New lock-hierarchy edge:** `primitives -> queue` (a per-CLL
`CllEventQueue`'s own lock), and — at `dispatch_tx_item`'s WAITING/CANCELLED
tail specifically — `logical_links -> primitives -> queue`. The queue lock
remains a leaf: `deliver_or_enqueue` never acquires any other lock while
holding it, so this is a pure extension of the existing acyclic lock graph,
not a new cycle. See `J2534Service::logical_links`'s doc comment
(`service.rs`) for the full, call-site-level statement of this edge.

## Consequences

- `GetEventItem` pollers now see COP status transitions
  (Executing/Waiting/Cancelled/Finished) that were previously visible only
  to a live `SubscribeEvent` subscriber, closing the second half of the
  ADR-105 P2 backlog item and matching `send_cll_status`'s existing
  behavior. `StatusEvent::Cop` (`service.rs`) is no longer dead code.
- **Queue-policy staleness window — found by Codex review, closed
  structurally, not accepted as residual.** An earlier draft of this design
  had `CllQueueTarget` carry `event_queue_cap`/`event_queue_mode`/
  `result_buffer_limit` alongside `rx_buf`, snapshotted from
  `LogicalLinkState` before `primitives` (and, for terminal statuses,
  `terminal_cops`) was acquired, and claimed the window was contained
  because `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES` "cannot race while
  connected." That claim was incomplete: it missed the disconnect
  transition. `rpc_disconnect_com_logical_link` sets `link.connected =
  false` (`rpc_link.rs`) well before its own `cancel_link_cops` call
  several `.await`s later; `rpc_module_disconnect` (`rpc_module.rs`) and
  `handle_channel_hard_error` (`events.rs`) have the same shape — each
  flips the CLL(s) to disconnected, then only later calls `cancel_link_cops`
  to actually cancel their still-live COPs. `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s
  guard (`pdu_connect_begun()`) reopens the instant `connected` flips, so a
  client could change queue policy while a COP was still live in
  `primitives` and some other producer's already-resolved (now stale)
  `CllQueueTarget` was still in flight toward `deliver_or_enqueue` — and,
  because `push_cll_event`'s `OverwriteOldest` mode only ever evicts one
  item per push, an over-cap queue produced this way never converges back
  to the new cap on its own; this was a persistent violation, not a
  self-healing one, so "accept as residual" did not apply.
  (`rpc_destroy_com_logical_link` does not share this gap: it removes the
  `LogicalLinkState` entry outright before calling `cancel_link_cops`, so
  `resolve_queue_target` always returns `None` there, hitting the existing
  no-`LogicalLinkState` fallback instead.) Closed by moving
  `event_queue_cap`/`event_queue_mode`/`result_buffer_limit` onto
  `CllEventQueue` itself (mirroring `live_sender`'s ADR-115 round-6
  placement) so every producer — `send_cll_status`, `send_error_event`,
  `send_cop_status`, `poll_rx_inner`, `handle_start_comm` alike — reads
  current policy under the queue's own lock at the point of use, and
  `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s policy write and its cap-trim
  loop now execute in that same single critical section. No snapshot of
  these fields exists anywhere in the system anymore, so there is nothing
  left to go stale.
- A COP that finished before any `SubscribeEvent` subscriber ever attached
  now has its terminal status delivered live, FIFO, the moment a subscriber
  does attach — previously it was silently dropped. This surfaced 32
  pre-existing `grpc_mock` tests whose event-wait predicates assumed "next
  terminal event on this CLL" meant "the COP this test started," which is no
  longer a safe assumption once backlogged status is delivered. Fixed by
  qualifying each affected predicate with the specific `cop_handle` under
  test, following the pattern `cop_ctrl_cycles.rs`'s pre-existing
  `sendrecv_error_event_carries_the_failing_cops_own_handle_not_a_concurrent_sibling`
  already established for the analogous `send_error_event` case. No
  production behavior was changed by these test fixes.
- `ioctl_set_event_queue_properties_atomic_trim_tests::policy_change_and_trim_are_atomic_so_a_push_right_after_never_exceeds_the_new_cap`
  (`rpc_misc.rs`) seeds a queue over a newly-lowered cap, runs the real
  `SET_EVENT_QUEUE_PROPERTIES` handler, and asserts a subsequent push still
  conforms to the new cap. **Caveat (`edge-case-hunter` review), resolved:**
  this test is purely sequential and only proves the end state after the
  IOCTL call has fully returned — it does not, on its own, prove the write
  and the trim loop execute as one atomic critical section with respect to a
  concurrent producer (a locally reverted split-lock version of the handler
  still passed it). The genuine concurrency proof is the sibling test
  `ioctl_set_event_queue_properties_atomic_trim_tests::concurrent_push_racing_the_gap_between_write_and_trim_never_leaks_pre_trim_backlog_live`
  (`rpc_misc.rs`), added in response to this finding: it forces a real race
  via `tokio::sync::Mutex`'s FIFO waiter ordering and asserts on what a live
  `SubscribeEvent` subscriber observes (the final queue length/content
  alone cannot distinguish the split and atomic cases even under a forced
  race — see that test's own doc comment for why). Confirmed to fail
  against a manually re-split handler and pass against the real one; see
  that test's doc comment for the empirical transcript.
- **`pdu_connect_begun()` gate reopened by the field-ownership move above,
  found by a second Codex review round on PR #3, closed structurally.**
  Moving `event_queue_cap`/`event_queue_mode` onto `CllEventQueue` had
  `ioctl_set_event_queue_properties` check the `pdu_connect_begun()` gate
  under `logical_links`, then drop that guard before separately re-locking
  the queue to write the new policy, reopening the same class of window
  ADR-126 round 2 had already closed with a single-lock check-and-write: a
  concurrent `ConnectComLogicalLink` could claim `connect_in_flight` in the
  gap between the two acquisitions, letting this IOCTL's write land after
  PDUConnect had effectively begun. Fixed by nesting the queue mutation
  inside the `logical_links` critical section instead of acquiring it
  separately, adding the `logical_links -> queue` skip-level edge (bypassing
  `primitives`) documented in `J2534Service::logical_links`'s doc comment
  (`service.rs`). ADR-126's Decision and invariant are unchanged, only the
  enforcement's lock geometry moved (to the queue-owned fields) and then
  moved back (to a single nested critical section); see ADR-126 for the
  original invariant this restores. Like ADR-126's own closure of this same
  class of window, this fix has no forced-interleaving regression test
  proving the `connect_in_flight`-vs-IOCTL race is actually closed — only a
  structural-safety argument (verified by direct code reading: `logical_links`
  held with no intervening `.await` from the gate check through the queue
  mutation). The mock harness's `PassThruConnect` has no hold-point to force
  this interleaving deterministically (see ADR-126's own Consequences for the
  identical disclaimer); the `concurrent_push_racing_the_gap_between_write_and_trim_never_leaks_pre_trim_backlog_live`
  test above proves round-1's write/trim atomicity against a live subscriber
  only, not this round's connect-race closure specifically.

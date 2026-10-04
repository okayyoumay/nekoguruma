# ADR-094: Poll-Loop Tick Duties Run on a Persistent Deadline, Not a Per-Iteration Sleep Arm

**Date:** 2026-07-16
**Status:** Accepted (extended by ADR-095 — a follow-up audit on this same PR found this fix's
tick-deadline mechanism didn't yet reach the parked-cyclic-drain and `tx_held`-drain loops, plus
several other poll-task holds; ADR-095 generalizes this ADR's block into a shared helper reused from
those sites)
**Affects:** `j2534-0404-service` events (`poll_channel_events`) — extends ADR-003's RX/tick
interleaving mechanism and ADR-083's tester-present dispatch integration

## Context

`poll_channel_events`'s per-physical-channel poll loop (`j2534-0404-service/src/service/events.rs`)
runs a `tokio::select! { biased; ... }` with, among other arms, a per-iteration
`_ = tokio::time::sleep(interval) => { poll_rx(&ctx).await; dispatch_due_tester_present(false,
&ctx).await; }` arm — `poll_rx` (the sole `read_messages` caller) and `dispatch_due_tester_present`
(both `CP_TesterPresentSendType` modes as of ADR-093) both previously ran *only* inside that one
arm at the outer-loop level, plus two narrower call sites inside `handle_delay`'s and
`wait_for_expected_response`'s own internal per-tick loops (ADR-083).

A Codex review finding on PR #97 (the ADR-093 PR) identified a real starvation bug: `sleep(interval)`
is constructed fresh on every loop iteration, so it is never yet elapsed at the moment `select!` is
polled if `tx_rx.recv()` also happens to be ready. A client that keeps the TX queue continuously
non-empty with operations that don't enter either of the two narrower sub-loops — a sustained stream
of zero-response (`NumReceiveCycles == 0`) `CoptSendrecv`s, or `CoptUpdateparam`s — causes
`tx_rx.recv()` to resolve immediately, essentially every time the select is polled, for as long as
the queue stays non-empty. Because the select is `biased` and `tx_rx.recv()` is listed first, it
wins every time it's ready — but **`biased` is not the cause**: an *unbiased* select between one
always-ready arm and one arm that has never yet elapsed still picks the ready one every time,
regardless of bias. The actual defect is the restarting timer never getting a chance to elapse, not
arm ordering.

This is not a new exposure for `poll_rx`/mode-1 tester-present — both have depended on this same
mechanism since ADR-003/ADR-083 respectively. It is materially worse for `CP_TesterPresentSendType=0`
specifically: mode 0 used to be hardware-autonomous via `PassThruStartPeriodicMsg`, immune to this
service's own queue health entirely, until ADR-093 moved it onto this same software path. A tester-
present keep-alive silently starving under sustained client traffic risks real ECU session/S3-timer
expiry for both modes now, not just mode 1's pre-existing, narrower exposure.

## Decision

Replace the restarting per-iteration sleep with a persistent deadline, checked unconditionally after
every `select!` resolution, regardless of which arm actually fired:

```rust
let mut next_tick = tokio::time::Instant::now() + interval;
loop {
    // ... parked-cyclic-follow-up draining (unchanged) ...
    tokio::select! {
        biased;
        _ = &mut cancel => break,
        _ = shutdown.changed() => break,
        item = tx_rx.recv() => { /* unchanged */ }
        _ = tokio::time::sleep_until(next_tick) => {}
    }
    if tokio::time::Instant::now() >= next_tick {
        if !poll_rx(&ctx).await { break; }
        dispatch_due_tester_present(false, &ctx).await;
        next_tick = tokio::time::Instant::now() + interval;
    }
}
```

`biased` and the arm order are unchanged — bias still gives deterministic cancel/shutdown-first
priority, which is worth keeping and is orthogonal to this fix. The sleep arm becomes a pure wakeup
(empty body); the actual tick duties (`poll_rx`, `dispatch_due_tester_present`) move to an
unconditional deadline check that runs after the `select!` closes on every loop iteration — whether
`tx_rx.recv()` fired, the parked-cyclic-follow-up drain ran, or the `sleep_until` wakeup itself fired.
This guarantees the tick duties run at least once per `interval`, regardless of how busy the TX queue
is, closing the starvation for both `poll_rx` and `dispatch_due_tester_present` in one mechanism.

`next_tick = now + interval` (re-arm from completion, not `next_tick += interval`) preserves the
loop's existing "restart pacing from actual completion, no burst catch-up" semantics — an iteration
that takes unusually long does not cause a burst of queued-up ticks immediately after.

### Alternatives considered and rejected

- **Drop `biased`.** Does not fix the bug: a pending/not-yet-elapsed sleep future loses to a ready
  `tx_rx.recv()` in an unbiased select exactly as often as in a biased one — readiness, not
  declaration order, decides an unbiased select's outcome when only one arm is ready. Also forfeits
  the deterministic cancel/shutdown-first priority for no benefit.
- **A persistent `tokio::time::interval` ticker as its own `select!` arm.** Under `biased` with the TX
  arm listed first, a merely-elapsed (but not yet polled) tick still loses to a ready `tx_rx.recv()`
  every iteration; without `biased`, fairness becomes probabilistic rather than guaranteed. The
  post-select unconditional check is the only construction that guarantees the tick duties actually
  run every iteration once the deadline has passed, independent of arm readiness/ordering.
- **Call `dispatch_due_tester_present` from inside the `tx_rx.recv()` arm too.** Would miss `poll_rx`'s
  own starvation (a pre-existing, independent exposure) and the parked-cyclic-follow-up drain path;
  the post-select check covers every path with one site instead of duplicating the call.

## Consequences

- Tick cadence (`poll_rx` + `dispatch_due_tester_present`) is now bounded to `interval` regardless of
  TX queue pressure — the residual slip is at most one in-flight `dispatch_tx_item`'s own duration
  (the same order-of-magnitude bound ADR-083 already accepts for `wait_for_p3_gap`'s own wait), not
  unbounded.
- `CP_TesterPresentSendType=0` regains the starvation-immunity it had before ADR-093 (when it was
  hardware-autonomous), closing the regression that motivated this fix; mode 1 and RX polling get the
  same guarantee they previously lacked, as a side benefit of one shared mechanism rather than a
  tester-present-specific patch.
- **A pre-existing test's timing margin, previously padded by accident by the old jittery cadence,
  needed widening after this fix**: `tester_present_send_type::kline_five_baud_init_stamps_last_bus_activity_deferring_mode_1_sibling`
  assumed a sibling CLL's real bus activity would land comfortably before a mode-1 CLL's original
  due-deadline, with enough slack to absorb RPC/setup round-trip overhead. The old restarting-sleep
  cadence incidentally delayed due-checks under any concurrent traffic, which happened to preserve
  that slack; the new deterministic cadence fires close to precisely on schedule, exposing that the
  test's own margin was too tight independent of this fix's correctness. Widened rather than weakened
  — see the test's own updated timing constants/comments for the corrected arithmetic.
- New regression test (`tester_present_send_type.rs`): a mode-0 CLL under sustained TX-queue pressure
  from a sibling CLL's zero-response `CoptSendrecv` stream (each individually parking the poll task
  inside `wait_for_p3_gap`, which never itself calls `dispatch_due_tester_present`, per ADR-083's
  recursion-avoidance) continues to fire its periodic keep-alive on schedule instead of starving for
  the pressure window's duration.
- Extends, does not replace, ADR-003's RX/tick interleaving mechanism and ADR-083's "driven from every
  long-running per-tick poll loop on the channel" principle — the outer loop itself was the one path
  that principle didn't yet cover under sustained queue pressure; `handle_delay`'s and
  `wait_for_expected_response`'s own internal per-tick loops are unaffected by this change.

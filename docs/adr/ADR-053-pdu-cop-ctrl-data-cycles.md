# ADR-053: PDU_COP_CTRL_DATA Cycle Semantics for CoptSendrecv (Time / NumSendCycles / NumReceiveCycles)

**Date:** 2026-07-04
**Status:** Accepted (NumReceiveCycles = -1 receive-phase decision superseded by ADR-100)
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`StartComPrimitive` validation), `j2534-0404-service/src/service/events.rs` (`handle_send_recv`, `wait_for_expected_response`, `MatchProbe`, `dispatch_tx_item`, `schedule_continuation`, `poll_channel_events`), `j2534-0404-service/src/service.rs` (`TxItem::SendRecv`, `ComParamSet::p2_max_timeout_ms`)

## Context

ISO 22900-2 defines `PDU_COP_CTRL_DATA` for `PDU_COPT_SENDRECV` as:

- **Time** — the cycle period in ms for a cyclic send (or the delay length
  for `PDU_COPT_DELAY`).  With a cyclic time of `0`, the ComPrimitive is
  re-queued on the transmit queue after every completed cycle, ranked behind
  other ComPrimitives and tester-present messages.
- **NumSendCycles** — number of send cycles; `-1` = infinite cyclic send.
- **NumReceiveCycles** — receive cycles per send; `-1` (IS-CYCLIC) =
  infinite receive, `-2` (IS-MULTIPLE) = multiple expected responses from
  one or more ECUs.

The service ignored `NumSendCycles`/`NumReceiveCycles` entirely (every
`CoptSendrecv` was one send + at most one matching response) and misused
`Time` as the expected-response timeout, with a 50 ms fallback.  Clients
written against the standard (cyclic requests, functional requests answered
by several ECUs) could not express these operations, and conformant clients
setting `Time` as a cycle time silently got it applied as a response
timeout.

## Decision

### Field semantics

`StartComPrimitive(COPT_SENDRECV)` validates `NumSendCycles >= -1` and
`NumReceiveCycles >= -2` (`InvalidArgument` otherwise).

> **Amended by ADR-059:** `NumSendCycles == 0` is **not** normalised to a
> single send. It means no transmission happens at all — a receive-only
> capture — and a caller that wants the previous default single-send
> behaviour must now pass `1` explicitly. See ADR-059 for the corrected
> semantics, why the original normalisation conflicted with the caller's own
> receive-only use case, and how it composes with ADR-058's
> `NumReceiveCycles == 0`.

Each **send cycle** is one transmit (skipped entirely when `NumSendCycles ==
0`, ADR-059) followed by one receive phase; `PduCopstExecuting` is emitted
once before the first cycle and `PduCopstFinished` exactly once, after the
last cycle's receive phase.

### Response window: `CP_P2Max`, not `Time`

The receive phase's response window comes from the Active `CP_P2Max`
ComParam (stored in µs, D-PDU API convention; `ComParamSet::p2_max_timeout_ms`,
default 50 ms — the same value as the previous fallback), snapshotted per
cycle so `SetComParam` between cycles takes effect.  The window restarts
after each accepted response.

### Receive phase (`NumReceiveCycles`)

> **Amended by ADR-058:** `0` does **not** mean "one matching response." It
> means the send requires no response at all, and the phase completes
> immediately with no receive window. A caller that wants exactly one match
> must pass `1`, same as any other exact count below. See ADR-058 for the
> corrected semantics and why the original "legacy single-shot" reading
> conflicted with `CP_P3Func`/`CP_P3Phys`'s own "receiveCycle=0" condition.

- `0` — no response required; the send is fire-and-forget and the cycle
  completes right after the write (ADR-058).
- `n > 0` — exactly `n` matching responses; `MatchProbe` counts matches per
  poll pass (bounded by the remaining requirement) instead of a boolean. An
  empty `expected_response_array` with `n > 0` is not a special case: nothing
  can ever match, so the phase naturally times out (ADR-058).
- `-1` (IS-CYCLIC) — no window at all; the phase accepts matches
  indefinitely and ends only via `CancelComPrimitive` or a hard channel
  error.
- `-2` (IS-MULTIPLE) — unlimited matches until the window closes; the
  window closing is the *normal* end of the phase, so `PduErrEvtRxTimeout`
  is emitted only when no response at all arrived.  For finite counts, a
  window close before the required matches also emits `PduErrEvtRxTimeout`;
  in both cases the cycle still completes (matching the previous
  timeout-then-finish behaviour).

### Cyclic send scheduling (`Time`)

`handle_send_recv` executes **one** cycle per dequeue and hands a
`CycleContinuation` back to the poll loop when cycles remain, so a cyclic
COP never monopolises the channel between cycles:

- `Time > 0` — the follow-up cycle is *parked* in the poll task and
  dispatched when `cycle start + Time` elapses (checked on every loop pass;
  up to one `POLL_INTERVAL_MS` = 10 ms of jitter via the RX-poll tick).
- `Time == 0` — the follow-up cycle is re-enqueued at the **back of the TX
  queue** through a sender handle the poll task holds on its own queue,
  which is exactly the standard's rule that such a COP ranks behind
  the other ComPrimitives: everything already queued runs first.  (Tester-present
  messages are periodic messages on the adapter itself and are never
  blocked by the queue.)

Between cycles the COP stays in `primitives` with `executing_cop` cleared,
so `GetStatus` reports `PDU_COPST_WAITING` — the ADR-021 states carry over
naturally.  Cancellation is honoured at every cycle boundary
(`should_skip_cancelled_item` on dispatch) and inside the receive phase on
every poll pass; parked cycles that never come due (channel teardown) are
drained as `PduCopstCancelled` exactly like queued items.

## Alternatives Considered

1. **Keep `Time` as the response timeout** — non-conformant; leaves cyclic
   send and multi-response receive unimplementable and double-books one
   field.  Rejected.
2. **Sleep inside `handle_send_recv` between cycles** — trivially simple,
   but an infinite (`-1`) or long cyclic COP would block every other
   ComPrimitive on the shared physical channel for its entire lifetime.
   Rejected.
3. **A separate tokio task per cyclic COP** — breaks the invariant that all
   J2534 API calls on a channel are serialised through the single poll task
   in FIFO order (see `TxItem`), reopening the concurrency problems that
   design exists to prevent.  Rejected.

## Consequences

- Clients that previously passed a response timeout in `Time` must set
  `CP_P2Max` (µs) via `SetComParam` instead; `Time` on a single-shot COP is
  now ignored.  `tests/live_grpc_flow.rs` and the mock-backed tests were
  updated accordingly.
- Cyclic cycle timing has up to ~10 ms jitter (poll-tick scheduling), and a
  cycle whose receive phase runs longer than `Time` starts its next cycle
  late (cycle time is measured from cycle start, not enforced as a hard
  period).
- `CancelComPrimitive` on a parked cycle takes effect when the cycle next
  comes due, not instantly; the `cancelled_cops`/`primitives` bookkeeping
  guarantees it is honoured before any further transmit.
- `tests/grpc_mock/cop_ctrl_cycles.rs` pins the cycle-count, cycle-time,
  infinite-send, `n`-match, IS-MULTIPLE, and IS-CYCLIC behaviours against
  the mock.

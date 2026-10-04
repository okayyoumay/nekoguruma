# ADR-081: `tx_held` Drains at the Next Same-CLL Dequeue, Not by Re-Injection

**Date:** 2026-07-12
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`TxItem`), `j2534-0404-service/src/service/events.rs` (`spawn_channel_poll_task`, `dispatch_tx_item`, `drain_tx_held_backlog`), `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_resume_tx_queue`)

## Context

`PDU_IOCTL_SUSPEND_TX_QUEUE`/`PDU_IOCTL_RESUME_TX_QUEUE` (ADR-079) promise that a ComLogicalLink's (CLL's) own submission order survives a suspend/resume cycle. `SharedChannel::tx_queue` is a single unbounded mpsc channel per physical channel, shared by every CLL connected to it (dual-CAN-mode primaries and UUDT companions included) — `dispatch_tx_item` diverts an item into its owning CLL's `LogicalLinkState::tx_held: VecDeque<TxItem>` at the moment it is dequeued from that shared channel, if the CLL is currently suspended.

The original `PDU_IOCTL_RESUME_TX_QUEUE` implementation cleared `tx_suspended` and then drained `tx_held`, re-sending each item to the *tail* of the same shared `tx_queue`. A Codex automated review (PR #80) found this could invert order: if any of the same CLL's own items were still sitting un-dequeued in `tx_queue` at the moment of resume (e.g. because another CLL's items were ahead of them in the interleaved FIFO stream, or the poll task simply hadn't reached them yet), appending the held items to the tail placed them *after* those not-yet-dequeued items — even though the held items were submitted earlier and should execute first.

## Decision

**`tx_held` is never re-injected into the shared queue.** Instead:

- `ioctl_resume_tx_queue` clears `tx_suspended` and sends a single content-free `TxItem::ResumeWake { cll_handle }` marker onto the CLL's `SharedChannel::tx_queue`. This guarantees the poll loop dequeues *something* for this CLL soon, so a backlog isn't stranded forever when the client sends nothing further — but the marker carries no payload and is never dispatched as a real operation.
- The poll loop's `tx_rx.recv()` branch, upon dequeuing *any* item (real or a `ResumeWake`), first calls `drain_tx_held_backlog` for that item's owning CLL: it pops and fully dispatches (via the normal `dispatch_tx_item`/`schedule_continuation` pipeline) every entry currently in that CLL's `tx_held`, stopping early only if the CLL is re-suspended mid-drain or no longer exists. Only after this drain does the loop dispatch the just-dequeued item itself — unless that item *is* the `ResumeWake`, which is discarded without ever reaching `dispatch_tx_item`.
- The `parked` due-item loop (time-scheduled cyclic follow-up cycles, ADR-053) makes the identical call before dispatching a due item (Codex-review fix): a due parked continuation is dispatched directly from this loop, bypassing `tx_rx.recv()` entirely, so without this a CLL resumed (flag cleared, `ResumeWake` sent) but whose wake hasn't been dequeued yet could have an older backlog item sit in `tx_held` while a later cyclic follow-up runs first from `parked`.

This is correct without any epoch/generation counter because of an invariant that holds by construction: **everything in a CLL's `tx_held` is strictly older than anything still in the shared queue for that same CLL**, since items are only ever diverted into `tx_held` at dequeue time, in the same FIFO order the queue delivers them. Flushing the backlog at the next same-CLL dequeue point — whether that's a fresh client item or the resume's own wake — therefore always restores full per-CLL submission order, regardless of how the suspend/resume calls interleave with other CLLs' traffic on the shared channel.

## Consequences

- `PDU_IOCTL_RESUME_TX_QUEUE`'s external contract is unchanged (a CLL's own FIFO order survives suspend/resume); this ADR only changes the internal mechanism, not the observable guarantee.
- Cross-CLL ordering on a shared physical channel remains unspecified, as before (ADR-079) — this fix only strengthens the *same-CLL* ordering guarantee.
- The poll loop now performs one extra `logical_links` lock/check per dequeued item to test for a backlog; this is the same lock already taken for the pre-existing suspend siphon, not a new lock acquisition pattern.
- A cyclic COP's parked follow-up continuation (ADR-053) also goes through `drain_tx_held_backlog` (both call sites now covered), since a due continuation is dispatched directly from the `parked` loop and would otherwise be able to run ahead of an older backlog item still sitting in `tx_held`.
- A concurrent re-suspend racing an in-flight drain stops the drain early (remaining held items wait for the next resume); this matches how an item already past the suspend-check siphon in `dispatch_tx_item` always completes once dispatched, not a new inconsistency introduced by this ADR.

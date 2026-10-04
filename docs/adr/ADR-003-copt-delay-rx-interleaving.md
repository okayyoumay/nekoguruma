# ADR-003: CoptDelay RX Interleaving

**Date:** 2026-06-28  
**Status:** Accepted (extended by ADR-094 — the outer poll loop's own RX-polling cadence had a
separate, later-discovered starvation gap under sustained TX queue pressure, unrelated to the
`TxItem::Delay` case this ADR covers)  
**Affects:** `j2534-0404-service/src/service/events.rs`

## Context

The poll task uses a `biased; tokio::select!` with two branches:
- **TX branch** — dequeue and execute one `TxItem`
- **RX branch** — fire every `POLL_INTERVAL_MS` (10 ms) to read incoming frames

When `TxItem::Delay` was dequeued, the task called
`tokio::time::sleep(delay_ms).await` inside the TX branch body.  Because the
`biased` select gives the TX branch higher priority and the entire branch body runs
to completion before re-entering `select!`, the RX branch was blocked for the full
duration of the delay.

For delays in the range of hundreds of milliseconds (common in KWP2000 init
sequences), received frames would be silently dropped or buffered by the adapter
until the delay expired.

## Decision

`TxItem::Delay` is now handled with an internal loop that sleeps in
`POLL_INTERVAL_MS`-sized chunks and calls the extracted `poll_rx` helper after
each chunk.  The delay accuracy is bounded to ±`POLL_INTERVAL_MS` (±10 ms), which
is acceptable for ISO timing tolerances.

The `poll_rx` function encapsulates the full receive-fanout logic (previously
inline in the RX branch) and is shared between the outer timer arm and the delay
inner loop.

## Alternatives Considered

1. **Spawn a separate task for the delay** — maintains strict ordering guarantees
   but adds inter-task coordination complexity and a new channel.
2. **Non-blocking delay with a deadline future in the outer select** — cleaner
   architecture but requires restructuring the entire poll loop state machine.
3. **Abort the delay on cancel/shutdown** — not implemented; the worst-case
   overshoot is one `POLL_INTERVAL_MS` tick, which is acceptable.

## Consequences

- RX frames are never blocked for more than `POLL_INTERVAL_MS` during a delay COP.
- Frame ordering relative to the delay endpoint is preserved: frames arriving
  within the final `POLL_INTERVAL_MS` chunk may be delivered slightly before the
  delay's nominal expiry.
- Cancel/shutdown signals are not checked inside the delay inner loop; the outer
  loop catches them at the next iteration.
- Hard channel errors detected by `poll_rx` during a delay are handled immediately:
  the delay loop checks the return value and exits when `poll_rx` returns `false`.
  `handle_channel_hard_error` (called inside `poll_rx`) emits `PduCopstCancelled`
  for the Delay COP via `cancel_link_cops`; the delay handler therefore suppresses
  its own status emit to avoid duplication.  (Updated 2026-06-29 per ADR-019.)

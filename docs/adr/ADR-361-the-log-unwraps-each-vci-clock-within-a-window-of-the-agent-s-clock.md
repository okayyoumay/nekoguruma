# ADR-361: The Log Unwraps Each VCI Clock Within a Window of the Agent's Clock

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/clocks.rs` `VciClocks`, `comm_log.rs`
`CommLog::exchange_with`, `vci_connected`, the reopen scan; `comm_log/exchange.rs` `VciTime`,
`Pending`, `Rejected`, `pending_intervals`); design 8.11 (`clock_epoch`, "Timing judged",
`times`, `pending`, `rejected`); ADR-353, ADR-357

## Context

A VCI time is a 32-bit count of microseconds (ISO 22900-2:2022 8.1.6.1), so it wraps about every
71.6 minutes. It also starts over when the VCI resets or loses power. Design 8.11 aligns each
clock with the agent's clock, unwraps its times and opens a new epoch on a reset, on a backward
step that is not a wrap, or on a reconnect whose time base did not carry on. It does not say how
to tell these apart, what a `clock_epoch` event holds, or how epochs are numbered across runs of
the log.

The difficulty is the agent's view of a VCI time. The agent reads each item some time after the
VCI took it: the worker, the RPC and the event path all take time, and how long varies. That
delay has a floor but no ceiling. The drift bound and clock margin (ADR-357) bound the VCI's
clock against the agent's, not the delay.

The maintainer chose the rules below.

## Decision

1. **The state is per clock.** For each clock (`device`, `worker`; ADR-353) the log keeps:
   - the current epoch;
   - the latest unwrapped time and when the host read it;
   - the least delay seen in the epoch: the agent's read time less the unwrapped VCI time, with
     the agent time it was seen at.

   The least delay is the alignment. The state lives as long as one run of the log.

2. **A time read at agent time A is unwrapped within a window.**
   - Upper bound: A less the least delay, plus the tolerance. A time cannot have been taken
     after the agent read it. The tolerance is the drift bound times the agent time since the
     least delay was seen, plus the clock margin, both as `vci_connected` records them.
   - Lower bound: the clock's latest time less the clock margin. A clock does not run back.
   - Of the values "raw plus a whole number of wraps", exactly one in the window keeps the epoch.
     If it lies past a wrap, it is unwrapped past it.
   - No value in the window opens a new epoch. When the raw time is below the latest raw time,
     the reason is `backward_step`: it stepped back, and no wrap explains it. Otherwise the reason
     is `forward_step`: it ran ahead of the agent's clock.
   - More than one value in the window means the agent read nothing on the clock for longer than
     a wrap. How many wraps passed is unknown, so a new epoch opens with `gap_beyond_wrap`.

3. **Times are placed in the order the host read them.**
   - Read items, ordered by agent read time: unbound items, pendings and rejected results.
   - Then the final response's end, as read when the primitive ended (the entry's agent end
     time, which is no earlier than the true read).
   - A transmit done or a start that the response's own item carries comes before that end on
     its clock. It is placed at or before the end, in the end's epoch, without moving the
     clock's state.
   - The error time and any time read at no known agent time are placed nearest the clock's
     latest time. On a clock that has no epoch yet, they are read as at the entry's end.
   - The reading taken at the agent's deadline is read as at the entry's end.

4. **A reconnect opens a new epoch on every clock that had one.** `vci_connected` marks each
   clock. Its next time opens an epoch with `reconnect`. Whether the time base carried on over
   the loss is for `vci_loss_settled` to show; until that settles it, every reconnect counts as
   a break.

5. **`clock_epoch` is written before the first line that uses the epoch.** It carries:
   - the clock, the epoch, the reason (`first`, `backward_step`, `forward_step`,
     `gap_beyond_wrap` or `reconnect`), and the agent epoch;
   - `first`: the raw time that began the epoch and when the host read it;
   - `previous`: the latest raw time of the epoch it ends, with that epoch and when it was read,
     when this run had one.

   Each clock's first epoch in a run is written too (`first`), so every epoch a line names has
   its event.

6. **Epoch numbers are unique per clock within a log.** A reopened log goes on after the largest
   `clock_epoch` epoch the file holds for each clock, as the agent epoch does after the largest
   `agent_clock`. A new run starts with fresh state, since the worker and its clocks started
   over with the agent.

7. **The entry keeps the raw times, now with their epochs.**
   - Every VCI time in `times`, `pending`, `rejected` and `unattached` keeps its raw `us`
     (`vci_us`). It gains `epoch` once placed.
   - A time whose item names no clock has neither.
   - The unwrapped value is not written. A reader recomputes it from the raw times and the
     epoch's `first`.
   - A pending's interval is a forward difference of unwrapped times on one clock and epoch, so
     it is given across a wrap.

## Consequences

- A pending interval across a wrap is now given, and a reset no longer passes for a long gap or
  a wrap within one exchange.
- Accepted limitation: a reset that happens to land inside the window reads as a wrap. The
  window is as wide as the agent time since the last read, plus the excess delay of that read,
  so the chance grows with long silences on the clock. Nothing in the log can tell such a reset
  apart; `vci_loss_settled` does no better with a reading that fits both readings.
- A slow event path widens the window and never causes a new epoch, since the delay has no
  ceiling. A VCI clock that runs ahead of the agent's beyond the tolerance does cause one.
- A reconnect always opens an epoch. A clock that carried on over a short loss loses its
  continuity until `vci_loss_settled` lets it keep the epoch.
- Every log gains `clock_epoch` lines, and every placed VCI time an `epoch`; the `epoch` 0 that
  every time carried before is no longer written for a time with no clock.

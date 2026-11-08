# ADR-438: An Entry Keeps the Finished Time of a Collecting Window That Ran Its Course

**Date:** 2026-11-08
**Status:** Accepted
**Affects:** `agent` (`host.rs` `entry_response_times`, the entry built in `exchange`), design
8.11 `late_response`; ADR-436 Consequences, ADR-425 decision 5

## Context

ADR-436 keeps a collecting exchange whose own wait read a pending and then the primitive's
finished status, and lets its ECU's final answer settle it. Past the limit the pending put in
force, that final answer is the vehicle's only when the entry shows the VCI ended its wait at
the limit (`timing.window` `timeout_at_limit`), and the window takes that time from a receive
timeout or from the finished time of a collecting window.

The host zeroed every response time of an `Err` result. A window with a pending and no usable
answer has `Err(NoResponse)`, so its entry lost the finished time, its window was `not_judged`,
and its late final was `undetermined`; the same exchange's `primitive_ended` placed the finished
time and judged the answer. The zeroing is right for a response read before an agent deadline,
lost events or a lost VCI ended the wait: that response is not the entry's outcome. It never
applied to the finished time, which is no response's: a collecting primitive takes no
response times, and the finished status is read for every collecting finish whatever the
result.

## Decision

1. **The entry keeps the finished time, and only it, for `Err(NoResponse)` of a collecting
   exchange without an agent deadline.** This is the gate the collecting end already uses.
   An `Ok` result keeps every time as before; any other error drops every response time as
   before.
2. **A failed window keeps none.** A collecting result with a finished status read is `Ok`,
   `Err(NoResponse)` or `Err(PrimitiveFailed)`; a window that ended in an error is not judged
   at its limit (ADR-425 decision 5), and the judgement has no input for the kind of end, so a
   kept time would read as `timeout_early`.
3. **Classification and the keep are unchanged.** `vci_ended` is still a read receive timeout,
   or a finished status with an `Ok` collecting result; the rows and `late_final_settles` are
   not touched. Only `timing.window` and the reason text change: the entry's window is
   `timeout_at_limit`, `timeout_early` or `timeout_late`, and a late final past the limit on
   an entry at the limit is `vehicle`, as for the `primitive_ended`. A finished time on the
   worker's clock is not placed on the transmit's and stays unjudged.

## Consequences

- The entry's late final of a lone pending and the finished status is judged as the end's.
  The unit test pinning it `undetermined` now covers the shown-limit case and keeps
  `undetermined` for nothing shown (no finished time, or one on the worker's clock).
- A rejected-only window that the finished status ended gets the limit evidence too
  (`timeout_at_limit` at its restarted window); whether such a window counts as the VCI
  ending its wait, and so is kept, is not decided here.
- The log applies the end's finished time to a failed window in `primitive_ended`, against
  decision 2; classification is unaffected, `timing.window` can read `timeout_early`.

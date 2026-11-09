# ADR-440: A Collecting Window Whose Only Results Are Rejected Is Ended by Its Finished Status

**Date:** 2026-11-09
**Status:** Accepted (Consequences bullet 3, a non-collecting primitive finished on a wrong echo unchanged, superseded by ADR-443)
**Affects:** `agent` (`host.rs` `vci_ended`, `Sent::vci_ended`, `primitive_ended_of`;
`comm_log/exchange.rs` `Evidence::vci_ended`, the classification row for a collecting window the
VCI ended), design 8.11 `primitive_ended`, `late_response`, "Classification"; ADR-435 decision 4,
ADR-437 decision 6 and Consequences, ADR-438 decision 3 and Consequences, ADR-431 decision 3

## Context

ADR-437 made a rejected collecting result (a wrong echo, `positive_unusable`) no answer on its
identifier, so a window with only rejected results ends `positive_unusable`, is owed on the whole
receive table (ADR-435 decision 7) and is not settled. The host set `vci_ended` for a read
receive timeout, or for the finished status of a collecting window whose result was `Ok`
(ADR-435 decision 4, ADR-438 decision 3). A rejected-only window has `Err(NoResponse)`, so
neither its entry nor its `primitive_ended` was kept (`late_final_settles` false, no pending), and
the ECU's later usable answer on a silent or on the rejected identifier was `unattached`
(ADR-437 Consequences, ADR-438 Consequences).

The VCI took the rejected result as a response (the worker matched and bound it; the agent's test
is stricter) and ended the window by its own timer from it (ADR-437 decision 6), as it does for a
usable answer. The maintainer decided that such a window counts as the VCI ending its wait.

## Decision

1. **`vci_ended` is a receive timeout read, or, for a collecting primitive the finished status
   ended, a window that took a result.** A window took a result when its result is `Ok`, or
   `Err(NoResponse)` with at least one rejected result read. This gate is on the kind of result,
   not on "any rejected result was read": every other result (`PrimitiveFailed`, a lost VCI,
   lost events, a cancel, an agent deadline) leaves `vci_ended` false, so a link-error window
   stays unkept and released after its `primitive_ended` unless a pending is open (ADR-436), as
   a usable answer beside a link error already is. One shared function serves both entries: the
   exchange's entry reads `Sent::rejected`; `primitive_ended_of` reads the stray's `rejected_read`
   flag (whether the agent's wait read a rejected result) or its own `Progress::rejected`.
2. **Nothing else in the log's rules changes.** The entry and the renewed record are kept by
   `late_final_settles`; `late_response_owner` owns the late usable answer on the rejected
   identifier or a silent one; the restated end replaces the rejected end and `end_rejected`
   falls (ADR-437 decision 3); the late final's judgement is ADR-436 decision 5 and ADR-438.
   `silence_is_result` (a suppressed positive, non-collecting) still reads a receive timeout only.
3. **The classification row "collecting, `vci_ended`, window early or late: `system`" applies to
   such a window.** The entry's own classification for a rejected-only (or rejected plus pending)
   window whose finished time, judged from the window restarted at the rejected result, is
   `timeout_early` or `timeout_late` is `system`, where ADR-438 decision 3 left it
   `undetermined`. At the limit, or not judged, it stays `undetermined`. This is accepted: the
   VCI ended the window off its own timer, as for a usable answer.

## Consequences

- Amends ADR-435 decision 4, ADR-438 decision 3 and extends ADR-431 decision 3; closes the
  Consequences bullets of ADR-437 (rejected-only window unkept) and ADR-438 (whether it counts
  as the VCI ending its wait).
- Pending-only and silence-only windows ended by the finished status are unchanged
  (silence-only stays unkept, reachable only from a library that reports no receive timeout for
  an unanswered window).
- A non-collecting primitive the worker finished on a wrong echo (ADR-359 item 3) is unchanged:
  `Progress` records no finished time for it, and `vci_ended` stays false.
- Tests: the host's truth table for the shared function, the equivalence of the entry's gate
  with `collecting_end`'s rejected flag, the log's keep, `late_response` and early/late rows, and
  the runner's own-wait presence run with a wrong echo then a usable late answer.

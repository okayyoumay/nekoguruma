# ADR-443: A Primitive Finished on a Wrong Echo Is Ended by the VCI

**Date:** 2026-11-09
**Status:** Accepted
**Affects:** `agent` (`host.rs` `vci_ended`, `unanswered_wrong_echo`, `unanswered_end`, `Progress::finished`,
`Sent::vci_ended`, `Sent::silence_is_result`, `Stray`, `primitive_ended_of`; `comm_log.rs`
`PrimitiveEnded`, `CommLog::primitive_ended`; `comm_log/exchange.rs` `Kept::answered_placed`),
design 8.11 `rejected`, `primitive_ended`, `late_response`; ADR-359 item 3, ADR-431 decision 3,
ADR-435 decision 7, ADR-440 Consequences

## Context

ADR-359 item 3 ends a primitive that is not collecting, and whose worker finished it on a result
that does not answer the request (a wrong echo of positive form) with no usable answer, as
`positive_unusable`; the request stays owed (ADR-435 decision 7 owes a rejected end on the whole
receive table for any exchange). ADR-440 made a collecting window that took only rejected results
count as ended by the VCI, but left the non-collecting case as it was, because `Progress`
recorded no finished status for it. So such an exchange was owed and never kept, and the ECU's
usable answer delivered later was `unattached`, with no owner named.

The VCI took the wrong echo as the response and finished the primitive (one receive cycle, no
timer). No later result can be bound to the primitive, so any usable answer arrives unbound, which
is exactly the situation `vci_ended` gates (ADR-440's reason, here more directly).

## Decision

1. **`vci_ended` is a receive timeout, or the finished status of a collecting window that took a
   result (ADR-440), or the finished status of a primitive that is not collecting whose last
   result was a wrong echo with no usable answer after it** (no pending or answer after it, no
   receive timeout, no agent deadline): the condition `unanswered_end` turns into
   `positive_unusable`. One function states it (`unanswered_wrong_echo`) and both the end and
   `vci_ended` call it, so they cannot drift. `Progress::finished` records the finished status
   for every primitive (not a cancel) and `Sent` carries it. A usable answer leaves `vci_ended`
   false for a non-collecting primitive (ADR-442 decision 2 depends on that).
2. **`silence_is_result` reads the receive timeout only** (ADR-408, ADR-440 decision 2), not
   `vci_ended`: a suppressed request whose primitive finished on a wrong echo is not answered by
   silence.
3. **A late final answer to a non-collecting exchange is a usable response.** The restated
   evidence (`Kept::answered_placed`) clears `end_rejected`, so it is `positive` with an answer
   head and settles what was owed. `PrimitiveEnded` carries `end_rejected` for a non-collecting
   end and `primitive_ended` copies it; `primitive_ended_of` derives the end as the entry does, so
   a stray's end read after the deadline is `positive_unusable` (rejected, ended by the VCI), not
   `no_response`. `Stray` carries the wrong echo the entry's wait ended on so the stray's wait
   reads on from it. `no_response` with `vci_ended` stays reserved for a receive timeout.
4. **The late answer is judged as any late response:** `system` in time and `undetermined` past
   the limit (the VCI ended its wait before the limit), never `vehicle`.

## Consequences

- Supersedes ADR-440 Consequences bullet 3 (a non-collecting primitive finished on a wrong echo
  is unchanged); amends ADR-431 decision 3 (the renewed record's `vci_ended`) and ADR-359 item 3
  (the request stays owed, and its usable answer is its `late_response`).
- A primitive cancelled after a wrong echo keeps the end `positive_unusable` but is not ended by
  the VCI (no finished status). A negative of another service (`PatternMismatch`) that ended a
  non-collecting primitive stays `no_response` and unkept (residual).
- Tests: the host's truth table and the end-equals-`vci_ended` shapes, the suppressed request,
  the stray's end; the log's `owed_ids`, restatement, keep and `primitive_ended`; the runner's
  end-to-end reads, with the wrong echo in the entry's wait and across the deadline.

# ADR-447: A Wrong Echo Bound to a Later Primitive Is First Offered to the Kept Owed Exchanges

**Date:** 2026-11-10
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `CommLog::exchange_with`, `Inner::offer_rejected`,
`late_response_owner_with`, `Inner::late_response_written`, `Inner::late_response_unowned`;
`comm_log/exchange.rs` `Rejected`, `RejectedItem`, `Evidence::end_placed`, `classify`), design 8.11
"Owed request", `rejected`, `late_response`, "Classification"; ADR-359 items 1 and 3, ADR-368
item 4, ADR-375, ADR-387, ADR-443 Context and decision 3

## Context

ADR-443 keeps a non-collecting exchange the VCI finished on a wrong echo, and assumes that the
ECU's usable answer to it arrives unbound, since the finished primitive can bind no further
result. A worker may instead bind that answer to the next request's primitive. The host rejects
it there as a wrong echo (ADR-359) and the log listed it in the later exchange's `rejected` and
did nothing more: the earlier exchange stayed kept and owed until the job ended or an accepted
restart, with nothing saying that its answer had been read.

Which workers can produce this is narrow. The worker's response pattern is the host's positive
prefix with a full mask. The J2534 worker compares only as many bytes as the shorter of the two
and discards what no primitive expects (ADR-100), so a result it binds that the host rejects as a
wrong echo is always shorter than the request's positive prefix and equal to its beginning. A
conforming D-PDU library binds no response shorter than its pattern (ISO 22900-2:2022
10.1.4.19.3), so it produces no wrong echo. A full-length late answer bound to the next
primitive begins with that primitive's own prefix, so it is that primitive's usable answer, not a
rejected result. The lone-owner settlement below is therefore reachable only with a library that
binds beyond its pattern, or with the fake worker.

The maintainer decided on 2026-11-10 that the log matches such a result to the kept owed
exchanges.

## Decision

1. **The log offers each `wrong_echo` result of a non-collecting exchange to the kept owed
   exchanges**, through the one owner function the unbound items use (`late_response_owner_with`,
   the caller supplying whether the exchange in flight could own the item), in the order the host
   read it among the unbound items, before the exchange's entry. The time the log placed for the
   result at the entry is reused, never placed twice (ADR-361). A result with no VCI time is not
   offered; one whose identifier is 0 is offered with none.
2. **One owner settles; several are left unowned.** One owner gets its `late_response`, which
   restates and judges it as any late response (ADR-443 decision 4). Several get one unowned
   `late_response` naming them all, each at least `undetermined` and still owed. Neither writes an
   `unattached` line: the result is already listed in the later entry's `rejected`. The line says
   `primitive` is `in_flight`.
3. **The rejecting exchange counts as an owner in flight only for its own response cut short.**
   That is when the result is a non-empty, proper beginning of its own positive prefix. It then
   always gives the unowned form and never settles an earlier exchange, so the log does not decide
   between possible owners (ADR-375) or take an earlier exchange's answer for a truncated answer
   to the later request.
4. **The later entry keeps its facts and takes a new reason.** The result stays in `rejected`
   with `late_response`, the `seq` of the line written for it. The outcome (`positive_unusable`),
   `end_rejected`, `vci_ended`, the keep and the owed rows are unchanged (ADR-443, the timing
   of ADR-437 decision 6). The classification stays `undetermined`, but by a row of its own, read
   from one predicate (`Evidence::end_placed`) above the rows that take the result for the ECU's
   answer to this request. The line is written before the entry, so the entry's history already
   sees the owner settled.
5. **Scope.** Not offered: a `pattern_mismatch` result, a collecting exchange's rejected answers,
   and a result read in a stray's wait after the deadline. A stray's carried wrong echo was
   offered at the entry and adds no read for the stray's end.

## Consequences

- On the J2534 worker only the unowned form occurs.
- The host and the job's result are unchanged.
- Revises ADR-359 item 1 (the list item gains `late_response`) and item 3 (a rejected result
  counts for nothing else), ADR-443's Context and decision 3 (the usable answer may come bound to a
  later primitive), and ADR-368 item 4 (an unowned `late_response` names an `unattached` line: not
  for a result bound to the exchange in flight).
- Residual: a collecting exchange's rejected answers are not offered, and a result read in a
  stray's wait after the deadline is not offered; both are in the backlog.
- Tests: the log's one owner, several, none, cut short, scope, read order and placement; the
  host's carried echo; the runner's end-to-end reads.

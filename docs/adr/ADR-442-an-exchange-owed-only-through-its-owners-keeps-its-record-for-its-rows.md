# ADR-442: An Exchange Owed Only Through Its Owners Keeps Its Record for Its Rows

**Date:** 2026-11-09
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `ExchangeEntry::owed_through_owners`; `comm_log.rs`
the entry's keep in `CommLog::exchange`), design 8.11 "Settlement", `late_response`; ADR-365
decision 1, ADR-436 decisions 1 and 3, ADR-439 Consequences, ADR-441 decision 2 and
Consequences

## Context

ADR-439 and ADR-441 refresh the rows of the exchanges the log keeps (`Kept`) when an owner
lets go: `refresh_kept_beyond_window`, `refresh_window_survivors`, `refresh_row_owners` and the
release in `free_owner_answer` reach a kept record or a window entry. A non-collecting
exchange with a final response whose answer had an owner (an earlier owed request that could
own it) is owed on its identifier (`LinkState::record`), yet the log kept no record of it:
the entry's keep was gated on a late final answer settling the exchange (the VCI ended it, or
a pending is open on it, ADR-365 decision 1, ADR-436 decisions 1 and 3), and this exchange
has its answer. Once it left the window nothing visited it. Its row stayed after its owner
let go, and its send stayed counted among the unknown ones (`unknown_sends`), so it still
counted as an owner of a late response and made a single kept owner's late final answer
ambiguous. The case also arose when an accepted restart was what let the owner go (ADR-441
refreshed only the survivors in the window or kept).

## Decision

1. **The entry keeps its record when it is owed only through its owners.**
   `ExchangeEntry::owed_through_owners` holds when the exchange settles
   (`LinkEffect::settles`: a final response, no identifier open on a pending or silent) and an
   owner stands for its answer. The keep in `CommLog::exchange` takes it as a further reason
   beside the late-final, unresolved-start and unended-VCI reasons. The record exists for the
   exchange's rows: `LinkState::keep` still inserts it only while the exchange has rows.
2. **The record owns no late final answer.** `Kept::late_final_settles` is false for it, so the
   gate on a single owner in `late_response_owner` does not give it one. While its owner
   stands it is named, and floored to `undetermined`, among the owners of an ambiguous late
   response (design 8.11 `late_response`), as any kept exchange is.
3. **The existing visits release it; no path is added.** Every path ADR-439 and ADR-441 define
   now reaches it as it does a kept non-collecting exchange with a final response: an owner's
   settlement or `primitive_ended` (`reclassify_after`), an accepted restart in time or late
   (`reset_after`: dropped when before `earliest`, otherwise visited as a survivor or as an
   exchange after the restart). Placed alone, the visit releases the rows and the record
   (`free_owner_answer`) and lowers `unknown_sends`; an exchange released after a late
   restart counts its matched answer once, as ADR-441 decision 7 says. With two owners it
   stays until the second lets go; an owner released with no answer (`release_kept`) leaves
   its rows, so the record's row stays right.
4. **Scope.** An exchange that does not settle is not kept by this rule: one that ended on a
   rejected result (ADR-437), one open on a pending (kept already, ADR-436), one with a silent
   identifier. A collecting exchange with a row an owner holds beside a silent row, whose VCI
   did not end its window, stays unkept (ADR-435 decision 4) and its row can still go stale
   beyond the window; that stays a backlog item. A link error or a lost VCI lets no owner go
   (the owed requests survive them), so they are out of scope.

## Consequences

- ADR-439's last Consequences bullet (an owed exchange with no kept record never visited) and
  ADR-441's second Consequences bullet are closed for an exchange that settles; ADR-441
  decision 2's "an unkept exchange beyond the window has no evidence left" no longer holds
  for it. ADR-365 decision 1 and ADR-436 decisions 1 and 3 gain a reason to keep a record
  that is not about a late final answer.
- A late response read while such an exchange and its owner stand now lists the exchange
  among the owners and floors it, where it was left out; the owner count of a single kept
  owner is no more inflated by it once the owner has let go.
- `place_error` sees the record's evidence (design 8.11 "Link error", ADR-384), as it does for
  any kept exchange.
- The record costs one `Kept` per such exchange while its owner stands and it is the owner's to
  release; it is released with the owner, so the log's size is bounded as before.

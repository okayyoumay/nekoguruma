# ADR-442: An Exchange Owed Only Through Its Owners Keeps Its Record for Its Rows

**Date:** 2026-11-09
**Status:** Accepted (decision 1 extended by ADR-445 to the entries that settle at a primitive's end and by a late response, with the owners sent before the exchange; the release after the end keeps such a record; decision 2's "the record owns no late final answer" has an exception: a record kept at a collecting end whose finished window shows the VCI ended owns one, ADR-445 decision 4; decision 3's release lowers `unknown_sends` only for a send of the link's current opening, ADR-448; decision 2 extended by ADR-450: the record owns no lone late pending or start either, unless it owns a lone late final; the third Consequences bullet's late start and pending read accordingly)
**Affects:** `agent` (`comm_log/exchange.rs` `ExchangeEntry::owed_through_owners`,
`ExchangeEntry::settled_answer`; `comm_log.rs` the entry's keep in `CommLog::exchange`,
`Inner::late_item`), design 8.11 "Settlement", `late_response`, `late_item`; ADR-365
decision 1, ADR-436 decisions 1 and 3, ADR-439 Consequences, ADR-441 decision 2 and
Consequences

## Context

ADR-439 and ADR-441 refresh the rows of the exchanges the log keeps (`Kept`) when an owner
lets go: `refresh_kept_beyond_window`, `refresh_window_survivors`, `refresh_row_owners` and the
release in `free_owner_answer` reach a kept record or a window entry. An exchange that
settles (a final response and no identifier open on a pending or silent, collecting or not)
whose answer had an owner (an earlier owed request that could own it) is owed on its
identifier (`LinkState::record`), yet the log kept no record of it:
the entry's keep was gated on a late final answer settling the exchange (the VCI ended it, or
a pending is open on it, ADR-365 decision 1, ADR-436 decisions 1 and 3), and this exchange
has its answer and its window ended by it. Once it left the window nothing visited it. Its row stayed after its owner
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
   its rows, so the record's row stays right, and the record is then the only kept candidate
   for a late response on its identifier: an ambiguous late response lists it alone
   (`Several([record])`) and floors it, where it was named in no entry before (the
   unattached line was written before); an ambiguous start or pending likewise.
4. **Scope.** An exchange that does not settle is not kept by this rule: one that ended on a
   rejected result (ADR-437), one open on a pending (kept already, ADR-436), one with a silent
   identifier. A collecting exchange with a row an owner holds beside a silent row, whose VCI
   did not end its window, stays unkept (ADR-435 decision 4) and its row can still go stale
   beyond the window; that stays a backlog item. A link error or a lost VCI lets no owner go
   (the owed requests survive them), so they are out of scope.
5. **A `late_item` settling entry ends no owed request.** An indication is no final response,
   so `Inner::late_item` hands the later exchanges an answer that frees no owner
   (`ExchangeEntry::settled_answer` with `ends_owed` false, whether or not the entry's evidence
   settles). An exchange that stays owed through it, a record kept for its rows or a
   collecting record kept with its VCI ended and owners (kept before this ADR too), stays among
   the owners of the exchanges logged after it, and the entry refers to it alone. Before, the
   entry passed the exchange's own settling as a final response, which removed it from those
   owners and reclassified them as if it had answered. This changes behaviour for the records
   kept before this ADR.

## Consequences

- ADR-439's last Consequences bullet (an owed exchange with no kept record never visited) and
  ADR-441's second Consequences bullet are closed for an exchange that settles; ADR-441
  decision 2's "an unkept exchange beyond the window has no evidence left" no longer holds
  for it. ADR-365 decision 1 and ADR-436 decisions 1 and 3 gain a reason to keep a record
  that is not about a late final answer.
- A late response read while such an exchange and its owner stand now lists the exchange
  among the owners and floors it, where it was left out; the owner count of a single kept
  owner is no more inflated by it once the owner has let go.
- A late indication sees the record too, among the kept exchanges `late_transmit_owners`,
  `late_start_owners` and `late_pending_owners` read (design 8.11 `late_item`). A transmit
  indication on the request identifier of a record whose send is still `unknown`, when every
  exchange on the link with an unknown send is kept and the record is the only one on that
  request identifier (`late_transmit_owners`), now has it as its lone owner and settles it as its late item
  (its send confirmed, the unknown count lowered), where it was `unattached`. An ambiguous
  start, pending or transmit indication read while the owner stands now has the owners
  `[owner, record]` and every owner known, so `late_item_unowned` floors the record too. This
  holds with the record in the window as well.
- A `late_item` settling entry ends no owed request, so a record kept for it stays among
  the owners of the later exchanges (decision 5), which holds for the records kept before this
  ADR too.
- `place_error` sees the record's evidence (design 8.11 "Link error", ADR-384), as it does for
  any kept exchange: after a restart clears the window it can answer `BeforeWindow` from the
  record's times.
- Cost: the record costs one `Kept` per such exchange while its owner stands, and it is the
  owner's to release. That is not bounded by the window. With an owner kept on a receive
  timeout whose answer never comes, every later answered poll of the same request is owed
  through it and kept, each with a full `Kept`; each later settlement's
  `refresh_kept_beyond_window` visits all N records and `kept_entry` reads all N owed rows,
  O(N squared) per settlement under the log lock; an ambiguous late response floors and lists
  all N. The owed rows already grew by one per poll, and timed-out exchanges are already kept; what
  is new is a full `Kept` per answered poll and the visit of all of them at each settlement.
  A backlog item measures it in a long polling run and bounds it if it matters.

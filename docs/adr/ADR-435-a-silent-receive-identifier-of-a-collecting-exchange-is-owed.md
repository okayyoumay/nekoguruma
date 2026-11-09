# ADR-435: A Silent Receive Identifier of a Collecting Exchange Is Owed

**Date:** 2026-11-08
**Status:** Accepted (Decision item 4's release amended by ADR-436: an exchange a pending is open on, or whose kept record notes one open at its keep or after the end's renewal, is kept after any end, and a late final answer on its silent identifier settles it there; decision 7 stands as an invariant for a collecting request, whose rejected results now leave their identifiers silent, and the Consequences' bullet on whether a rejected collecting answer leaves its identifier unanswered is closed, ADR-437; Decision item 6's window scope superseded by ADR-439: the rows of a kept exchange beyond the window follow its owners too; its first Consequences bullet closed; Decision item 4's parenthetical "(a receive timeout, or the finished status of an answered window)" superseded by ADR-440: the finished status of a window that took a rejected result ends it too)
**Affects:** `agent` (`comm_log/exchange.rs` `Evidence::owed_ids`, `Evidence::silent_ids`,
`Evidence::settles_owed`, `Evidence::unanswered_ids`, `Evidence::answerable_ids`, `LinkEffect`,
`Kept::answered_placed`, `Kept::late_start_for`, `LinkState::reclassify_one`,
`LinkState::refresh_owed_rows`, `LinkState::reset_after`, `LinkState::reclassify_after`,
`LinkState::error_after`, `LinkState::frees_owner`; `comm_log.rs` `primitive_ended`,
`settled_final_time`, `late_response`), design 8.11 "Owed request", `late_response`; ADR-429 decision 2, ADR-432
decision 2, ADR-433 decision 6 and Consequences

## Context

ADR-432 owes a collecting exchange with a receive table and answers only on the identifiers a
pending is open on, and, when it ended with a final response and nothing is open, on the
identifiers of final answers an owed request could own, else on the first final answer's.
Another identifier's final answer therefore ended the exchange's claim on a table identifier
that had said nothing: a presence window answered on 0x7E9 and ended with 0x7E8 silent was
owed on no identifier it still lacked an answer from, the 0x7E8 ECU's later final answer was
`unattached`, and a resend's answer there was placed alone. An answer on one identifier shows
nothing about the others: the ECU behind a silent identifier may simply have answered after the
VCI ended the window.

## Decision

1. **With a receive table and answers, `Evidence::owed_ids` is the union, each identifier once,
   of three lists, in this order:** the identifiers a pending is open on (as before); the silent
   identifiers, every table identifier with no named final answer standing, in table order
   (`Evidence::silent_ids`), whatever another identifier answered; and, when the exchange ended
   with a final response, the identifiers of the final answers whose own histories show an owner
   (an answer that names no identifier adds every table identifier), whether or not anything
   else is owed. The fallback to the first final answer's identifier is removed: with no silent
   identifier every table identifier has a named final answer, so nothing is left for it to
   cover. This supersedes ADR-432 decision 2's "an identifier with a final answer standing is
   never owed" and its first-final fallback, and ADR-429 decision 2's "only an open pending owes
   an identifier once an answer exists", on a link with a receive table. A physical link (no
   table) and a request that is not collecting are owed as before. The rule reads the evidence's
   `collecting` and `receive_ids`, not the request's origin, so it holds for every collecting
   exchange.
2. **`settles_owed` also requires no silent identifier**, and `LinkEffect::settles` reads that
   one predicate (the second copy of it is gone), so `record` sets the owed rows of an exchange
   that has a final answer and a silent identifier, and the exchange is kept. It is
   `silent_ids`, not `owed_ids`, that settlement tests: `owed_ids` also names identifiers owed
   because an earlier exchange could own the answer there, which do not stop a final response
   from settling it.
3. **A final answer that names no identifier covers none.** The exchange is owed on every table
   identifier with no named final answer. An unnamed late final answer is keyed on the one
   identifier that is unanswered (silent, or with a pending open) when there is exactly one,
   which folds that identifier's pending and covers it; an identifier owed only because an
   owner could own a final answer there does not count (`Evidence::unanswered_ids`). With
   several unanswered it is appended unnamed and settles none, and the final answers that name
   an identifier settle them. This reads ADR-433 decision 6's "open" as "unanswered".
4. **The release after `primitive_ended` is ADR-431's, unchanged.** A silent identifier alone
   keeps no record (ADR-436 keeps an exchange whose kept record notes a pending
   open at its keep or after the end's renewal): the exchange is kept when a pending is open or
   the renewed evidence shows the VCI ended its wait (a receive timeout, or the finished status of an answered window), and
   released after a link error, a cancel or an unknown end, when its owed rows remain and a
   later final answer on the silent identifier is `unattached`, as for an ordinary exchange.
5. **`primitive_ended` takes the time of the answer from whether a final answer exists**, not
   from whether the exchange settles (`settled_final_time`): an exchange with a final answer on
   one identifier and a silent one still has that answer's time, which ADR-384's test of a late
   link error against the answer reads.

6. **The rows follow the owners as they are now.** An exchange that stays owed re-derives its
   rows from its answers' histories whenever a settlement frees an answer of it
   (`LinkState::reclassify_one`, through `refresh_owed_rows`, which also refreshes the evidence
   the log kept), and `late_response` takes the histories of the restated answers again before
   it sets the rows, as `primitive_ended` does. An identifier an exchange was owed on only
   because an owner could own its final answer there goes when that owner no longer can, and
   the identifiers dropped free the later answers on them; an exchange left with no row is owed
   no more, like one placed alone.
7. **A result the worker rejected is no answer** (ADR-359): an exchange whose end is a rejected
   result is owed on every table identifier, as one with no answer is, whatever else it is owed
   on, since it is not settled either. The set does not depend on any owner, so it cannot grow
   after the fact; and a refresh of rows (decision 6) only drops rows, never adds one: the new
   rows are the old ones that the exchange is still owed on. A final answer delivered late is a usable
   response, so the restated evidence no longer carries the rejection. Since ADR-437 a collecting
   request's rejected results leave their identifiers silent, so for it the rule is a stated
   invariant (every identifier is owed whenever the end is a rejected result) rather than an
   addition, and the flag is derived from the answers.

## Consequences

- The refresh of decision 6 reaches the exchanges in the link's window. An owed exchange that
  has left the window keeps a row it holds only because an owner could own its answer there,
  after that owner lets the answer go, until its own `primitive_ended` or `late_response` takes
  the answers' owners again and drops it, or an accepted ECUReset or change of session ends it;
  until then it counts as an owner of a late final answer on that
  identifier, which is then `unattached`.
- A resend that is silent on an identifier the first exchange is owed on shares the ownership
  of a late final answer there: the answer has two owners, is `unattached`, and both exchanges
  are at least `undetermined` (design 8.11 "Owed request"). More resends are `undetermined`;
  the owed rows of a presence exchange grow to the size of the table. The two runner tests of
  a pending read in the resend's wait and of a silent presence exchange assert this outcome.
- A collecting answer that is a result the worker rejected (a wrong echo, `positive_unusable`)
  stood as a final answer on its identifier, as the entry's end took it (ADR-429), so the
  identifier was not silent. ADR-437 closes this: such a result leaves its identifier unanswered
  and the entry's end passes it over for a usable answer.
- ADR-432's deferred bullet on an identifier that stays silent beside another's final answer is
  closed, and ADR-433's Consequence that it stands is superseded by decision 1. ADR-429
  decision 2, ADR-432 decision 2 and ADR-433 decision 6 and Consequences are marked as
  superseded in their Status lines.

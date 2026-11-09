# ADR-444: The Answer of the Exchange a Late Error Falls After Counts by Its Stored Time

**Date:** 2026-11-09
**Status:** Accepted (Consequences' first bullet revised by ADR-445: an exchange kept at its end for its rows counts its matched answer once, at its release, beyond the window too)
**Affects:** `agent` communication log (`comm_log/exchange.rs` `LinkState::error_after`,
`Logged::after`, `Logged::matched_at`, `LinkState::kept_matched_after`); design 8.11
"Settlement"; ADR-384 item 1, ADR-385 decision 2 and Consequences, ADR-441 decision 7

## Context

A late link error is placed after an exchange `s` when `s`'s entry came before it and the next
exchange started after it. `error_after` took `s` out of the recount by position: the exchanges
after `s` did not count `s`'s matched answer in the error's epoch, whatever the answer's
stored time. Its kept record, however, follows the stored time (ADR-385 decision 5): the
record's matched answer ends only when the time is not shown after the error, and the restart's
recount counts it once `s` has left the window. The same answer therefore counted or not
depending on whether it was read before the error and on whether `s` was still in the window.
ADR-385 decision 2 already says the stored time decides for every other exchange; ADR-384's
positional treatment of `s` was the one place left that did not follow it.

## Decision

1. The answer of that exchange counts for the exchanges after it and for the link, as a matched
   answer and as an answer since the error, when its stored time passes the one test of ADR-385
   decision 2 and no link error in its own entry's evidence came after the answer or in no known
   order with it. The second condition is the one the kept record applies, and it covers an
   error placed within the exchange.
2. The window follows the record. `Logged::after` shifts as `Kept::clears` does, the exchange
   itself included, so a window entry and its record hold one epoch. `Logged::matched_at` shifts
   with the later exchanges' when decision 1 holds, and the recount takes the exchange in then.
   Its own `Logged::clears` stay, the epoch its history was taken in, and it is not
   reclassified.
3. `Kept::matched` and the way it ends are unchanged (ADR-385 decision 5), so a later offer of
   an answer already counted is never new: a final response, a transmit re-offer, a refresh
   beyond the window and an earlier owner's settlement add nothing.
4. Placement of the error is unchanged (ADR-385 decision 4).

## Consequences

- The counts no longer depend on the order in which the answer and the error are read, nor on
  whether the exchange is still in the window, while the exchange has a record or is in the
  window. An exchange a final response settled and released has no record: its answer counts in
  the window's recount while the exchange is in the window, and not once it has left it. A
  read-order case stays open: when the exchange at the error's place has its own error before
  its final response, its `matched_at` can alias a later exchange's `clears` and a late accepted
  restart then counts it for that exchange in one order and not in the other.
- A later silent exchange of the same match can be `vehicle` where it was `undetermined`; it
  rests on the same one-clock ordering the error-first reading already used.
- Still counted too few, as for every exchange: an in-time clear between the exchange and the
  later one, an error placed after a later exchange, a final response on another clock, and the
  released exchange above once it has left the window.
- Invariant: a window entry's `after` equals its record's `Kept::clears`.

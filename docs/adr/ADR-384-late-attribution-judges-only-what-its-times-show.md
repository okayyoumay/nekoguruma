# ADR-384: Late Attribution Judges Only What Its Times Show

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/exchange.rs` `ErrorPoint`,
`LinkState::place_error`, `LinkState::error_after`, `SettledAnswer`, `LinkState::take_answer`,
`LinkState::reclassify_after`; `comm_log.rs` `Inner::late_link_error`,
`Inner::late_item_unowned`, `CommLog::primitive_ended`, `Inner::late_response`,
`Inner::late_item`); design 8.11 "Settlement", `late_item`; ADR-368; ADR-370 item 4; ADR-372
items 2 and 3; ADR-375 item 5; ADR-376

## Context

ADR-375 item 5 left the log's existing judgements standing until each area was checked against
it. This is the last of those checks: the attribution of late responses, pendings, starts and
link errors (design 8.11 "Owed request", `late_response`, `late_item`). The earlier checks were
ADR-376 (the exchange classification), ADR-377 (the timing) and ADR-378 (the settlements).

Most of the area passes: the owners a late item may have (ADR-346 item 6's coarse rule, which
the maintainer chose), the settlement of a late response or pending by its time, and the choice
of the exchanges a late link error may have come during. Two gaps were already recorded as their
own work and are not part of this change: an item placed before its sole owner's transmit done,
and an unbound transmit indication attached without a test of its time.

Four judgements rest on an ordering their times do not show:

1. **An exchange with only end-side times holds no late link error.** ADR-372 item 2 placed an
   error within an exchange only between its earliest placed transmit done or response start
   and its latest time. An exchange with neither has only the end of its response, for example
   a single-frame answer on a link whose library refused transmit indications. An error before
   that end was placed after the exchange before it, as falling between the two. An error after
   that end was placed there too, so the exchange's answer, which came before the error, counted
   as a matched answer since it.
2. **A late answer counted since an error whatever its time.** ADR-372 item 3 counted an answer
   read late for the exchanges after a late error placed after its exchange, "as it would have
   had the error been recorded in time". When both times are on one clock and epoch, they show
   which came first, and an answer before the error is no answer since it. When they do not
   show it, counting it presumed the reading that excludes a lost frame (H2).
3. **An ambiguous later start did not unsettle a kept start.** A start-of-message indication
   that only one owed exchange could own records that its response started in time (ADR-370
   item 4). A later start that this exchange or another may own left that record in place. A
   late response then settled as `system`, although the ECU may have started again late.
4. **An error before the window reclassified none of the window's exchanges.** ADR-372 item 3
   restarted the counts from the window's exchanges and left their classifications. Those may
   rest on matched answers from before the window, which the error ends. The window alone gives
   the counts to reclassify them from.

## Decision

1. **An end-side exchange may have held the error.** Take an exchange with a latest placed time
   L on the error's clock and epoch, but no placed transmit done or response start:
   - An error at t with t no later than L, and later than every earlier exchange's latest time,
     may have come during it (`MaybeWithin`).
   - Such an exchange takes the error as a possible link error (H3) only, not as a fault seen in
     it. The counts start again before it, as for an error within an exchange.
   - The `late_item` entry says that the error came during it, or between it and the exchange
     before.
   - An error after L is after that exchange.

   The maintainer chose H3 only (2026-10-29). Such an exchange has no transmit time, so its
   timing is not judged and its classification is already `undetermined` (ADR-376, ADR-377).

2. **A late answer counts since an error only when shown after it.** The link state keeps the
   latest late link error placed within or after the exchanges it keeps, and whether one could
   not be placed. A late answer, a final response or a pending ADR-368 counts as matched, still
   counts as an answer. It counts as a matched answer and as an answer since an error only when
   no such error applies to its exchange, or its time is shown after the error's on one clock
   and epoch. An answer before the error, or one whose order against it is not shown, is not
   counted there: on another clock or epoch, with no time, or after an error that could not be
   placed. The maintainer chose this (2026-10-29). A start is no matched answer, as before
   (ADR-354).

3. **An ambiguous later start opens a kept start.** When an owed exchange keeps a start that
   started in time, and a later start is read that it or another exchange may own, the kept
   start's record is dropped unless the new start is shown before it on one clock and epoch. A
   late response is then judged by its own time. A later start that the exchange alone owns
   replaces the record, as before.

4. **An error before the window reclassifies the window.** The counts start again from the
   window's exchanges, and each of them is reclassified from the window alone, as an error
   placed after an exchange reclassifies those after it.

## Consequences

- More exchanges settle as `undetermined` after a late link error, and none settles `vehicle`
  or `system` on an ordering its times do not show.
- On a J2534 link, where an error is often not placed, a late answer read after an unplaced
  error never counts as a matched answer since it.
- The link state keeps one merged late-error time per clock and epoch: the latest time, and the
  newest exchange it may follow. It can only count fewer answers since an error, never more.
- For an error before the window, `not_reclassified_through` now names only the newest exchange
  that has left the window, since every window exchange is reclassified. It is left out when
  the error came during an owed exchange kept beyond the window and no exchange that has left
  the window is newer than that one.
- An end-side exchange's own `reason` keeps the H3 wording of a possible link error. The
  `late_item` entry's text names where the error may have come.

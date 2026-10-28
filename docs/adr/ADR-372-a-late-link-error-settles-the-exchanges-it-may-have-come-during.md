# ADR-372: A Late Link Error Settles the Exchanges It May Have Come During

**Date:** 2026-10-28
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `CommLog::exchange_with`,
`Inner::late_link_error`, `Inner::read_since`, `Inner::note_reads`, `Settled::after_link_error`;
`comm_log/exchange.rs` `ErrorPoint`, `LinkState::place_error`, `LinkState::written_since`,
`LinkState::error_in`, `LinkState::error_after`); design 8.11 ("Settlement", `late_item`, "Link
error"); ADR-346, ADR-361, ADR-368, ADR-369, ADR-370

## Context

Design 8.11 has a link error read after the exchanges it may have come during settle them in a
`late_item`:

- one whose VCI time falls within an exchange is a fault seen in that exchange, or H3 evidence;
- it ends the matched answers from before it for every later exchange on the physical channel;
- one between exchanges settles those later exchanges alone;
- one whose time cannot be placed on one clock with the exchanges counts as a possible link error
  during every exchange whose entry was written since the link's items were last read before it.

The owed requests survive it. The exchange in flight already takes an unbound link error read
during it as its own, unless its time comes before the exchange's earliest placed VCI time. The
log wrote such an earlier one `unattached` and changed nothing. One bound to an earlier
primitive was `unattached` too.

The log counts the matched answers and the answers since an error as it takes exchanges in
(`LinkState::record`). A late error has to change those counts after the fact, as ADR-369 does
for a late reset.

## Decision

1. **Which items.** An unbound link error of no primitive that the exchange in flight does not
   take: its time comes before that exchange's span, or the exchange was refused. Also one of an
   earlier primitive, by its time; it keeps `primitive` `earlier`. One the exchange in flight
   takes whose time cannot be placed is also taken as a possible error of the exchanges before it
   (item 4).
2. **Where it falls.** On each target's link state, by its VCI time on one clock and epoch:
   - **within** an exchange, from its earliest placed VCI time (its transmit done or the start of
     its response) to its latest, so a time before its transmit done is between exchanges;
   - **after** the latest exchange shown to start before it;
   - **before the window**, within an owed exchange the log keeps beyond it or within none.

   An exchange whose times cannot be set against it is taken as after it. When no exchange the
   log keeps has a time on its clock and epoch, it cannot be placed (item 4). A time that
   steps back on its clock opens a new epoch (ADR-361), so an error read after later times on its
   clock is one that cannot be placed.
3. **Placed.**
   - The exchange it falls within takes it: on its own link, among its link errors (a fault seen
     in it, read after its answer, so it ends that answer); on another link of the channel,
     among its channel link errors (H3). It is classified again. One a final response settled
     keeps the most severe of its classification and the error's. An owed exchange beyond the
     window is classified again from what the log keeps of it.
   - The error is inserted into each target state's clears where it falls (`error_after`):
     - the exchanges after it count their matched answers and their answers since an error again
       from the window's exchanges after it alone, and are reclassified;
     - the link's own counts start again the same way;
     - an answer from before it counts for none of the exchanges after it, also one read later.
   - Before the window, the counts start again from the window's exchanges, and no window
     exchange is reclassified, since what they counted from before the window is not known.
     `not_reclassified_through` names the newest exchange it does not reclassify.
4. **Not placed.** It is a possible link error (H3) during each window exchange whose entry was
   written since the link's items were last read before it:
   - the last read is the latest read time of an item of its link before it, read with it or
     during an earlier exchange (`Inner::link_reads`);
   - an entry was written since when its exchange ended after that time.

   Each such exchange takes it among its channel link errors, and the counts start again before
   the first of them. When there is none, the error is no `late_item`: the exchange in flight
   took it, or it stays `unattached`.
5. **The entry.**
   - `refers_to`: the exchanges it came during or may have come during, then the later ones it
     reclassifies.
   - Its top-level classification: the most severe of the classifications it changes, or `none`
     when it changes none.
   - It refers to no exchange of its own when it falls between exchanges.
   - A placed error with no exchange before it on its clock and epoch is the first case of
     item 4: it settles none and stays `unattached`.

## Consequences

- An error bound to the stray primitive the host tracks is placed by its time here. The host
  does not yet pass that primitive's `seq` with it, and at job end it does not pass link errors
  of no primitive to the log. That work is left to a later change.
- An exchange with no VCI time on the error's clock is taken as after it, so it may lose matched
  answers from before an error that came after it. This errs towards `undetermined`.
- The set of exchanges an error that cannot be placed may have come during rests on the read
  times of items alone. A read that returned nothing is not seen, so the set can be larger than
  the reads would show.

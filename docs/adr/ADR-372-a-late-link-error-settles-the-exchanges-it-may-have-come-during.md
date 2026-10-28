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
   takes because its time cannot be set against that exchange's (on no clock, or on a clock and
   epoch none of the exchange's own times are on) is also taken as a possible error of the
   exchanges before it (item 4).
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
     - an answer from before it counts for none of the exchanges after it. The exchange it falls
       within or after may still get a late answer, whose time against the error is not known;
       that answer counts for the later exchanges, as it would have had the error been recorded
       in time.
   - Before the window, the counts start again from the window's exchanges, and no window
     exchange is reclassified, since what they counted from before the window is not known.
     `not_reclassified_through` names the newest exchange it does not reclassify.
4. **Not placed.** It is a possible link error (H3) during each window exchange whose entry was
   written since the link's items were last read before it:
   - the last read is the latest time the host read an item of its link before it, read with it
     or during an earlier exchange (`Inner::link_reads`): an unbound item, or one bound to a
     primitive (a pending, a transmit confirmation, a response, the end);
   - an entry was written since when its exchange ended at or after that time, since an entry is
     written after its end is read;
   - a refused exchange sent nothing a link error could hit, read nothing, and is passed over;
   - an error with no read time takes the latest read of its link.

   Each such exchange takes it among its channel link errors, and the counts start again before
   the first of them, also when the first is the window's oldest. When there is none, the error
   is no `late_item`: the exchange in flight took it, or it stays `unattached`.
5. **The entry.**
   - `refers_to`: the exchanges it came during or may have come during, then the later ones it
     reclassifies.
   - Its top-level classification: the most severe of the classifications it changes, or `none`
     when it changes none.
   - It refers to no exchange of its own when it falls between exchanges.
   - A placed error with no exchange before it on its clock and epoch is the first case of
     item 4: it settles none and stays `unattached`.

## Consequences

- An error bound to the stray primitive the host tracks, cancelled before the link's next
  request, goes to that primitive's exchange whatever its time: the host passes the exchange's
  `seq` with it (`UnattachedItem.owner`), and the log takes it as a fault seen in that exchange
  on its own link (the maintainer's decision, 2026-10-27). At job end, the host also reads the
  link errors of no primitive while it waits for the stray primitive, and passes them to the log
  after the primitive's `primitive_ended` (`CommLog::late_link_errors`), with the primitive's own
  when no end of it is written. They are placed in the order the host read them, every read at
  job end stands for the errors read after it (item 4). The primitive's exchange stands in for
  the exchange in flight for the errors read before its end was read: one at or after its
  earliest placed VCI time, or that cannot be set against its times on one clock and epoch, is
  its own (one that cannot be set against them also a possible link error during the exchanges
  written since the last read, item 4), and one before it is placed among the exchanges before
  it (item 2); one read after its end was read is placed by its time alone. Both came in a
  later change than the rest of this ADR.
- An exchange with no VCI time on the error's clock is taken as after it, so it may lose matched
  answers from before an error that came after it. A target whose exchanges share no clock with
  the error is taken the same way. Both err towards `undetermined`.
- The exchange in flight is not among the places an error it does not take can fall (one of an
  earlier primitive, or one read during a refused exchange): such an error is placed among the
  exchanges before it, and the exchange in flight takes no H3 evidence from it.
- The set of exchanges an error that cannot be placed may have come during rests on the read
  times of items alone. A read that returned nothing is not seen, so the set can be larger than
  the reads would show.

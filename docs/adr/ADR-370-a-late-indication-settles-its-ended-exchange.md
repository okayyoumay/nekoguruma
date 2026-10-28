# ADR-370: A Late Indication Settles Its Ended Exchange

**Date:** 2026-10-28
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `exchange_with`, `Inner::late_item`,
`Inner::late_item_unowned`, `LateItemEntry`, `UnattachedItem::late_indication`,
`before_starts`, `late_indication_owners`; `comm_log/exchange.rs`
`LinkState::late_transmit_owners`, `LinkState::late_start_owners`, `LinkState::update_kept`,
`LinkState::floor_of`, `Kept::start_in_time`, `settle_late_response`); design 8.11
("Settlement", `late_item`); ADR-346, ADR-365, ADR-367, ADR-368

## Context

Design 8.11 has an unbound transmit indication, loopback echo or start-of-message indication
read after its exchange's entry settle that exchange, in a `late_item`. The exchange is the one
ended exchange that could own the item (one whose `sent` is still `unknown`, or an owed request
that awaits a response on the identifier), provided no exchange in flight on the physical
channel uses the identifier. Two possible owners, ended or in flight, leave the item `unattached`
and each of them at least `undetermined`. The log wrote every such item `unattached`.

The host reads link items only while it waits for an exchange, so an unbound indication is
always read while some exchange X is in flight. Read literally, the rule would never let an
indication with X's identifier settle an earlier exchange.

The maintainer decided (2026-10-28):

- the item covers the indications, and the late pendings become an item of their own;
- "in flight" is read by VCI time, as for a link error read during an exchange (design 8.11
  "Link error");
- items bound to an earlier primitive keep the ADR-367 path;
- a start judged in time is remembered, so a late response completing it is `system`;
- each possible owner of an ambiguous indication is raised to at least `undetermined`;
- a new ADR records it.

A pre-review pass then showed two consequences of the first reading, and the maintainer decided
again (2026-10-28):

- only the exchange in flight's own transmit done bounds it, since a response item can carry a
  start with no transmit done, and the exchange's own transmit indication comes before that
  start;
- the exchange in flight is raised only when the indication could change it. On J2534 a
  transmit indication has no primitive and the exchange in flight usually has no transmit done
  of its own, so otherwise one exchange ending with its send unknown would lower every later
  exchange on the link.

## Decision

1. **Which items.** An unbound transmit indication or loopback echo, by its request identifier,
   and an unbound start-of-message indication, by its response identifier. An item bound to an
   earlier primitive keeps the ADR-367 path: a stray primitive's own items go to its
   `primitive_ended`, and items after its end stay `unattached`. An indication with no identifier
   names no exchange.
2. **In flight.** X was in flight with the identifier unless the item's VCI time comes before
   X's own transmit done, on one clock and epoch. That transmit done is bound to X's primitive or
   carried by its response. Nothing X sent or received comes before it: neither the start of
   X's response nor X's own transmit indication. X's unbound indications are not counted, since
   they are what is being attributed. An item at or after that time, or one that cannot be set
   against it, may be X's. A refused X awaits nothing and uses no identifier.
3. **Possible owners.** For a transmit indication or loopback echo, the kept exchanges on the
   link with that request identifier whose `sent` is still `unknown`. For a start-of-message
   indication, the owed requests that await a response on that identifier. The owners are
   complete only when the log keeps every such exchange. An exchange the log keeps nothing of may
   also own the item, so it makes the item ambiguous.
4. **One owner.** When exactly one ended exchange E could own the item, X could not, and the
   owners are complete, a `late_item` settles E. It is written before X's attachments, so X then
   takes its own indications as it would have.
   - A transmit indication or loopback echo gives E its transmit evidence and transmit done. E's
     `sent` is restated (`confirmed`), and it can own a late transmit indication no more.
   - A start-of-message indication starts E's response. An E the VCI ended with no response is
     `response_incomplete`. Its start is judged against E's limit in force like a late response
     (`judge_late`). A start before the limit less the margin is kept with E
     (`Kept::start_in_time`), and E is at least `undetermined` until the response completes. A
     late response that completes it is `system`, since the system missed a response that
     started in time.
   - E's timing is judged again with what the item adds, as its own entry would have judged it:
     a transmit done places its times (and is kept for a later late response), and a start
     refutes a timeout judged on no response having begun. E is classified again from its new
     evidence; the floor an ambiguous item set stands. A `system` E's own entry gave rested on
     its timing, so it is judged again too. The classification the `late_item` gives sets no
     floor of its own (`LinkState::settled_from_evidence`), so a later item can refute what it
     inferred: a start read after a transmit indication refutes the early timeout that
     indication let the log judge. E stays owed, since an indication is no final response.
   - The latest start is the response's: a later start replaces E's earlier one, as the ECU
     aborted and began again, and the in-time judgement is that of the start that stands.
   - A start in time makes a later late response `system` only when that response spans frames:
     one that fits one frame came after a transfer the ECU aborted.
   - The start counts as an answer for the later exchanges (`reclassify_after`), but not as a
     matched answer (ADR-354).
   - The entry gives the item as `unattached` gives it, with E's `sent`, `sent_evidence` and
     outcome. It names E and each later exchange reclassified, and `not_reclassified_through`
     as ADR-368 does.
5. **Two or more possible owners.** Ended owners, X, or an exchange the log keeps nothing of. The
   item stays `unattached`. A `late_item` names that line in `unattached` and raises each kept
   owner to at least `undetermined` as a floor, and those exchanges stay owed. An owner already
   raised for an ambiguous item, and no lower now, is not named again, and no line is written
   when none is left. X, when it is one of the owners, is at least `undetermined` in its own
   entry, with that floor in the window, but only when the item could change X: a transmit
   indication or loopback echo while X's `sent` stays `unknown`, a start while X has no answer
   of its own. Both reasons are kept when X may also own an ambiguous response.

## Consequences

- The late pendings, bound and unbound, are not settled yet; they are an open item.
- On J2534 a transmit indication is unbound, and an exchange's transmit done often comes only
  from that indication. Such an exchange has no own time to set an earlier indication against,
  so an earlier exchange's late indication read during it stays ambiguous.
- An exchange in flight gets the `undetermined` floor for an ambiguous indication only when the
  indication could change it, as item 5 says.
- A late start after the limit plus the margin is not recorded yet, so a response that later
  completes it is judged on its own time; it is an open item.
- The `late_item` lines come before the exchange's other items, so the log's lines are not in
  the order the host read the items.
- An indication read at job end keeps the ADR-366 path, which gives it only to the stray
  primitive's exchange.

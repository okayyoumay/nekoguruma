# ADR-453: The Items an Exchange Reads Are Offered in the Order Read, the Late Finals Among Them

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `CommLog::exchange_with`, `CommLog::primitive_ended`), design
8.11 `late_item`; `crates/agent/docs/ngr-agent.md`; ADR-370 item 4 and Consequences, ADR-447
decision 1, ADR-450 Consequences

## Context

An exchange's entry places the items of no primitive that the host read during it. It did so in
two passes. The first gave every indication and forwarded pending to the one ended exchange that
could own it, before the exchange in flight took its own indications (ADR-370 item 4). The second
went through what was left in the order the host read it, the rejected wrong echoes with it, and
placed the late final answers there (ADR-447 decision 1). `primitive_ended` did the same with the
items read while its primitive was in flight.

An indication or pending was therefore offered against the kept records as they stood before
every late final of the same read, also one read before it. Since ADR-450 and ADR-452, what such
an item does depends on its owner's evidence: a record whose evidence settles its exchange, kept
only for its rows, takes no lone pending or start, and a transmit indication only confirms its
send. A late final that leaves such a record changed what the item read after it did only when
the two were read during two exchanges. Read during one, the record still took the pending or
start as its `late_item`, and a transmit indication classified it again (ADR-450 Consequences).
The paths that place items with no entry in flight (`late_link_errors`, the items read at or
after a primitive's end) already took one item after the other.

The maintainer decided on 2026-11-10: unbound items are offered to their one owner in the order
read together with the late finals, as ADR-447's pass places the finals, and settling first
(ADR-370) stays the rule for each item within that order.

## Decision

1. **One pass, in the order read.** `exchange_with` goes once through the unbound items and the
   rejected wrong echoes, by the place the host read each at (ADR-447 decision 1: an item with no
   known read comes last, in the order listed). Each is judged against the kept records, the owed
   rows and the unknown sends as the items read before it left them.
2. **For each item, the one ended owner first.** An indication or forwarded pending that one
   ended exchange alone could own, and the exchange in flight could not, is that exchange's
   `late_item` (ADR-370 item 4), with the gates of ADR-450 and ADR-452 read at that point. When
   none takes it, the exchange in flight takes it as its own if no ended exchange could own it
   then: a transmit indication with its request identifier while no ended exchange's send is
   unknown, a start-of-message indication with its response identifier while no owed request
   awaits a response on it. Anything else is placed where it was read: a response one owed
   exchange alone could own is its `late_response`, and every other item is `unattached`,
   followed by the line that names and floors the exchanges that could share it. A rejected
   wrong echo is offered as ADR-447 says.
3. **The exchange in flight decides on its own indications per item.** It took them after the
   whole first pass and before any late final. It now takes the first transmit indication that
   no ended exchange could own when it was read, and each start that no owed request could own
   when it was read; the latest start it took is its response's start under the conditions that
   held before. A decision per batch would either let it take an indication read before the
   final that freed its identifier, or keep it from its own indication read after an ended
   exchange's.
4. **What stays where it was.** The items bound to the exchange's primitive are its entry's.
   Whether the exchange was in flight for an item (ADR-370 item 2) is read from its own transmit
   done, bound to it or carried by its response, and not from the indications the pass
   attributes. A link error takes no owner: it is set against the exchange's span, which the
   pass completes, so the link errors are taken or written as `late_item` after the pass, and
   one that writes no line is `unattached` after them.
5. **`primitive_ended` follows the same order** for the items read while its primitive was in
   flight: each is offered to the one other exchange that alone could own it, and placed when
   none takes it, before the next. The indications the entry takes as its own are chosen as
   before.
6. **Scope.** Who could own an item, the gates of ADR-450 and ADR-452, the in-flight rule and
   every text are unchanged.

## Consequences

- No log format change. The lines an exchange's reads write are in the order read, except that
  the link errors follow them and the exchange's own indications write no line.
- An item is judged as it would be had the items read before it come during an earlier
  exchange:
  - a pending or start read after the final that leaves its one owner kept only for its rows is
    `unattached` and the record stands (ADR-450);
  - a transmit indication read after that final confirms the record's send only (ADR-452), so
    ADR-452's cost holds within one read too: the final is judged without the transmit time;
  - a start or transmit indication read after the final that settles the last ended exchange
    that could own it is the exchange in flight's own. It was `unattached`, or that ended
    exchange's `late_item` when its VCI time came before the exchange in flight's own transmit
    done;
  - an indication that an ended exchange and the exchange in flight could own when it was read
    stays ambiguous though an indication read later is the ended exchange's alone, where the
    first pass settled the later one first and the exchange in flight then took the earlier
    one. This needs the one read first to be not shown before the exchange in flight's own
    transmit done and the one read after it to be shown before it, which a worker that
    delivers in order and names its clock does not produce;
  - a start two exchanges could own and a start one alone owns act on the start kept with the
    exchange (ADR-384 item 3) in the order read.
- A late final and a late link error of one read are written final first, whichever was read
  first, where the link error came first. The counts go by the stored times (ADR-444, ADR-449);
  where those ADRs leave an outcome to which of the two is written first, the final now is.
- A link error read before the late final of a record kept beyond the window, in the same read,
  no longer reaches that record: the final settles and releases it in the pass, so the error
  placed after the pass finds no kept record at its time and names no exchange, and the
  record's settled classification leaves out the fault. Read across two reads, error first, the
  error reaches the record. Before this decision it was the other order that differed (a final
  read before the error); neither order is independent of the batching for a link error yet,
  which is in the backlog. In the window the record is still found and only the reason can
  differ.
- Residual, in the backlog: the entry of a primitive's end still chooses its own indications
  before the reads of its wait are placed; and the exchange in flight takes a transmit
  indication or start that no ended exchange could own also when its VCI time is shown before
  the exchange's own transmit done, as it did across reads.
- Closes ADR-450's Consequences bullet on the order of the offers.
- Revises ADR-370 item 4 (the `late_item` is written before the attachments of the exchange in
  flight: per item, in the order read) and its Consequences bullet on the order of the lines,
  and ADR-447 decision 1 (the pass also gives each indication and pending to its one owner and
  the exchange in flight its own).
- Tests: the final and the pending or start in one batch and in two, both orders; the order
  read against the order listed; a transmit indication after the final; the items of a
  primitive's wait; the exchange in flight's own start and transmit indication after the final
  that frees them, and a start read before it; two owners at the read; a link error beside the
  final.

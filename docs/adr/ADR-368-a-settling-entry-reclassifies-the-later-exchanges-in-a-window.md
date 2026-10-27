# ADR-368: A Settling Entry Reclassifies the Later Exchanges in a Window

**Date:** 2026-10-27
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `exchange_with`, `late_response_owner`,
`LateOwner`, `Inner::late_response`, `Inner::late_response_unowned`, `CommLog::primitive_ended`,
`LateResponseEntry`, `PrimitiveEndedEntry`; `comm_log/exchange.rs` `LinkState::log`,
`LinkState::reclassify_after`, `LinkState::settled`, `ExchangeEntry::settled_answer`, `Clears`,
`WINDOW`); design 8.11 ("Settlement", `late_response`); ADR-346, ADR-365, ADR-366

## Context

Design 8.11 has a settling entry that changes the evidence another exchange's classification
rests on also refer to that exchange and reclassify it:

- an answer it adds to an earlier exchange is a matched answer for the later exchanges on the
  link with the same match;
- a final response ends the earlier exchange as an owed request for them.

An exchange's standing classification is then the latest entry's for it. The design also has a
response that two or more exchanges could own leave each of them at least `undetermined`.

ADR-365 kept only the owed exchanges a late response can settle (item 1). It left every
ambiguous response `unattached`, settling nothing (item 2). Keeping every logged exchange until
an accepted reset or session change has no bound within a job. The maintainer decided the
retention (2026-10-27): the last 16 exchanges per link.

## Decision

1. **A window per link.** Besides the owed exchanges ADR-365 keeps, the log keeps the last 16
   logged exchanges of each link (`exchange::WINDOW`). The maintainer chose, at the design
   summary, every logged exchange, not only the answered ones. A silent exchange's
   classification also rests on the matched answers: with no response and the send confirmed,
   one counted makes it `vehicle`. For each, the window holds:
   - its evidence and the history it was classified with;
   - its origin and request;
   - its standing classification and reason;
   - the link's count of ends of its matched answers, and of its answers since a link error,
     when its history was taken (`Clears`).

   The oldest exchange leaves a full window. An accepted ECUReset or session change empties
   the window, as the maintainer decided at the design summary: it ends the matched answers and
   the owed requests from before it. So no later response can settle an exchange from before
   it, and nothing from before it can change a classification after it.
2. **What a settling answer gives the later exchanges.** When a `late_response` or a
   `primitive_ended` gives an earlier exchange its end, the log reads the answer it adds as
   `LinkState::record` reads an exchange's answer (`ExchangeEntry::settled_answer`). For each
   later exchange in the window:
   - **A first answer** counts one more answered exchange. It also counts one more since the
     last link error, unless a link error came between the two exchanges.
   - **A matched answer its entry did not count** (`Kept::matched`; a pending an owed request
     could own was not one) counts for a later exchange with the same match, unless the link's
     matched answers ended between the two exchanges (a link error, a reopened link, an
     accepted or unusable reset). An answer freed this way keeps its order against its own
     exchange's link errors: one that came after it, or in no known order, still ends it.
   - **A final response** ends the earlier exchange as an owed request the later answers could
     belong to.

   The later exchange is classified again from its own evidence with that history. Only one
   whose classification or reason changes is reclassified.

   The answer also counts for the exchanges logged after the settling entry, as `record`
   would have counted it in time: in the link's answered counts, and as a matched answer
   unless the matched answers ended since. A later exchange whose answer no owed request can
   own any more is counted as `record` counts one (design 8.11 "Matched answer", "Owed
   request"):
   - its answer, a pending too, is a matched answer for the exchanges after it, in the window
     and logged later;
   - a final response places it alone: it is owed no more, what the log kept of it is
     released, and it is no owner of theirs any more. An exchange with only a pending stays
     owed.
3. **The settling entry names them.**
   - `refers_to` lists the settled exchange, then each reclassified one, in `seq` order.
   - `exchanges` gives each its new classification, with a reason that names the exchange whose
     answer changed it.
   - The top-level `classification` is the most severe of them (design 8.11 "Settlement").
   - When a later exchange had already left the window, `not_reclassified_through` gives the
     largest such `seq`, and `text` says those exchanges are not reclassified.
   - Each reclassified exchange's standing classification is the entry's, also for an owed one
     the log keeps.
4. **An ambiguous late response.** ADR-365 item 2 and its Consequences left a response that two
   or more exchanges could own `unattached`, settling nothing. Such a response is now still the
   `unattached` event, and a `late_response` follows it:
   - `unattached` gives the event's `seq`, and no owner is chosen;
   - `refers_to` lists each kept owed exchange that could own the response;
   - each becomes at least `undetermined`, and stays owed.

   A response is ambiguous when two or more owed exchanges could own it, or one could and the
   exchange in flight awaits the same response. The exchange in flight is then at least
   `undetermined` in its own entry, written after the settling entry. Its window entry keeps
   that floor, so a later reclassification from its history does not lower it.
5. **An exchange a settling entry settled.**
   - Given a final response by a `late_response` or `primitive_ended`, it is not reclassified
     from its history afterwards: its classification rests on evidence its history alone does
     not give, and it is owed no more. Its window entry takes that evidence and its owners, so
     its answer is still freed for the exchanges after it once none of them can own it.
   - Given any other end by a `primitive_ended`, it stays owed. It is still reclassified, from
     its evidence with that end, keeping at least the classification the entry gave it.
   - Raised by an ambiguous response, it stays owed. It is still reclassified, keeping at
     least `undetermined`, so a later answer of its owner still frees its answer.

   An answer counts as a matched answer once (`Kept::matched`), however it became one. One that
   no owed request can own any more also confirms its exchange's send, as `sent_of` reads it, so
   that exchange can own a late transmit indication no more.
6. **A link error a `primitive_ended` carries** (its end, or one read beside it) has no known
   order with an answer read with it. That answer is no matched answer, and the error ends the
   matched answers of every target on the channel, as an error an exchange saw does.

## Consequences

- ADR-365 item 1's retention and item 2's ambiguous response are revised. Its Consequences'
  notes on matched answers, owed requests and ambiguous owners are now done.
- The window keeps 16 copies of evidence per link, besides the owed exchanges.
- A matched answer rarely counts for a later exchange in practice. A later exchange with the
  same match is usually itself owed, so the answer could be its own and is no matched answer
  (`LinkState::record`). The final response ending an owed request is the common case.
- The settling entry of an ambiguous response refers only to the ended owners the log keeps.
  An owed exchange it does not keep, such as one that ended with a link error, is not named.
- A late positive response to an ECUReset or a session change is not taken as accepting it:
  it is no matched answer, but the owed requests and matched answers from before it stay.
  Whether a late acceptance ends them is left open.
- An ambiguous late response adds nothing to the link's answered counts, since none of its
  possible owners is chosen.
- On the agent's link a `primitive_ended` is written before the link's next exchange, so it
  finds no later exchange in the window yet; it reclassifies the same way once one can.
- A `late_response` judges the settled exchange against the link's history as of the settling
  entry (ADR-365 item 4), while the later exchanges keep the history of their own time.
- A `late_item` that adds an answer will reclassify the later exchanges the same way when it is
  built.

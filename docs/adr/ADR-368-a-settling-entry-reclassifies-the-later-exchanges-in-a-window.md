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
   - **A matched answer it did not hold before** counts for a later exchange with the same
     match, unless the link's matched answers ended between the two exchanges (a link error,
     a reopened link, an accepted or unusable reset).
   - **A final response** ends the earlier exchange as an owed request the later answers could
     belong to.

   The later exchange is classified again from its own evidence with that history. Only one
   whose classification or reason changes is reclassified.
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
   exchange in flight awaits the same response.
5. **An exchange settled by its own entry** is not reclassified from its history afterwards.
   This covers the owner of a `late_response` or `primitive_ended`, and an owner an ambiguous
   response raised. Its classification then rests on evidence its history alone does not
   give.

## Consequences

- ADR-365 item 1's retention and item 2's ambiguous response are revised. Its Consequences'
  notes on matched answers, owed requests and ambiguous owners are now done.
- The window keeps 16 copies of evidence per link, besides the owed exchanges.
- A matched answer rarely counts for a later exchange in practice. A later exchange with the
  same match is usually itself owed, so the answer could be its own and is no matched answer
  (`LinkState::record`). The final response ending an owed request is the common case.
- The exchange in flight, as a possible owner of an ambiguous response, is classified by its
  own entry; the settling entry refers only to the ended owners.
- A `late_response` judges the settled exchange against the link's history as of the settling
  entry (ADR-365 item 4), while the later exchanges keep the history of their own time.
- A `late_item` that adds an answer will reclassify the later exchanges the same way when it is
  built.

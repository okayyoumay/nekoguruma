# ADR-369: A Late Acceptance of a Reset Ends the Evidence as in Time

**Date:** 2026-10-27
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `Inner::late_response`,
`CommLog::primitive_ended`, `settle_later`; `comm_log/exchange.rs` `LinkState::reset_after`,
`LinkState::reclassify_one`, `ExchangeEntry::late_restart`, `Logged::matched_at`); design 8.11
("Matched answer", "Owed request", "Settlement"); ADR-346, ADR-365, ADR-366, ADR-368

## Context

Design 8.11 ends the evidence from before an accepted DiagnosticSessionControl (0x10) or
ECUReset (0x11) of any type (ADR-346 item 6):

- the matched answers end;
- the owed requests sent before the earliest request its answer could belong to end.

An unusable positive response to either ends the matched answers only, since the ECU may be
restarting but the agent cannot tell. `LinkState::record` applies both when the exchange's entry
is written, and ADR-368 empties the reclassification window at an accepted one.

A request that timed out can still be answered later. A `late_response` (ADR-365), or the end of
a primitive the agent stopped waiting for (`primitive_ended`, ADR-366, ADR-367), then adds the
answer. ADR-368 counted such an answer for the later exchanges, but did not apply an
acceptance; its Consequences left that open. The exchanges logged between the reset's entry and
the late answer kept the matched answers and owners from before the reset. An owed request from
before it could still be settled and reclassify them.

The maintainer decided (2026-10-27):

- a late acceptance is applied as one accepted in time;
- the exchanges logged in between are reclassified without the matched answers and owners
  from before it;
- a late unusable response ends the matched answers only, as in time;
- both settling entries apply it;
- when the reset exchange has left the window, the exchanges after it still in the window count
  their matched answers from the window alone, and those that left are named;
- a new ADR records it.

## Decision

1. **Which answers.** A settling entry's answer applies a reset when the exchange's request is a
   session change or an ECUReset and the answer is either:
   - a positive final response, which accepts it;
   - a positive response the agent cannot use (`positive_unusable`).

   `ExchangeEntry::late_restart` gives this, with the earliest request the answer could belong
   to: the exchange itself, or an earlier owed request that could also own the answer.
2. **When.** The reset is applied as of the exchange's entry (`LinkState::reset_after`), before
   the answer itself is counted for the later exchanges (ADR-368).
3. **The matched answers.** An end of the matched answers is inserted at the exchange's entry:
   - the link's `Clears::matched`, and that of every later exchange in the window and of every
     owed exchange after it, goes up by one;
   - each logged exchange records the value its answer counted as a matched answer in
     (`matched_at`).

   The later exchanges then count their matched answers again from the window, in `seq` order.
   Each counts the earlier exchanges whose answer counted under its own value, and so do the
   link's counts for the exchanges to come. The answered counts stand, since the answers were
   given.
4. **The owed requests**, for an acceptance only:
   - the owed requests sent before the earliest request end, as in time, with what is kept of
     them;
   - they are no owners of the later exchanges' answers. A later exchange whose answer is then
     placed alone is owed no more, and is no owner of the exchanges after it;
   - the window keeps the reset exchange and the ones after it.

   Owed requests sent after the reset stay owed.
5. **Reclassification.** Each later exchange in the window is classified again with its new
   history by the same per-exchange rule `reclassify_after` uses (`reclassify_one`: floor,
   freed answer, confirmed send, placed alone). Each whose classification or reason changes is
   named in the settling entry, as ADR-368 does. The entry's text says the response accepts, or
   is an unusable answer to, the exchange's session change or ECUReset.
6. **Past the window.** When the reset exchange has left the window (it is still kept while
   owed), the exchanges after it that are still in the window count only the matched answers of
   the window's exchanges. This undercount can only make a silent read less sure. The latest
   later exchange that left is named by `not_reclassified_through`.

## Consequences

- ADR-368's open note on a late acceptance is closed.
- The window now records each logged exchange's matched value (`matched_at`). The reset's
  recount and `reclassify_after` share `reclassify_one`.
- A reset whose exchange has left the window drops the matched answers of the later exchanges
  that left it too, for the exchanges to come. That leaves them `undetermined` where the in-time
  rule could have given `vehicle`.
- An exchange in flight when the late answer is read takes the new state, since the log writes
  the settling entry before that exchange's history is taken.

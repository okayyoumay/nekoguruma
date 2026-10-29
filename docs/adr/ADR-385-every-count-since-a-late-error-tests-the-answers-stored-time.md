# ADR-385: Every Count Since a Late Error Tests the Answer's Stored Time

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/exchange.rs` `Kept::answer_at`,
`Logged::answer_at`, `Kept::answer_time`, `LinkState::log`, `LinkState::judge_answer`,
`LinkState::since_late_error`, `LinkState::error_after`, `LinkState::reclassify_one`,
`LinkState::take_answer`; `comm_log.rs` `CommLog::exchange_with`,
`CommLog::primitive_ended`, `Inner::late_response`, `Inner::late_item`); design 8.11
"Settlement", `late_item`; ADR-372 items 2 and 3; ADR-384 item 2 and Consequences

## Context

ADR-384 narrowed the settling entries a late link error is read before: an answer they settle
counts as a matched answer, or as one since the error, only when its time is shown after the
error's. It left the recount that the error itself starts on positions (its Consequences):
the window's exchanges after the error's placement counted their answers again, whatever the
answers' times. So an answer settled before the error was read, and shown before it on the same
clock and epoch, kept counting; an owed exchange with no time at its entry was taken as after
the error by position (ADR-372 item 2); and an answer freed later through its owners counted
too. A later silence could stay `vehicle` on evidence that showed the opposite.

ADR-384 also marked an answer judged "answered only" with a stored flag, so that a later path
would not offer it again. That flag was a second record of a fact the times already decide, and
the paths that offered an answer again were where its findings kept coming from.

## Decision

1. **Every exchange keeps its answering item's time.** At its entry, the time of its own
   answering item (its final response, else its latest pending, else its start alone), picked
   as ADR-384 item 2 picks it; none when it has no answer of its own. Each settling entry
   replaces it with the time of the item it settles, when that item answers. The time is kept
   with the window's entry and with the owed exchange beyond the window.

2. **One test decides every count since a late error.** An answer counts as a matched answer,
   and as one since a late link error, only when no late error applies to its exchange or its
   stored time is shown after the error's on one clock and epoch (ADR-384 item 2). The settling
   entries, the recount the error starts and an answer freed through its owners all use that
   test. Once an answer fails it, later errors cannot make it pass, so no flag records it; a new
   answering item of the exchange is tested by its own time.

3. **An in-time answer with no comparable time does not count since a later error.** An answer
   in its exchange's own entry, with no time on the error's clock and epoch, logged before the
   error was read, is not shown after it, and stops counting since it in the recount. The
   maintainer chose this (2026-10-29). Only an exchange logged after the error was read counts
   an answer with no comparable time.

4. **The stored time does not place a later error.** A late error is placed by the entries'
   own times, as before. An answer settled late can come after later exchanges began, so
   letting it stretch its exchange's span would break the order the placement rests on: an error
   a later exchange may have held would be taken as certainly within the earlier one. The
   maintainer first chose to place by the stored time and, after review found this, chose not to
   (2026-10-29).

5. **An answer the recount leaves out is no longer counted.** When a late error's recount leaves
   an owed exchange's matched answer out, the exchange has no matched answer counted any more.
   A later answering item of its own is then a new matched answer and is tested by its own
   time; without this, a later final response shown after the error would have been passed
   over as already counted.

6. **The kept answer's time is its final response's.** An owed exchange that keeps a final
   response another request may own takes that response's time as its answer's, not its latest
   pending's, as ADR-384 item 2 orders the answering items. The maintainer confirmed this
   (2026-10-29).

## Consequences

- No count since a late error rests on an exchange's position any more, in a settling entry or
  in the recount. More exchanges settle as `undetermined` after a late error on links whose
  items share no clock with it.
- The positional counters stay for what they decide without times: in-time errors, the
  exchange an error falls within, an accepted reset between exchanges, and a reopen.
- The recount still counts from the window alone (ADR-372 item 3, ADR-384 item 4): an answer of
  an exchange that has left the window, or of an owed exchange kept beyond it, counts for no
  window exchange. That can only count too few.
- One field per exchange replaces the flag ADR-384 kept, and the answering item's time is
  picked in one place.
- Because the stored time does not place an error, an error after an exchange's late answer
  can still fall after that exchange's entry times and recount the exchange's own history,
  and the entry may say the error fell between exchanges. That only counts too few.

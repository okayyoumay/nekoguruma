# ADR-449: A Matched Answer Counted After Its End's Link Error Counts Only for the Exchanges Logged After It

**Date:** 2026-11-10
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/exchange.rs` `MatchedAt`, `Logged::matched_at`,
`Kept::matched_through`, `SettledAnswer::after_end_error`, `LinkState::reset_after`,
`LinkState::error_after`, `LinkState::reclassify_after`, `LinkState::kept_matched_after`;
`comm_log.rs` `CommLog::primitive_ended`); design 8.11 "Settlement"; ADR-369 decision 3,
ADR-387, ADR-441 decision 7, ADR-444

## Context

ADR-369 decision 3 counts a matched answer for a later exchange when the epoch the answer
counted in equals the epoch the later exchange's history was taken in. Every end of the matched
answers, read in time or late, adds one to everything placed after it, so two points have the
same epoch exactly when no end lies between them. The late shifts (`reset_after`, `error_after`,
ADR-444) decide by `seq` and treat the answer as sitting at its exchange's entry.

One writer puts the answer elsewhere. When a primitive's end reads a link error before the
answer, `CommLog::primitive_ended` applies the error to the link and counts the answer at the
link's clears after it, past every exchange logged since the entry. For an exchange L logged
between that entry and the end, the answer's epoch is then greater than L's by the in-time ends
between them. Each late event that moves L and leaves the answer closes the gap by one, and at
zero the two meet with an end between them, so L counts an answer it cannot have. The events are
a late link error that leaves the answer out (ADR-385 decisions 2 and 5; ADR-444 for the exchange
at the error's place), one placed after a later exchange, a late restart between the two, a reopen
followed by late errors, and, for an exchange that has left the window, the kept record, whose
`Kept::matched` ADR-441 decision 7 reads as an answer counted in the epoch of the record's own
`Kept::clears`, which is still the entry's. ADR-444's Consequences recorded one such case, which
depends on the order the error and a late restart are read in.

The failing order is not reachable from the host today: the host cancels a stray primitive before
the link's next request and writes its `primitive_ended` before that request's entry (ADR-445,
last paragraph of Consequences), so it logs no exchange between an entry and its primitive's end.
It is reachable through `CommLog`'s API, and the log's counts should not depend on that.

## Decision

1. A counted matched answer keeps, with its epoch, the line through which no exchange counts it
   (`MatchedAt`): its own exchange's line, or, for an answer its primitive's end counted after a
   link error read with that end, the line that error was written as.
2. One predicate decides whether it counts for an exchange: the exchange is logged after that
   line and its history was taken in the answer's epoch. The settlement of a later exchange, the
   window's recount after a late restart or link error, and the records beyond the window use it,
   in place of the equality they used. The link's count after a late restart or link error
   tests only the epoch against the link's live one, which is equivalent: the link's next
   exchange comes after every line an answer counts from.
3. The record keeps the same line beside `Kept::matched` (`Kept::matched_through`), set when the
   record's answer is counted at the end and cleared when its matched answer ends or is counted
   again at the entry.
4. The shifts, the stored-time test, the way a matched answer ends, `Kept::clears`,
   `Logged::after` and ADR-444's bound on the exchange at the error's place are unchanged. The
   predicate only adds a condition, so it can remove a count and never add one.

Invariant: for window entries E and L with L logged after E's line, E's epoch never exceeds L's
`clears.matched`, and no late event lowers the difference: `reset_after` moves E's epoch only
when E is after the reset, and then L moves too; `error_after` moves it only when E is at or
after the error's place, and then L moves too. An equality therefore means one when the later of
the two was written and no end lies between them since. The pairs left out are those the link
error read with the end lies between (design 8.11 "Matched answer", since the last link error).

## Consequences

- The counts of an exchange logged between a primitive's entry and its end no longer depend on
  the order of the end, a late link error and a late restart, nor on whether the answering
  exchange has left the window, for an answer the end itself counted (no owner standing at the
  end). ADR-444's open read-order case is closed for it.
- Still read-order dependent: an end with an owner standing counts nothing and stores no line;
  a late accepted restart that drops the owner then frees the answer from the exchange's own
  line in the pre-error epoch, so an exchange logged between the entry and the end counts it and
  one logged after the end does not, or the reverse if the acceptance is read first.
- An answer counted at the exchange's entry (a forwarded pending) still counts for the exchanges
  logged after it.
- Still counted too few, as before: an exchange logged after the end when a late link error is
  placed after a later exchange or a late restart after the answering exchange, read after the
  exchange was logged; the kept record beyond the window for an exchange logged after the end,
  whose epoch is the entry's.
- No written classification changes in the host's flow, which logs no exchange between an
  entry and its primitive's end. Through the log's API the count can still change the reason's
  H2 reading (it drops or adds H2 on a zero count), and a reclassification line is then
  written when only the reason changes.

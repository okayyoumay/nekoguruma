# ADR-439: The Rows of an Owed Exchange Beyond the Window Follow Its Owners

**Date:** 2026-11-08
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `LinkState::refresh_kept_beyond_window`,
`LinkState::reclassify_after`, `LinkState::reset_after`, `LinkState::refresh_owed_rows`),
design 8.11 "Settlement", "Owed request"; ADR-435 decision 6 and first Consequences bullet,
ADR-368

## Context

ADR-435 decision 6 re-derives the rows of an exchange that stays owed whenever a settlement
frees one of its answers, but only for the exchanges in the link's window, because only they
are reclassified (ADR-368). An owed exchange that has left the window keeps its record
(`Kept`) and its rows, and a row it holds only because an earlier owner could own its final
answer there stayed after that owner let the answer go: the stale row made the exchange an
owner of a late final answer on that identifier, which the exchange that already held its
final answer there then wrongly took (or, with another owner, left `unattached` and both
`undetermined`), against the rule that the log never decides between possible owners. The
in-window path gave the opposite outcome for the same facts.

## Decision

1. **Rows follow owners for every kept exchange.** After a settlement frees an answer
   (`reclassify_after`, for each answer it takes, including the ones freed in turn) and after
   an accepted restart ends the owed requests before its earliest request (`reset_after`),
   `LinkState::refresh_kept_beyond_window` visits each kept exchange not in the window and
   later than the settled exchange, in `seq` order, and takes its rows again from its answers'
   owners as they are in the live rows (`refresh_owed_rows`). The rows only lose identifiers,
   as in ADR-435 decision 7, so the passes end.
2. **A visit takes the step the window's reclassification takes, from one shared helper
   (`LinkState::free_owner_answer`, also used by `reclassify_one`).** An answer no owed
   request can own confirms the exchange's send; a final response that only the owners that are
   gone could own is placed alone, so the exchange is released with its record (a link with no
   receive table, which owes the exchange on its own identifier whatever its owners, is
   released this way and not by a dropped row); otherwise the rows are taken again and an
   exchange left with none is released too. The visit frees its answer for the later exchanges
   when it releases the exchange, drops an identifier (`settled_ids`) or turns the answer into
   a matched one, counted once (`Kept::matched`) and only when no late link error may have
   come after it. Beyond the window only the clears after the entry are kept, so an answer a
   link error of its own exchange followed is not counted as a matched answer there (in the
   window it is counted with the clears before the entry). The answer goes through the same
   worklist, so a chain of kept exchanges, and the window's exchanges after them, lose the
   owner in one pass.
3. **Classification beyond the window is not re-derived.** ADR-368's window scope stands: the
   entry's history is not kept, so the visit reads the owners from the live rows
   (`LinkState::history_settling`, keeping only the requests sent before the exchange: that
   method also lists later owed requests, which in the window never mattered and beyond it kept
   an exchange from being released), and the entry it builds takes the kept classification,
   floor included, in place of one derived from live counts. What the visit changes is the
   rows and the kept evidence, the release of an exchange placed alone (its record and
   `unknown_sends`), `Kept::sent` once an answer confirms the send, and `Kept::matched` when its
   answer turns into a counted matched answer. A matched answer that cannot count (a late link
   error may have come after it, or its own exchange saw one after it) is not offered to the
   later exchanges, since it changes nothing for them and would be offered again at every
   visit. After an accepted restart the visit changes rows and records only: the restart
   recounts the matched answers from the window alone, so a matched answer is neither counted
   nor noted in the record, and the exchange's later final response still counts as one. Rows
   that need no owner stay: an identifier a pending is open on (ADR-436) and the whole table
   of an exchange that ended on rejected results (ADR-437).

## Consequences

- ADR-435's first Consequences bullet (an exchange beyond the window keeping a stale row until
  its own end or an accepted restart) is closed, and decision 6's window scope is widened to
  kept exchanges.
- An accepting restart (`LinkState::record`, and `reset_after` for exchanges between the
  earliest request and the reset) still drops owners without refreshing the rows of the
  surviving exchanges, in or out of the window; this is not changed here.
- An owed exchange beyond the window with no kept record (a non-collecting exchange with a
  final response whose answer had an owner is owed on its identifier but not kept) is never
  visited and its evidence is gone, so its stale row still counts among a late response's
  owners (`late_response_owner`) and can make a single kept owner's late final answer
  ambiguous.

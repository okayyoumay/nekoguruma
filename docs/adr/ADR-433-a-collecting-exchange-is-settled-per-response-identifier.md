# ADR-433: A Collecting Exchange Is Settled per Response Identifier

**Date:** 2026-11-08
**Status:** Accepted (decision C built: the covering fold of an unnamed pending)
**Affects:** `agent` (`comm_log.rs` `late_response`, `late_item`, `primitive_ended`;
`comm_log/exchange.rs` `Kept::answered_placed`, `LinkState::settle_on`, `LinkState::reclassify_after`,
`SettledAnswer`, `settle_late_response`, `Evidence::latest_answer_on`); design 8.11 "Owed
request", `late_response`, `late_item`, `primitive_ended`; ADR-429 decision 3, ADR-432
Consequences, ADR-426

## Context

ADR-429 owes a collecting exchange once per response identifier with an open pending, and
decision 3 left settling per identifier open: the first late final answer on any of them removed
every row and what the log kept of the exchange. The ECUs that answer later on the other
identifiers then had no owner, so their final answers were `unattached`, and the later exchanges'
answers on those identifiers lost their owner at once instead of when their own identifier was
settled. The same exchange-wide state hid a late start: `late_item` read whether the response
spans frames from the exchange's end, which another identifier's short final answer can be, so a
start on an identifier still open was neither kept nor judged, and its later multi-frame final
answer missed the in-time `system`.

## Decision

1. **A late final answer settles the exchange on the identifier it names, and the exchange is
   settled whole only when none is left.** `late_response` restates the answer as
   `Kept::answered` does (ADR-425 decision 4, ADR-429), and sets the owed rows again from the
   restated evidence's `owed_ids` (`LinkState::settle_on`, through the one writer of the rows).
   The exchange is settled whole (`settle_owed`, the entry's `settled` and the answer's
   `final_response`) only when the restated evidence settles it (`settles_owed`), or when no
   identifier is left to owe it on. Otherwise it stays owed and kept, with the restated evidence
   and the places the host read its answers in and where each ended (`answer_order`,
   `answer_ends`, which the fold keeps in step with the folded answers), so a later late item is
   judged in its own identifier's chain.
2. **One `late_response` per identifier's final answer**, each referring to the same exchange.
   The verdict is judged from the window of the identifier the answer came on (ADR-426), as
   before.
3. **The standing classification is the most severe of the per-identifier verdicts so far,
   with the `system` the exchange's entry observed.** A partial settlement keeps its verdict as
   the exchange's floor, with a reason suffix, and `settle_late_response` raises a later
   verdict to it. The verdict is kept apart from the floors ambiguous items set (`Kept::partial`,
   not `Kept::floor`), and it does not take the exchange's classification wholesale, which an
   entry's `undetermined` would then lift for a final answer that is the vehicle's. An ambiguity
   raise (`LinkState::raise`) keeps the higher of the floor it finds and its own, so a partial
   settlement's `system` survives it and the rebuilds that follow.
4. **The later exchanges' answers are freed per identifier.** `SettledAnswer` carries the
   identifiers the exchange is owed on no more (`settled_ids`); `reclassify_after` drops the
   exchange from a later exchange's owners only when the rows it has now no longer list it for
   that exchange's answers. `primitive_ended` passes the identifiers its renewal dropped, which
   also closes the stale owner a renewed exchange left behind.
5. **A late start is judged by its own identifier's response state.** `late_item` reads
   whether the response spans frames, and whether to judge a start, from the latest answer on
   the identifier the start came on (`Evidence::latest_answer_on`): none or a pending is still
   to answer, a final answer decides by its length. `primitive_ended`'s own check for the
   entry's end is not changed.
6. **An unnamed late final answer with two or more identifiers open settles none.** It is
   appended as an answer, judged against each open window (verdicts that differ leave the
   window open), the rows are unchanged, and the named final answers that follow settle them as
   above. With one identifier open it folds that identifier's pending, as before.

## Consequences

- ADR-429 decision 3's "settles the exchange, removing all its rows" is superseded by decision 1
  above; its owner lookups, the shared settling condition and the renewal at `primitive_ended`
  stand.
- ADR-432 Consequence 1's whole settlement by a final answer on a table identifier holds for an
  exchange owed on one identifier; with several open it is per identifier. Its deferred bullet on
  settling per identifier is closed. Its bullet that an identifier which stays silent beside
  another's final answer is not owed stands.
- A reset or session change accepted later ends every row and kept exchange as before, and the
  host's release after a `primitive_ended` keeps an exchange the VCI ended its wait for.
- Built: an unnamed pending (identifier 0) is covered by the final answers of the table's
  identifiers, so on a one-ECU table an unnamed lone pending is no longer open once the table's
  identifier has its final answer. The covering rule is one function
  (`standing_answers_covering`) in all three restatements of a collecting exchange's answers
  (`Kept::answered`, `primitive_ended`, the host's collected answers), and the log keys an
  unnamed late answer as the host does. The partial settlement's guard that settled whole when
  no identifier was left to owe the exchange on is removed, since `settles_owed` now says it.

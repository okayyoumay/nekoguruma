# ADR-445: A Final Response an Earlier Owner Could Own Leaves Its Exchange Owed, Whichever Entry Delivers It

**Date:** 2026-11-09
**Status:** Accepted (decision 2's "for `late_response` ... a collecting exchange only" superseded by ADR-446: a non-collecting exchange stays owed through an earlier owner too; decision 4 extended by ADR-446: a non-collecting record whose evidence settles the exchange owns no late final, whatever ended the VCI's wait)
**Affects:** `agent` (`comm_log.rs` `CommLog::primitive_ended`, `Inner::late_response`;
`comm_log/exchange.rs` `ExchangeEntry::owed_through_owners`, `Kept::late_final_settles`, `LinkState::release_kept_ended`,
`LinkState::reclassify_one`; `host.rs` `settle_cancelled`, comments only), design 8.11
"Settlement", `primitive_ended`, `late_response`; ADR-366 decision 3, ADR-431 decisions 1 and 2,
ADR-433 decision 1, ADR-436 decisions 1 and 4, ADR-442 decisions 1 and 2, ADR-444 Consequences

## Context

ADR-442 keeps an exchange whose final response came with its entry and that an earlier owed
request could own: it is owed on its identifier and its record stays for its rows until the owner
lets go. Two other entries decided from the evidence alone and never asked whether such an owner
stands. `primitive_ended` took a final response delivered at the primitive's end as the end of
the owed request, so the exchange was released at once and freed from the later owners, and a
late answer both could own was routed to the owner alone where the same exchange logged in time
names both. `late_response` did the same for a collecting exchange a lone late final answer
settled whole while an owner could own another of its answers. The release after the end
(`release_kept_ended`) also dropped a record the entry had kept for its rows and left its row.

Decided by the maintainer on 2026-11-09: the exchange stays owed and is kept, as one logged in
time (primitive end); and the same rule holds for `late_response`.

## Decision

1. **A settling final response ends the owed request only when no owed request sent before the
   exchange could own it.** One predicate, `ExchangeEntry::owed_through_owners` (the entry settles
   and one of its owners was sent before it), serves the entry's keep, `primitive_ended`,
   `late_response` and the release. Owed requests sent after the exchange do not count: the
   exchange's history lists them too, but they cannot keep it owed. An exchange owed on no
   identifier (`LinkEffect::owed_ids` empty) has no row to keep, so it is never owed through
   owners: its owners may include one that could own only a pending of it, which its rows do
   not count. This is a defensive invariant: no production path
   reaches a record owed through owners with no identifier, since a covered unnamed pending is
   dropped at every restatement, so every earlier owner owns a final answer whose identifier
   `owed_ids` adds.
2. **Otherwise the exchange is renewed as any exchange left owed** (ADR-431 decision 1): its rows
   come from `Evidence::owed_ids`, as when it is logged in time, for a collecting exchange the
   identifiers of the final answers an earlier owner could own, the others being freed from the
   later exchanges. If no row is left it is owed no more, as the entry's keep treats one. For
   `late_response` the rows of a collecting exchange are set again by `settle_on`; the rule
   holds there for a collecting exchange only (design 8.11 names collecting only): a
   non-collecting exchange a late final settles is settled whole, as before, though an owner
   could own the answer, which stays an open question for a functional link.
3. **Its verdict is final; the owner's letting go only releases it.** The classification of the
   end (`system` within the limit) is not a floor. The existing visits release the record and
   rows when the owner lets go (ADR-442 decision 3; `reclassify_one` frees the answer of a
   window entry only once it has no row), counting its send and its matched answer once then; at
   the end the answer counts in `answered` and `answered_since_error`, the matched answer and the
   lowering of its unknown send wait for the release, unless the end's own transmit evidence
   confirms the send.
4. **The record owns a late final only as its evidence says.** `late_final_settles` follows the
   evidence: false for a non-collecting usable answer (ADR-443 decision 1), true for a collecting
   window with a finished status, as in time. A pending open when the record was kept
   (`Kept::pending_at_end`) gives it a late final only while its evidence does not settle the
   exchange: once the end has read the final answers and the record is kept for its rows, it
   owns none, as its in-time twin owns none, whatever the end after them (a link error, a lost
   VCI). The release after the end keeps the record while
   `late_final_settles` holds or the record is still owed through an owner (derived from its
   rows, with no stored flag), so a record kept at the entry for its rows survives the end.
5. **Scope.** An end that does not settle (a wrong echo, a pending only, no response, a cancel, a
   link error, a lost VCI) is renewed, kept or released as before. A link error read before the
   final response at the end, with an owner standing, still counts the deferred matched answer
   in the entry's epoch, one too few; it needs new writers of `Kept::clears` and `Logged::after`
   (ADR-444) and is a backlog item.

## Consequences

Revises:

- ADR-431 decision 1 ("no final response by `settles_owed`") and decision 2 (the release's
  reasons to keep a record): an exchange whose final response an earlier owed request could own
  is renewed and kept too.
- ADR-442 decision 1: it applies to the entries that settle at the primitive's end and by a late
  response, with the owners sent before the exchange; the release after the end keeps the record
  it keeps.
- ADR-442 decision 2 ("the record owns no late final answer"): a record kept at a collecting end
  whose finished window shows the VCI ended owns one, as its evidence says (decision 4).
- ADR-366 decision 3 ("a final response ends the exchange as an owed request"): not when an
  earlier owed request could own it.
- ADR-436 decision 1: the release predicate shared with the entry's keep gains the rows reason.
- ADR-436 decision 4 ("the flag does not lapse", `Kept::late_final_settles` is the evidence's
  or the flag): the flag lapses once the record's evidence settles the exchange and it is kept
  only for its rows (decision 4 above); it still never lowers while a pending can be open.
- ADR-433 decision 1 ("settled whole"): not while an owed request sent before could own one of
  the exchange's answers.
- ADR-444 Consequences bullet 1: an exchange a final response settled at its end and that was owed
  through owners is kept, so beyond the window its matched answer counts once, when it is
  released.

Cost: more records are kept under ADR-442's cost paragraph, for such exchanges only. The bound
on the owners matters in production for `late_response`: later polls of the same request are
routinely among the owners a late final's history lists (`history_settling` leaves out only the
exchange itself), and without the bound the earliest poll would stay owed through a poll sent
after it. At the primitive's end no later request is logged yet in the host's flow (it cancels
the stray before the link's next request), so there the bound matters to the order of logging
only.

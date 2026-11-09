# ADR-445: A Final Response an Earlier Owner Could Own Leaves Its Exchange Owed, Whichever Entry Delivers It

**Date:** 2026-11-09
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `CommLog::primitive_ended`, `Inner::late_response`;
`comm_log/exchange.rs` `ExchangeEntry::owed_through_owners`, `LinkState::release_kept_ended`,
`LinkState::reclassify_one`; `host.rs` `settle_cancelled`, comments only), design 8.11
"Settlement", `primitive_ended`, `late_response`; ADR-366 decision 3, ADR-431 decisions 1 and 2,
ADR-433 decision 1, ADR-436 decision 1, ADR-442 decision 1, ADR-444 Consequences

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
   exchange's history lists them too, but they cannot keep it owed.
2. **Otherwise the exchange is renewed as any exchange left owed** (ADR-431 decision 1): its rows
   come from `Evidence::owed_ids`, as when it is logged in time, for a collecting exchange the
   identifiers of the final answers an earlier owner could own, the others being freed from the
   later exchanges. If no row is left it is owed no more, as the entry's keep treats one. For
   `late_response` the rows of a collecting exchange are set again by `settle_on`.
3. **Its verdict is final; the owner's letting go only releases it.** The classification of the
   end (`system` within the limit) is not a floor. The existing visits release the record and
   rows when the owner lets go (ADR-442 decision 3; `reclassify_one` frees the answer of a
   window entry only once it has no row), counting its send and its matched answer once then; at
   the end the answer counts in `answered` and `answered_since_error`, the matched answer and the
   lowering of its unknown send wait for the release, unless the end's own transmit evidence
   confirms the send.
4. **The record owns a late final only as its evidence says.** `late_final_settles` follows the
   evidence: false for a non-collecting usable answer (ADR-443 decision 1), true for a collecting
   window with a finished status, as in time. The release after the end keeps the record while
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
- ADR-366 decision 3 ("a final response ends the exchange as an owed request"): not when an
  earlier owed request could own it.
- ADR-436 decision 1: the release predicate shared with the entry's keep gains the rows reason.
- ADR-433 decision 1 ("settled whole"): not while an owed request sent before could own one of
  the exchange's answers.
- ADR-444 Consequences bullet 1: an exchange a final response settled at its end and that was owed
  through owners is kept, so beyond the window its matched answer counts once, when it is
  released.

Cost: more records are kept under ADR-442's cost paragraph, for such exchanges only. In
production no request sent after the exchange is logged before its end (the host cancels the
stray before the link's next request), so the bound on the owners matters to the log-level
tests and to the order of logging, not to the host's flow.

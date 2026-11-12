# ADR-446: A Non-Collecting Exchange a Late Final Settles Stays Owed Through an Earlier Owner

**Date:** 2026-11-10
**Status:** Accepted (decision 3 extended by ADR-450: the record owns no lone late pending or start either)
**Affects:** `agent` (`comm_log.rs` `Inner::late_response`; `comm_log/exchange.rs`
`Evidence::owed_ids`, `Kept::late_final_settles`), design 8.11 "Owed request", `late_response`;
ADR-432 decision 2, ADR-443 Consequences, ADR-445 decisions 2 and 4

## Context

ADR-445 decision 2 left a non-collecting exchange that a late final settles settled whole, and
named the functional link as open. On a physical link routing excludes the case: the late final
and the restated history read the same rows by the same identifier and bytes, so a lone late
final never reaches an exchange that an owner could also own. On a link with a receive table the
restated exchange has no identifier of its own, so an owner owed on another identifier stands.
Its row on identifier 0 (ADR-432 decision 2) is one no named answer finds, so even the exchange
logged in time, with its final response, was owed in name only.

Decided by the maintainer on 2026-11-10: it stays owed through such an owner, as a collecting
exchange does; and, once the design was summarised, that on a link with a receive table a
non-collecting exchange with a final response is owed on its own identifier and, when that is 0,
on every receive identifier. No production path sends such a request today (ADR-443): the only
link with a receive table carries the presence request, which collects, so this keeps the two
kinds of exchange alike for when one does.

## Decision

1. **`late_response` applies `owed_through_owners` to every exchange.** A final that settles the
   exchange ends its owed request only when no owed request sent before it could own its answer
   (ADR-445 decision 1), collecting or not. The existing settle-on path restates the rows.
2. **A non-collecting exchange with a final response on a link with a receive table is owed on
   its own identifier and, when that is 0, on every receive identifier.** One function
   (`Evidence::owed_ids`) serves every writer of the rows, so the entry, the renewal, the
   restatement and the refresh agree, and the set does not depend on owners (ADR-435 decision 7).
   A rejected end and a silent one are as before; a link without a table is as before.
3. **Its record owns no late final once its evidence settles it, whatever ended the VCI's wait.**
   `Kept::late_final_settles` is false for a non-collecting record whose evidence settles the
   exchange; `vci_ended` stands as the end set it, since the verdict of a later answer rests on
   it. Without this the restated record would own later lone finals that only it matches.
4. **The existing visits release it, counted once** (ADR-442 decision 3, ADR-445 decision 3):
   the owner's letting go lowers its unknown send and counts its matched answer once, and the
   owner's `late_response` does not name it.
5. **Scope.** A collecting exchange is unchanged; ADR-445 decision 5 stands.

## Consequences

Revises:

- ADR-445 decision 2 (a non-collecting exchange a late final settles is settled whole) and
  decision 4 (the record owns a late final as its evidence says: not for a settled non-collecting
  one, whatever its end).
- ADR-432 decision 2: a non-collecting exchange with a final response on a link with a receive
  table is owed on the whole table when its own identifier is 0.
- ADR-443 Consequences: the same, for a usable or negative final response.

The rows of such an exchange on a functional link change in time as well as late: it now holds
table rows where it held a row on 0 that no named answer found, and an exchange in flight there
awaits a named answer after its final response, as on a physical link. More late answers are
ambiguous (named for both the owner and the exchange) where the log used to pick the earlier
owner. Cost as ADR-442: a record is kept for its rows until the owner lets go.

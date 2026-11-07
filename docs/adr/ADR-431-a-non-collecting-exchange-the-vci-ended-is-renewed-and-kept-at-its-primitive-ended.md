# ADR-431: An Exchange Left Owed by Its `primitive_ended` Is Renewed, and Kept When the VCI Ended Its Wait

**Date:** 2026-11-07
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `CommLog::primitive_ended`, `release_kept_ended`;
`comm_log/exchange.rs` `LinkState::renew_owed`, `LinkState::release_kept_ended`; `host.rs`
`settle_cancelled`, comments only); design 8.11 `primitive_ended`, `late_response`; ADR-367 item
3, ADR-429 item 3, ADR-366 item 1

## Context

ADR-429 made `primitive_ended` renew the owed rows and the kept evidence of a collecting
exchange it left owed, and made the host's release after the entry (ADR-367 item 3) spare an
exchange owed per an open pending identifier. An ordinary (non-collecting) exchange got neither.
When the agent's deadline ended it (`vci_did_not_end`, so `vci_ended` false in its entry) and its
primitive then ended by the VCI's receive timeout, the entry restated that end, but the kept
evidence still showed the entry's `vci_ended` false. `late_response_owner` owns a response only
by an exchange whose kept evidence shows the VCI ended its wait, so the exchange's own response,
read at job end or in the next exchange's wait, was `unattached`, while a sibling whose entry
already showed `vci_ended` was its `late_response`. The release also dropped the record at once,
since no pending was open.

## Decision

1. **Every exchange a `primitive_ended` leaves owed is renewed** (no final response by
   `settles_owed`), collecting or not: `renew_owed` sets its owed rows from the evidence the
   end restated, and `update_kept` replaces its kept evidence, send and transmit. `renew_owed`
   clears the kept late pendings only for a collecting exchange, whose placed pendings already
   include them (`Kept::all_pendings`); an ordinary exchange keeps its late pendings.
2. **The release after the entry keeps the record of an exchange that is still owed and either
   has an identifier a pending is open on (ADR-429) or whose renewed kept evidence shows
   `vci_ended`.** That is the case when the primitive ended by the VCI's receive timeout. Its
   response is then its `late_response` (design 8.11), judged by its time against the limit the
   renewed evidence shows, and a next exchange that awaits the same response makes it
   `unattached` with an unowned `late_response` naming the kept exchange, as for two owners.
3. **A cancel, a link error, a lost VCI and an unknown end are unchanged:** the end leaves
   `vci_ended` false, so the record is released and a later response is `unattached`
   (ADR-366 item 1).

## Consequences

- An ordinary exchange's own response read after its primitive's receive timeout is attached
  to it. A result bound to an ended primitive that was `unattached` with `primitive` `earlier`
  can now be a `late_response`. No existing test pinned the old outcome.
- The `system` that the `primitive_ended` entry gives an end within the limit stands under the
  late response (design 8.11 "Settlement"), so a response past the limit is `system` with the
  reason that the VCI had ended its wait at the limit, not `vehicle`.
- Deferred: a response read during the stray primitive's job-end wait with no owner bound is
  never written, since only link errors among the end's unattached items reach the log.

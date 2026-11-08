# ADR-431: An Exchange Left Owed by Its `primitive_ended` Is Renewed, and Kept When the VCI Ended Its Wait

**Date:** 2026-11-07
**Status:** Accepted (the Consequences' deferred bullet on a response read in the stray primitive's job-end wait closed)
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
   clears the kept late pendings, which its placed pendings already include
   (`Kept::all_pendings`).
2. **The release after the entry keeps the record of an exchange that is still owed and either
   has an identifier a pending is open on (ADR-429) or whose renewed kept evidence shows
   `vci_ended`.** That is the case when the primitive ended by the VCI's receive timeout. Its
   response is then its `late_response` (design 8.11), judged by its time against the limit the
   renewed evidence shows, and a next exchange that awaits the same response makes it
   `unattached` with an unowned `late_response` naming the kept exchange, as for two owners.
3. **An end that read no receive timeout is released as before:** a cancel, or a link error, a
   lost VCI or an unknown end with no receive timeout read, leaves `vci_ended` false, so the
   record is released and a later response is `unattached`. `PrimitiveEnded.vci_ended` is true
   when a receive timeout was read, whatever else the end read after it, so such an end that
   did read one is kept, as an entry with `vci_ended` is.
4. **An ordinary exchange keeps its late pendings with their verdicts.** Renewal sets its
   placed pendings to the entry's and the end's own and leaves `late_pendings` (each with its
   `LatePending` verdict, ADR-371 item 4) and `late_places`, the number of placed pendings
   before each, so `Kept::all_pendings` and `Kept::limit_pendings` merge them in the order of
   `evidence.pending`: a late one is passed over, an open one leaves the limit unknown, an
   in-time one extends it. A collecting exchange is renewed as ADR-429 had it (its placed
   pendings include the late ones and the late list is cleared), since its windows read the
   placed pendings by their place and would count the late ones twice.

## Consequences

- An ordinary exchange's own response read after its primitive's receive timeout is attached
  to it. A result bound to an ended primitive that was `unattached` with `primitive` `earlier`
  can now be a `late_response`. No existing test pinned the old outcome.
- The `system` that the `primitive_ended` entry gives an end within the limit stands under the
  late response (design 8.11 "Settlement"), so a response past the limit is `system` with the
  reason that the VCI had ended its wait at the limit, not `vehicle`.
- Closed: a response read during the stray primitive's job-end wait with no owner bound is
  written. `primitive_ended` hands it over at the item that ended the primitive (its receive
  timeout, finished status, final response or ending error): a response read before that item
  is read while the exchange is in flight and is placed as `exchange_with` places one, before
  the entry, with the stray at least `undetermined` when it awaits the response; one read at or
  after it is read after the VCI ended the wait (decision 2) and is placed after the entry,
  beside the link errors, with the stray an ended owner. A cancelled primitive has no such item,
  so everything it read counts as read in flight.

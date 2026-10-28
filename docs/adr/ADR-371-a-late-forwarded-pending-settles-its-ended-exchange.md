# ADR-371: A Late Forwarded Pending Settles Its Ended Exchange

**Date:** 2026-10-28
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `UnattachedItem::pending_bytes`,
`LateIndication::Pending`, `late_indication_owners`, `late_response_owner`, `Inner::late_item`;
`comm_log/exchange.rs` `Expected::pending`, `LinkState::late_pending_owners`,
`LinkState::add_kept_late_pending`, `Kept::late_pendings`, `Kept::limit_pendings`, `LatePending`,
`settle_late_pending`); design 8.11 ("Settlement", `late_item`); ADR-346, ADR-365,
ADR-370

## Context

Design 8.11 has a forwarded pending that came after the VCI ended its exchange, bound to no
primitive, settle the one ended exchange that could own it in a `late_item`. It is judged
against that exchange's limit in force before it, and the late-pending row does not apply:

- within the limit less the margin it is `system`, since the ECU answered in time and the
  system missed it;
- after the limit plus the margin it is `vehicle`, since the VCI ended its wait in time;
- otherwise it is `undetermined`.

On a link whose VCI absorbs pendings, or one whose application-layer profile declares another
code a pending code, a forwarded pending is not judged against the limit before it.

The worker gives a received message to the primitive in flight when it matches that primitive's
expected responses. A pending for an earlier request with another negative pattern therefore
reaches the log unbound, during a later exchange. The log wrote it `unattached`.
`late_response_owner` already skipped it as a late item, not a late response. ADR-370 built the
`late_item` path for indications: one owner, by VCI time against the exchange in flight's
transmit done, or `unattached` with each possible owner at least `undetermined`.

## Decision

1. **Which items.** A received result bound to no primitive whose message is a negative response
   with the response-pending code (`7F`, a service identifier, 0x78), with a response
   identifier. It is a late item, never a late response.
2. **Possible owners.** The owed requests awaiting a response on that identifier whose negative
   pattern the pending carries (`Expected::pending`). They are complete only when the log keeps
   every one of them. The exchange in flight could own it when it awaits the same identifier
   and pattern and the pending is not placed before its own transmit done (ADR-370 item 2).
3. **One owner.** The pending is added to E's pendings in its entry. E's own timing is judged
   again as ADR-370 does for an indication, but with the pendings its entry saw only: the
   late-pending row does not apply to a pending that came after the VCI ended the exchange.
   The pending is judged against the limit in force before it (`judge_late`, its end on one
   frame), with what E's own rows give, as `settle_late_response` judges a late response:
   - within the limit less the margin, `system`;
   - after the limit plus the margin, what E's rows give when that is at least `undetermined`
     (a `system` its own timing showed, such as a receive timeout well before the limit,
     stands), else `vehicle` when the VCI is shown to have ended its wait at the limit, else
     `undetermined`;
   - otherwise, and always on a link whose VCI absorbs pendings, at least `undetermined`.

   When this is more than E's rows give, it is kept as E's floor, so it stands when a later
   settling entry classifies E again from its history; a floor an ambiguous item set stands too.
   E stays owed, since a pending is no final response. Its answer counts for the later
   exchanges as ADR-368 counts a pending.
4. **The limit after it.** E keeps the pending, with its judgement, for a later late item or
   response. Only one in time carries the limit in force on (P2* from it). One past the limit
   carries nothing on: the VCI had ended its wait before it and the ECU was already late, so a
   later response is judged against the limit before it, and is not `system` for coming within
   a P2* the VCI never waited for. After one whose time is not shown, the limit in force is not
   known, and a later response is not judged against it.
5. **Two or more possible owners.** The pending stays `unattached`. A `late_item` raises each
   kept owner as ADR-370 item 5 does. The exchange in flight is at least `undetermined` when it
   has no answer of its own.

## Consequences

- A pending E's entry saw judges E's timing; one that came after E ended is judged on its own.
  E's `pendings` in the log show both, and the intervals between them when every one is placed
  in order.

- A pending bound to its primitive and read after its exchange's entry is the ADR-367 case: a
  stray primitive's pendings go to its `primitive_ended`. This ADR covers the unbound ones.
- Only the UDS shape of a pending (`7F`, the service, 0x78) is taken as a late item. A protocol
  with another negative pattern keeps the general check in `late_response_owner`, which leaves
  such a pending `unattached`.

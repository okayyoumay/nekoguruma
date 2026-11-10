# ADR-450: A Record Kept Only for Its Rows Owns No Lone Late Pending or Start

**Date:** 2026-11-10
**Status:** Accepted (decision 4's unchanged transmit indication and the Consequences' bullet on a lone transmit indication superseded by ADR-452: it confirms the send only)
**Affects:** `agent` (`comm_log.rs` `Inner::late_item_lone_owner`; `comm_log/exchange.rs`
`Kept::late_pending_or_start_settles`), design 8.11 `late_item`; ADR-370 item 4, ADR-371
item 3, ADR-442 decision 2 and Consequences, ADR-445 decision 4, ADR-446 decision 3

## Context

ADR-442 keeps the record of an exchange that is owed only because an earlier owed request could
own its final answer, for its rows. ADR-445 and ADR-446 say that its verdict stands until the
owner lets go, and that it owns a lone late final answer only as its evidence says: never for a
non-collecting record whose evidence settles the exchange, and for a collecting one only when
its finished window shows the VCI ended.

The step that gives an unbound forwarded pending or start-of-message indication to its one
possible owner had no such test. On a link with a receive table such a record can be owed on
more identifiers than its owner: through its own identifier 0 (ADR-446) or through an answer
that names no identifier, which may have come on any identifier of the table (design 8.11
"Owed request"). A pending or a start named on one of those identifiers had
the record as its only possible owner. The `late_item` then restated the record with the item
and classified it again from its entry, which dropped the verdict of the late final or of the
primitive's end: a record judged `system` by a late final in time became `undetermined`. On a
physical link the record shares its one identifier with its owner, so it is never the only
possible owner while the owner's row stands.

The maintainer decided on 2026-11-10 that such a record owns no lone late pending or start, and
chose the gate that `Kept::late_final_settles` is for a lone late final: a record owns a lone
late pending or start when, and only when, it owns a lone late final. Among the owners of an
ambiguous item it is named and floored as before.

## Decision

1. **A lone late pending or start does not settle a record whose evidence settles its exchange
   and that a late final would not settle.** When that record is the one kept exchange that
   could own an unbound forwarded pending or start-of-message indication, no `late_item`
   settles it. The item is written `unattached` and nothing else is written: no exchange is
   named or raised, and the record keeps its classification, reason, evidence, floor and rows.
2. **The gate is in the lone-owner step, not in the owners.** The record still counts among the
   possible owners (`LinkState::late_start_owners`, `LinkState::late_pending_owners`), and
   "every owner kept" keeps its meaning. With another kept owner, an exchange in flight that
   uses the identifier, or an owed request the log keeps nothing of, the item is ambiguous as
   before and the record is named and floored (ADR-442 decision 2, ADR-370 item 5, ADR-371
   item 5).
3. **The gate is the one on finals** (ADR-445 decision 4). It holds for a non-collecting
   record whose evidence settles the exchange, whatever ended the VCI's wait (ADR-446
   decision 3), and for a collecting one whose evidence settles the exchange and whose window
   is not shown ended by the VCI, also when a pending was open when it was kept. A collecting
   record whose finished window shows the VCI ended keeps its lone pending or start as it keeps
   its lone late final.
4. **Scope.** An exchange whose evidence does not settle it is unchanged, including one that a
   late final does not settle because the agent's deadline ended it and its primitive still
   runs: the gate is not `late_final_settles` alone. A transmit indication or loopback echo is
   unchanged: it can still confirm the send of such a record (ADR-442, ADR-448).

## Consequences

- No log format change. An item of this kind is now one `unattached` line where a `late_item`
  named the record.
- What the item showed is attached to no exchange. If it was the record's ECU after all, the
  log shows that only as the `unattached` line. The record's evidence has its answers complete,
  so the log does not read the item as its own.
- One such record alone leaves the item plain and nobody floored; two of them, or one beside
  any other possible owner, are each named and floored. A late final behaves the same way
  (ADR-442 decision 2).
- The record still makes a pending or start ambiguous for an exchange in flight on its
  identifiers, as ADR-442's cost paragraph says.
- A collecting record whose finished window shows the VCI ended is still classified again by a
  lone pending or start, as by a lone late final.
- A lone transmit indication still classifies such a record again while it confirms its send;
  that is a backlog item.
- The gate reads the record's evidence when the item is placed. Unbound items are offered to
  their one owner before the late finals of the same read are placed, so a pending or start read
  together with a late final that settles the record is still taken by it, while the same two
  items read in two batches leave the pending or start `unattached`. Before this decision the
  record took it either way; making the order not matter is a backlog item.
- A gated start-of-message indication stays among the unbound items, so an exchange in flight on
  the same response identifier that does not take it records that a start it did not take was
  read (`start_unattached`), as it does for an ambiguous start today.
- Revises: ADR-370 item 4 and ADR-371 item 3 (the one owner is not settled when it is such a
  record), ADR-442 decision 2 and its third Consequences bullet, ADR-445 decision 4 and ADR-446
  decision 3 (each extended from a lone late final to a lone late pending or start).

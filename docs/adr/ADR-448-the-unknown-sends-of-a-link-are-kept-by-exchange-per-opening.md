# ADR-448: The Unknown Sends of a Link Are Kept by Exchange, per Opening

**Date:** 2026-11-10
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `LinkState` `unknown_sends`, `LinkState::reopened`,
`LinkState::may_own_tx_indication`, `LinkState::alone_may_own_tx_indication`,
`LinkState::late_transmit_owners`; `comm_log.rs` `Inner::taken_indications`), design 8.11
`sent_evidence`, `late_item`, "Owed request"; ADR-346 decision 4, ADR-387, ADR-439 decision 3,
ADR-442 decision 3

## Context

Design 8.11 lets an ended exchange whose `sent` is `unknown` own a late transmit indication
only on the same opened link: the worker gives an indication to an open link, so a send from
before the link was opened again cannot show up on the new one. The log counted such sends
(`unknown_sends`, a number). A reopening set the number to 0 but left the kept records it had
counted at `sent` `unknown`, so the number and the records disagreed. A record from before the
reopening could become the one owner of a later exchange's indication, which then confirmed
the wrong exchange, and its later release or confirmation took a send logged after the
reopening out of the number, so that send was missed as an owner. ADR-442 keeps such records
longer, which widens the gap.

`LinkState::reopened` has no production caller yet: the log's own entry points for a reopened
link (`CommLog::link_opened`, `CommLog::vci_connected`) do not call it. The failure was
reachable only by calling it directly, and this decision changes no production log output.

## Decision

1. `LinkState` holds the `seq` of each exchange logged on the current opening of the link whose
   `sent` is still `unknown`, kept or not. Recording such an exchange adds its `seq`; a
   confirmation of one, and the release of one by an answer (`settle_owed`, `update_kept`,
   `free_owner_answer`, an emptied row set), removes its `seq`, under the condition each path
   has had; a reopening empties the set. Removing a `seq` the set does not hold changes
   nothing, so the release of a record from before a reopening leaves the later sends alone.
   A release that is no answer (`release_kept`, `release_kept_ended`, and the drop of the
   records an accepted restart makes) leaves the `seq` in the set, as the count it replaces
   was not lowered there either: the exchange stays an unknown send of its opening the log
   keeps nothing of, so `late_transmit_owners` reports not every owner known until the next
   reopening.
2. A kept record that crosses a reopening keeps its `sent`, its owed rows, its classification
   and its floor, and nothing is written for it: a reopening is no evidence about the send.
3. It owns no transmit indication or loopback echo read afterwards and makes none ambiguous: the
   owners of an unbound one are the kept exchanges of the set, and the owners are all the
   candidates when each member of the set has a kept record. `primitive_ended` gives such a
   record no unbound transmit indication either (`alone_may_own_tx_indication`). An indication
   bound to the record's own primitive stays its own, since the binding decides.
4. This decision changes only the ownership of transmit indications and the bookkeeping of the
   unknown sends. The owed requests and the kept records are left as they are, so what they own
   across a reopening (a start, a pending, a final answer) is unchanged by it; this ADR does not
   say whether that is right.
5. A record's release or confirmation leaves the later opening's unknown sends as they are.

## Consequences

- No log format change: `schemas/` and `api/asyncapi.yaml` are untouched.
- After a reopening a transmit indication can attach again to an exchange logged after it, and
  can no longer confirm the send of an exchange from before it. An answer still can.
- The set holds one `u64` per unknown send of the opening, and an unkept one (left the window, or
  released by `release_kept`) stays in it for the whole opening, as the count it replaces did not
  go down for it. It grows by one `u64` per such exchange per opening, where the count was a
  constant `u32`, and `late_transmit_owners` walks it for each unbound transmit indication. The
  entries are far smaller than a `Kept`. The type change makes the compiler list every writer of
  the set.
- `update_kept` can write `unknown` over a `sent` that was `confirmed` (the rebuild a `late_item`
  makes from `history_settling`) without adding the `seq` to the set. The record is then no
  transmit owner, and `late_transmit_owners` can report every owner known although a kept
  `unknown` record of the current opening is not one. No known input reaches it.
- An exchange that predates a reopening and was confirmed by an answer alone (no transmit
  evidence) can still take an unbound indication in a `primitive_ended` the host writes after
  the reopening. The host never writes that order today; excluding it needs an opening marker
  on every owed request.
- `LinkState::reopened` is still not called by the log's own reopening. Until it is, matched
  answers from before a second `link_opened` still count and an unknown send from before it
  still blocks later transmit indications, which design 8.11 does not intend. The wiring must
  land after this decision, since calling the earlier `reopened` from `link_opened` would give
  a `confirmed` to the wrong exchange.

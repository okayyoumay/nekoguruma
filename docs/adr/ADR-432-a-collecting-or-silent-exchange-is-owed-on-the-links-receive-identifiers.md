# ADR-432: A Collecting or Silent Exchange Is Owed on the Link's Receive Identifiers

**Date:** 2026-11-07
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `Evidence::receive_ids`, `Evidence::owed_ids`,
`LinkState::set_owed_rows`, `LinkState::history_for`, `LinkState::record`,
`LinkState::renew_owed`; `comm_log.rs` `late_response`, `late_response_owner`; `host.rs`
`receive_ids_of`), design 8.11 "Owed request"; ADR-429 decision 2, ADR-400

## Context

ADR-429 owes a collecting exchange once per response identifier whose last standing answer is a
pending, and otherwise "as before": on its link's own response identifier. A functional link
(`LinkConfig::functional`, the presence link) has no identifier of its own, so its `rx_id` is 0,
which no ECU's answer carries. Two cases were therefore owed on identifier 0 only, and the ECU's
later final answer on a table identifier (0x7E8, 0x7E9) was `unattached`: an exchange with no
answer, and one whose lone pending came with no identifier (`Answer.rx_id` 0). A third effect
followed from the same lookup: `history_for` found a presence resend's owners by the resend's own
`rx_id` 0, so a resend whose final answer an earlier owed exchange could own was placed alone and
not itself owed, while its answers' histories left it `undetermined` (ADR-400 row C).

## Decision

1. **The host gives each exchange's evidence the identifiers of its receive table**
   (`Evidence::receive_ids`, from the table the runner sets for the link, each identifier once,
   not written to the log). An empty list stands for the link's own `rx_id`, so a physical link,
   whose table is its own identifier, is unchanged.
2. **One function names the identifiers an exchange is owed on** (`Evidence::owed_ids`): those a
   pending is open on, where an unnamed pending (identifier 0) stands for every receive
   identifier with no final answer in the exchange (ADR-400 decision 1: the pending is judged
   against each one's window); with none open and no final response, every receive identifier;
   with none open and a final response, the identifier of the first final answer (the link's own
   when no answer names it). `LinkState::set_owed_rows` takes that list, once per identifier,
   from `record`, from `renew_owed` and, for a late response that names no identifier, as the
   list it is judged against (differing verdicts leave the window open, as ADR-429 had it).
   The exchange in flight that is not collecting awaits a response on those identifiers too.
3. **`history_for` of a collecting exchange looks the owners up by its answers' identifiers,**
   deduplicated by exchange, not by its own `rx_id`. A presence resend whose final answer an
   earlier owed exchange could own is then owed itself and `undetermined`, as ADR-429 intended.
4. **Not changed:** the owner lookups for a late response, start and pending (they match owed
   rows by identifier as before; the rows are what changed), the start-indication checks that
   read the link's own `rx_id`, and the host's presence marking.

## Consequences

- On a functional link the ECU's late final answer on a table identifier is the
  `late_response` of an exchange that got no answer or an unnamed lone pending, and settles it
  whole (ADR-429 decision 3). An answer on an identifier outside the table is `unattached`.
- ADR-429 decision 2's fallback "on its link's identifier" is superseded by decision 2 above,
  and the deferred bullet on an unnamed pending and an exchange with no answer is closed.
- Deferred, kept as backlog items: an identifier that stays silent beside another identifier's
  final answer is not owed (ADR-429 decision 2 stands: only an open pending owes an identifier
  once an answer exists); settling per identifier.

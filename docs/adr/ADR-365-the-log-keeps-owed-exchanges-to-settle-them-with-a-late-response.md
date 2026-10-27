# ADR-365: The Log Keeps Owed Exchanges to Settle Them with a Late Response

**Date:** 2026-10-27
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `exchange_with`, `late_response_owner`,
`Inner::late_response`, `LateResponseEntry`; `comm_log/exchange.rs` `Kept`, `LinkState::keep`,
`late_response_owners`, `settle_owed`, `settle_late_response`; `comm_log/timing.rs`
`judge_late`); design 8.11 ("Settlement", "Owed request", `late_response`); ADR-346, ADR-361,
ADR-364

## Context

Design 8.11 never rewrites an exchange's entry. Evidence that comes later is written as a
settling entry. That entry names the exchanges it settles, carries what a reader needs without
the earlier entry, and classifies each of them again from all the evidence known by then. The
first such evidence the log can see is a response the worker delivers after the VCI ended its
exchange. It reaches the log as an unbound item of a later exchange, and the log wrote it
`unattached`.

To settle the exchange, the log needs that exchange's evidence again. Its limits and VCI times
are needed too, since the response is judged against that exchange's limit in force. The log
kept only enough of an owed request to match an answer: its `seq`, response identifier, SID
and match.

## Decision

1. **The log keeps each owed exchange.** For each exchange it logs, it keeps:
   - the evidence;
   - its entry's origin, request and classification;
   - its placed transmit and pending times;
   - the link's limits.

   This is kept only while the exchange is owed (`LinkState::keep`). It is dropped when the
   exchange is settled, or when an accepted ECUReset or session change ends the owed requests.
   The log already bounds the owed list by the job, and the kept evidence has the same bound.
2. **A late response needs exactly one owner.** An unbound result item is a late response when
   all of these hold:
   - it is a message another node sent: no indication, loopback or break flag, and bytes;
   - exactly one owed request could own it, by its response identifier and match;
   - that request's exchange was ended by the VCI;
   - the exchange in flight does not await the same response;
   - it is not a pending of that request (a pending is a `late_item`).

   Anything else stays `unattached`. An exchange the agent's deadline ended (`vci_did_not_end`)
   is settled by `primitive_ended`, not here.
3. **It is judged by its end.** The item's time is its end (ISO 22900-2:2022 8.1.6.3), so
   `judge_late` takes these windows:
   - **In time:** an end at or before the earliest limit in force less the margin shows the
     response began in time.
   - **Late:** past the latest limit plus the margin, by more than a single frame's transfer,
     shows it began late. This needs a response of one frame (ADR-364). The amount is the end
     less that transfer less the limit.
   - **Open:** anything else, including a time not on the transmit's clock and epoch.

   The limits are those of the kept exchange, from its own pendings. On an absorbing link they
   are bounded as ADR-362 bounds them.
4. **The classification follows design 8.11's `late_response`:**
   - **In time:** `system`.
   - **Late:** `vehicle`, unless the rows that do not turn on timing give the exchange
     `system` or `undetermined` with this response as its end. Those rows are applied with the
     window taken as within the limit, and with the link's history as of the settling entry
     (for example, a code such as 0x35).
   - **Open:** `undetermined`, or a more severe classification those rows give.

   When the exchange's own entry was `system`, `system` stands, since the faults such an
   entry records (a receive timeout early or late, a late pending, a VCI fault) are observed,
   not inferred.
5. **The entry's form.**
   - `event` is `late_response`, and `refers_to` is the settled exchange's `seq`.
   - The response is given as an `unattached` event would give it.
   - `exchanges` holds one element, with the exchange's `seq`, `link`, `origin`, `request`,
     `classification` and `reason`.
   - The top-level `classification` is that element's.
   - `text` names the exchange.

   The settled exchange is owed no more, so a second response settles nothing.

## Consequences

- The log cannot tell a `positive_unusable` response: only the host decodes a response against
  the definition. A late positive response is taken as `positive`.
- A late response does not yet become a matched answer for later exchanges, and an exchange it
  settles is not yet removed from the owed requests later entries were classified against.
  Both are left to the item that reclassifies later exchanges.
- A response that two owners could own stays `unattached`. The design also asks that each of
  those owners becomes at least `undetermined`; that is left to the same item.
- `primitive_ended`, `vci_loss_settled` and `late_item` use the same form of settling entry and
  the same kept evidence.

# ADR-426: A Collecting Exchange's Late Items Are Judged From Their Window's Start

**Date:** 2026-11-07
**Status:** Accepted (Consequences' `past_vci_limits` gap closed)
**Affects:** `agent` (`comm_log` `Inner::late_item`, `Inner::late_response`, `late_window`;
`comm_log/timing.rs` `judge_late`, `judge_late_start`); design 8.11 `late_item`,
`late_response`; ADR-371 item 3, ADR-400 decision 1, ADR-425

## Context

ADR-400 decision 1 starts the window of a collecting request (the presence request) at the
transmit and restarts it at the end of each final answer, whatever identifier it came on, with
the pendings kept per response identifier (ISO 14229-2:2021 10.2.1, 10.2.3). ADR-425 made the
settling entries judge the exchange's own timing and its answers with that rule
(`answer_windows`, `judge_windows`), and left the judgement of a late item itself measured from
the transmit, with the limit in force the entry's pendings put in force (ADR-371 item 3,
`Kept::limit_pendings`). For a collecting exchange with answers that is wrong in both
directions:

- A late pending or response that is in time from the last answer's end, and late from the
  transmit, is called late, where the ECU answered in time and the system missed it.
- A pending of one identifier puts P2* in force for a late item on another, so an item that is
  late from the last answer's end is called in time.
- A late start of an exchange with answers was not judged at all: the gate was the end
  `ResponseIncomplete`, which only an exchange with no answer gets.

## Decision

1. **A collecting exchange's late item is judged as the final answer of the link's response
   identifier would be at that place.** One rule, `answer_windows`, gives the window; no second
   one is written. The late item stands for a final answer on the exchange's `rx_id`, read after
   the entry's items. The window is that identifier's pendings since its previous final answer
   (a pending the host did not name counts for it), and it starts at the end of the latest final
   answer of any identifier before the first of them, or at the end of the last final answer when
   there are none, else at the transmit. The entry's pendings and answers keep the places the
   host read them in; the late pendings in time or open follow them, in the order read, and a late
   one is passed over, as ADR-371 item 4 has it.
2. **The judgement runs from the window's start, the reported time from the transmit.**
   `judge_late` and `judge_late_start` take the window's start: the item must come at or after it
   and after every pending of the window, else the judgement is open; a start not placed on the
   transmit's clock and epoch, or no transmit time, leaves it open. The limit in force runs from
   the start. `InTime { at_us }` stays measured from the transmit, as every response time is
   (ADR-400), and `Late { by_us }` is the lateness from the start.
3. **`late_response` judges from the same window**, by the same function: the two are writers of
   one judgement. Any exchange that is not collecting is unchanged: the transmit and
   `limit_pendings` (ADR-371).
4. **A late start is judged when the VCI ended the window and the link's own identifier has
   no final answer standing.** The conditions are that the exchange's `vci_ended` is set, that
   its end is no link error, refusal, loss or unknown end (a start whose handle failed,
   ADR-416, is `Unknown`), and that the latest answer on the exchange's `rx_id` is none or a
   pending, whether or not other identifiers answered. A window the agent's deadline ended, or
   with a final answer on the identifier, is only restated, as ADR-425 does. The exchange's
   outcome is unchanged. The start must come after the identifier's own last pending in the
   window, not any identifier's, to be taken as the response's start.
5. **A late pending is kept with the identifier it came on** (`Evidence::pending_ids`), so a
   later late item takes it into the chain of that identifier only.
6. **The entry's own timing is unchanged.** The judgement of the exchange's own timing in a
   `late_item` keeps the entry's pendings only; a late pending is in no window of the entry
   (ADR-371 item 3, ADR-425 decision 3).

## Consequences

- Closes ADR-425's "Not decided here" for `late_item`'s own judgement, and amends ADR-371 item 3
  for a collecting exchange: its late pending is judged from the window the identifier's final
  answer would get, not from the transmit.
- A per-identifier window differs from the entry's merged window: an item on one identifier is
  judged from its own chain, so another ECU's open pending does not extend its limit.
- The agent's own deadline check for a collecting exchange with answers (`past_vci_limits`,
  design 8.11 `vci_did_not_end`) reads the entry's merged window, the one the entry's `timing` is
  judged from, as `at_limit` does: the limits run from the end of the last final answer, with
  the window's open pendings, not from the transmit. Still open: a lone pending of a collecting
  exchange counts as a final response, which owes the exchange no late item (ADR-400 reads it
  as none). It is a backlog item.
- A collecting exchange whose window the VCI ended is owed only when it has no answer at all,
  while a lone pending counts as a final response (the backlog item on `collecting_end`). So a
  late start of one whose other identifiers answered is reached only once that item lands. The
  rule is still the right one to have in place.
- `at_limit` (whether the VCI ended its wait at the limit, `settle_late_pending`,
  `settle_late_response`) keeps the entry's merged window: it asks about the VCI's single wait
  (ADR-400 decision 2), while lateness asks about the ECU's window, per identifier.
- `primitive_ended` read the placed pendings of a collecting exchange by their place in the
  evidence's list but built them from the entry's pendings only, so a late pending kept before
  it put the lists out of step (an out-of-bounds read, and the end's pendings measured from the
  wrong one). It now builds them from all the exchange's pendings, with the late ones in no
  window, and `judge_windows` checks the lists' lengths.

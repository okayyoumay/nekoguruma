# ADR-452: A Transmit Indication Only Confirms the Send of a Record Kept Only for Its Rows

**Date:** 2026-11-10
**Status:** Accepted
**Affects:** `agent` (`comm_log.rs` `Inner::late_item`; `comm_log/exchange.rs`
`Kept::late_transmit_reclassifies`, `LinkState::restate_send`), design 8.11 `late_item`;
`crates/agent/docs/ngr-agent.md`; ADR-370 item 4, ADR-442 Consequences, ADR-445 decision 3,
ADR-446 decision 4, ADR-448 decision 1, ADR-450 decision 4 and Consequences

## Context

ADR-442, ADR-445 and ADR-446 keep the record of an exchange that has its final answer but stays
owed because an earlier owed request could own that answer; its verdict stands until the owner
lets go. Its `sent` can still be `unknown`, since an answer an owner could own shows nothing
about the send. A transmit indication or loopback echo read later that only this record could
own was written as its `late_item` (ADR-370 item 4), which took the indication's time as the
transmit done, judged the record's timing again from it and classified the record from its
entry. That replaced the verdict: a record kept at its primitive's end as `system` became
`undetermined` when the indication's time put its response within the limit, and an in-time
record could be raised to `system` when it put the answer past it. ADR-370 assumed a kept
`system` rests on timing; since ADR-445 it can be the give-up the end observed, which design 8.11
"Settlement" says stays observed. ADR-450 closed the same gap for a lone pending or start and
left this path.

The maintainer decided on 2026-11-10: for a record whose evidence settles its exchange, such a
`late_item` confirms `sent` only and leaves the classification and reason as they were; a record
whose evidence does not settle it is still classified again. The predicate is
`Evidence::settles_owed` across all such records, including a record kept at its entry in time
(ADR-442): a late transmit indication then confirms its send only and no longer judges its
timing again.

## Decision

1. **The send is all that changes.** When a transmit indication or loopback echo has a kept
   record whose evidence settles its exchange (`Evidence::settles_owed`) as its one owner, the
   `late_item` gives the record its transmit evidence and transmit done, restates `sent` as
   `confirmed`, and takes its `seq` out of the link's unknown sends (ADR-448). The record keeps
   its classification, reason, timing, floor, late start and pendings, and rows. The item ends
   no owed request (ADR-442 decision 5).
2. **The entry is a `late_item` as before.** It restates `sent`, `sent_evidence` and the outcome
   and names the record with the classification it stands with and a fixed reason that says the
   indication confirms the send and the exchange keeps its classification. The log keeps no
   reason with a record beyond the window, so the standing reason is not written again. No field
   is added.
3. **The test is the evidence alone.** It is not the gate on a lone late final that ADR-450
   uses: a collecting record whose finished window shows the VCI ended owns a lone late final
   (ADR-445 decision 4) and is covered here all the same, as is a record kept at its entry in
   time (ADR-442). An indication of the send adds no answer, so it gives no ground to judge the
   answers again, whatever the record still owns.
4. **The rule is in the entry, not in the owners or the lone-owner step.** The record still owns
   the item (`LinkState::late_transmit_owners`), unlike a lone pending or start, which ADR-450
   leaves `unattached`.
5. **The timing is not judged again while the transmit time is kept.** The transmit time places
   a late link error against the record and judges a late final of a record that still owns
   one, and a `primitive_ended` written later for the record judges its timing from it.
6. **Scope.** An exchange whose evidence does not settle it is classified again as before. An
   indication two or more exchanges could own names and floors the record as before (ADR-442
   decision 2). A record from before a reopening owns none (ADR-448). An entry that takes its own
   indication at its primitive's end is unchanged.

## Consequences

- No log format change. The entry's reason for such a record is new text.
- A late transmit indication by itself no longer moves the verdict of a settled record in
  either direction. A timer fault that only the late transmit time would show is not reported
  for it; for a record that stands at `system` nothing more severe exists, and for an in-time
  record at `undetermined` this is the cost. The transmit time it stores (decision 5) can still
  move the verdict later: through a late link error placed beyond the window within the span
  the time gives the record (a backlog item), through a lone late final, pending or start of a
  collecting record whose window the VCI ended, and through a `primitive_ended` written later.
- An in-time record in the window is still reclassified from its history when its owner lets go,
  with the timing of its entry and its send confirmed.
- What such a record owns among late finals, pendings and starts is unchanged; the collecting
  record whose finished window shows the VCI ended is still classified again by those (a backlog
  item).
- Closes ADR-450's Consequences bullet on a lone transmit indication.
- Revises ADR-370 item 4 (the one owner's timing judged again and the owner classified again:
  not for such a record), ADR-442's third Consequences bullet (the record's `late_item` confirms
  its send and classifies nothing), ADR-450 decision 4 (a transmit indication is unchanged: it
  now confirms the send only).

# ADR-362: The Log Judges a Receive Timeout Only Before a Response Began

**Date:** 2026-10-26
**Status:** Accepted (Decision item 4 narrowed by ADR-364)
**Affects:** `agent` communication log (`comm_log/timing.rs` `judge`, `comm_log.rs`
`link_opened` and `CommLog::exchange_with`, `comm_log/exchange.rs` rows); design 8.11 ("Timing
judged", "Limit in force", `timing`); ADR-346, ADR-357, ADR-361

## Context

Design 8.11 judges an exchange's response start and the VCI's receive timeout against the limit
in force. That limit is P2 from the transmit until a pending; after a pending it is the earlier
of P2* from the latest pending and the 0x78 completion limit from the first.

Two cases are not covered by that wording.

- **A timeout after the response began.** Once the first frame of a multi-frame response has
  arrived, P2 is met. What ends the wait after that is the transport's consecutive-frame timer.
  ISO 22900-2:2022 D.4.2 names a consecutive-frame or flow-control timeout as extra error
  information of the receive timeout event. Judged against P2, such a timeout reads as the VCI's
  own late timer, which is `system`. The real cause, a frame lost on the network or the ECU
  stopping, is `undetermined` by the `response_incomplete` row.
- **A pending seen on a link whose VCI absorbs pendings.** Design 8.11 bounds the limit from the
  transmit there, assuming the pendings are unseen. A worker that does not say whether it
  forwards pendings still passes on the ones it sees, and each of them restarts the limit.

## Decision

1. **Windows are judged once `link_opened` records the link's limits.**
   - The limits are P2, P2*, the 0x78 completion limit and the timing margin.
   - A time is judged from the transmit done, on one clock and epoch (ADR-361).
   - Evidence on a link the log did not open keeps the timing it carries.
   - `link_opened`'s `response_times_judged` is true when the link has transmit indications.
2. **A receive timeout is judged only when no response may have begun.** A response may have
   begun when any of these holds:
   - a start of this exchange's response was seen;
   - a start-of-message indication with the response identifier was left unattached;
   - the link has no start-of-message indications to show one.

   In those cases the start, when there is one on the clock, is judged instead. Otherwise the
   window is `not_judged` with `response_begun`. The timeout rows apply to `no_response` only.
3. **On a link whose VCI absorbs pendings, the limit is a range.** The window is judged against
   it:
   - The expected limit is the one the seen pendings give, or P2 with none.
   - The earliest limit assumes an unseen pending at the transmit. It is the earlier of P2* from
     the latest pending seen and the completion limit from the transmit, and with no pending
     seen also P2.
   - The latest limit is the completion limit from the first pending seen, or from P2 with
     none. With no completion limit there is no latest.
   - A timeout before the earliest, less the margin, is `timeout_early`, and one past the
     latest, plus the margin, is `timeout_late`.
   - A timeout within the margin of the expected limit is `timeout_at_limit`.
   - A start within the expected limit is within it, and one past the latest is `late`.
   - Anything between is `possible_pending`.

   On a link that forwards pendings all three limits are the expected one.
4. **An end proves only that a response was on time.** An end with no start, past the expected
   limit, is `not_judged` with `end_after_limit`.
5. **Each other case that stays `not_judged` names its reason:**
   - `no_transmit_time`;
   - `other_clock`: a time, or a pending the limit depends on, that is not on the transmit's
     clock and epoch;
   - `before_transmit`;
   - `no_time_to_judge`.
6. **The rows that need a judged window now classify:**
   - `no_response` with `timeout_early` or `timeout_late` is `system`.
   - A final response whose start is `late` is `system`, with its outcome named.
   - A suppressed request's silence is `none` when no hidden cause is open, the send is
     `confirmed`, and either the VCI's timeout came at P2 or the VCI ended the wait on a link
     that forwards pendings.

## Consequences

- A link without start-of-message indications judges no receive timeout. A timeout there cannot
  be told from the transport's.
- A receive timeout of a single-frame response that never came is judged as before. It gives no
  start indication.
- Proving a response late by its end, judging each forwarded pending against the limit before
  it, and the `vci_did_not_end` row are left for later work. Their evidence needs per-protocol
  transfer bounds and the clock reading at the deadline.
- `margin_ms` reports the margin rounded up to whole milliseconds, while the judgement uses its
  exact microseconds.

# ADR-376: The Communication Log Presumes No Timer Right

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** design 8.11 "Classification" (H4, the paragraph after the hidden causes, the rule
above the table, the suppressed no-response rows, the `positive` row, the no-response rows
after a matched answer or a pending, the example of an ECU that stops answering); `agent`
communication log (`comm_log/exchange.rs`: `row`, `rows`, `silence_open`);
`crates/agent/docs/ngr-agent.md`; ADR-346 item 6

## Context

ADR-346 item 6 accepted exactly one presumption. Where the log cannot judge the timing, the
VCI's own timer, and its handling of pendings as `CP_RC78Handling` configures it, are presumed
right. Three kinds of outcome rested on it:
- a final response whose time is not judged was taken as on time, so a positive response was
  `none` and a refusal for the ECU's state `vehicle`;
- a receive timeout the log could not judge was taken as one at P2, so a silence after a matched
  answer or a pending was `vehicle`, and a suppressed request's silence `none`. On a J2534 link
  the worker's own timer was taken as right in the same way;
- on a link whose VCI absorbs pendings, a receive timeout judged at P2 was taken to show that no
  pending came, since a pending would have restarted the VCI's wait.

ADR-375 then ruled that the log states an outcome only when every case its evidence leaves
gives it, and adds no value it did not read; its item 5 left the existing rules standing until
they were checked. A timer that fired early or late, or a VCI that absorbed a pending without
restarting its wait, is consistent with the evidence in each case above, and gives another
outcome. The maintainer chose (2026-10-29) to remove the presumption, and to treat the J2534
worker's timer like the VCI's.

## Decision

1. **No presumption.** The log presumes no timer right: not the VCI's, not the J2534 worker's.
   Nor does it presume that the VCI handles a pending as configured. A timer is right only where
   judged timing shows it.

2. **A final response whose time is not judged.** A `positive`, `negative` or
   `positive_unusable` outcome with `window` `not_judged`, whose row would give `none` or
   `vehicle`, is `undetermined`. Its reason is the row's, and adds that the time is not judged,
   so the response may also have come after the limit in force with the VCI past its own limit,
   a `system` reading. A row that already gives `undetermined` or `system` stands.

3. **A silence.** A no-response is `vehicle`, and a suppressed request's no-response `none`, only
   when a receive timeout is judged at the limit in force on one clock (`timeout_at_limit`) on a
   link that forwards pendings. Otherwise the reason names what stays open:
   - the end of the wait, not judged at the limit, may have come early;
   - or the VCI absorbs pendings, and how its wait went on after one it absorbed is not seen
     (H4).

   So H4 is excluded only on a link that forwards pendings.

## Consequences

- On a link without transmit times, which includes every J2534 link today, a positive response
  is `undetermined` rather than `none`. A silence there is `undetermined` rather than `vehicle`.
  So are a refusal for the ECU's state and a positive response to a suppressed request when
  their time is not judged.
- On a link whose VCI absorbs pendings, no silence is `vehicle` or `none`, even with a timeout
  judged at P2. The design's example of an ECU that stops answering shows such a link, and is
  now `undetermined`.
- The log's outcomes become decided where the VCI gives the times a judgement needs: transmit
  times on the VCI's clock, and a link that forwards pendings.
- A late response's settlement (`settle_late_response`) classifies the response as if it came
  in time and judges its time by its own window, so item 2 does not reach it. Whether that
  judgement, and the log's other rules on attribution, timing and settlement, rest on an
  estimate is checked against ADR-375 separately.

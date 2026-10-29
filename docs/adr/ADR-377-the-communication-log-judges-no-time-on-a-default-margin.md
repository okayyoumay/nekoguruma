# ADR-377: The Communication Log Judges No Time on a Default Margin

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** design 8.11 ("Timing margin", `timing`, the example logs), 9.3 (the VCI profile's
timing row), 13.5 ("Hardware profile"); `agent` communication log (`comm_log/timing.rs`:
`Limits`, `judge`, `judge_late_at`, `past_vci_limits`; `comm_log/exchange.rs`:
`NotJudgedReason::NoMargin`; `comm_log.rs`: `VciMargins`, `link_opened`'s `timing_margin`);
`crates/agent/docs/ngr-agent.md`, `crates/ngr-hw-checks/docs/hardware-profile.md`; ADR-357 items
1, 3 and 5 for the timing margin; ADR-362 item 1 (`response_times_judged`); ADR-375 item 5

## Context

The log compares a VCI time with a limit only beyond a margin, which covers the error of the
VCI's times. No API reports that error. ADR-357 gave the agent a built-in timing margin of 2 ms,
used unless a hardware profile gives the VCI's own, and `link_opened` records it with `source`
`default`. No job reads a VCI profile yet, so every link outside the hardware checks is judged on
that default.

ADR-375 rules that the log adds no value it has not measured or been given, and its item 5 named
the default margins as rules to check. The 2 ms default is an estimate standing in for a VCI's
error: a VCI whose times err by more puts a time on the other side of a limit, and the outcome
comes out false either way. The maintainer chose (2026-10-29) that the log judges no time on a
default margin. The T2 timing check already does so (ADR-356): without a margin from the
profile, it records the latency and judges nothing.

## Decision

1. **No default timing margin.** The agent has no timing margin of its own. A link's margin is
   the one a profile gives for its VCI (a hardware profile today, the VCI profile, design 9.3,
   later), and none otherwise.

2. **No margin, no judgement.** Without a margin, nothing bounds the error of the VCI's times.
   So no time of the link is compared with a limit:
   - an exchange's `window` is `not_judged`, with the reason `no_margin` wherever a comparison
     would have given its outcome. A reason the times' own placement gives stands, since no
     margin would let such times be judged: `no_transmit_time`, `other_clock`,
     `before_transmit`, `response_begun`. An end past the limit is a comparison, so it is
     `no_margin`, not `end_after_limit`;
   - a response, start or pending read after its exchange ended is judged neither in time nor
     late;
   - the agent's deadline reading does not show the VCI past its own limits.

   The entry still records the measured times, the response time and the limits in force, so
   that a reader who knows the VCI's error can judge them (ADR-375 item 3). Under ADR-376, such
   a response or silence is `undetermined`.

3. **The record.** `link_opened`'s `timing_margin` gives the margin in force with `source`
   `profile`, or `{"source": "none"}` with no value when none is given. Its
   `response_times_judged` is true only when the link has transmit indications and a margin,
   which narrows ADR-362 item 1. An exchange's `timing.margin_ms` is left out when there is no
   margin.

4. **The other margins.** The drift bound and the clock margin keep their defaults
   (ADR-357), which a loss's settlement uses. Whether they stand under ADR-375 is checked with
   the settlements, separately.

## Consequences

- Until a job reads a VCI profile, no production log judges a time against a limit. Its entries
  record the times, and their outcomes rest on the rows that do not turn on timing. So most
  exchanges are `undetermined` (ADR-376). A hardware run with a profile that gives the timing
  margin judges as before.
- The VCI profile's timing row (design 9.3) is what makes production logs judge times, so
  reading it gains weight.
- Tests that check the timing rows give their runs a profile margin of 2 ms; one test checks
  the run with none.

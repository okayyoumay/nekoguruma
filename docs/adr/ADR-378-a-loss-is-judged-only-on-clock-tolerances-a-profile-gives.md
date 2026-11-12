# ADR-378: A Loss Is Judged Only on Clock Tolerances a Profile Gives

**Date:** 2026-10-29
**Status:** Accepted (the placement it left open settled by ADR-383)
**Affects:** design 8.11 (`vci_loss_settled`, "Drift bound"); `agent` communication log
(`comm_log.rs` `Inner::settle_loss`); `crates/agent/docs/ngr-agent.md`,
`crates/ngr-hw-checks/docs/hardware-profile.md`; ADR-357 (the defaults' use in a settlement);
ADR-373 item 4; ADR-375 item 5

## Context

A loss's settlement (ADR-373 item 4) compares the VCI's first clock reading after the reconnect
with the last time before the loss plus the gap. Its tolerance is twice the clock margin, plus
the reading's round trip, plus the drift bound times the gap. No API reports the drift bound or
the clock margin. ADR-357 gave the agent defaults (100 ppm, 1 ms), used unless a hardware
profile gives the VCI's own, so outside a hardware run every settlement would be judged on them.
Today no settlement reaches the comparison within one agent epoch, but the rule stands for the
day one does.

ADR-375 rules out a value the log has not measured or been given, and its item 5 named the
default margins as rules to check. ADR-377 removed the default timing margin. The maintainer
chose (2026-10-29) to apply the same rule to the settlement, and to check the defaults' other
use separately: they also place a clock's times (design 8.11 "Window", ADR-361).

## Decision

1. **No kept power on a default tolerance.** A settlement compares the reading only on a drift
   bound and a clock margin that a profile gives. When either is the agent's default, the
   comparison is recorded and judges nothing:
   - the gap, the expected value, the tolerance it gives and each value's `source` (`default`)
     stay in `vci_loss_settled`, as for a gap on the boot clock (ADR-374);
   - `power_kept` is `unknown`, and the loss stays `undetermined`;
   - the reason says that no profile gives the VCI's drift bound and clock margin.

2. **The defaults stay for placing times.** `VciMargins` keeps its defaults for the drift bound
   and the clock margin, which the log uses to place a clock's times within an epoch and to open
   a new one. Whether that use rests on an estimate is checked on its own.

3. **Other settlement rules checked and kept.** The review found the other settlements sound:
   - `primitive_ended`: a final response, or a receive timeout judged at the limit, after the
     agent stopped waiting is `system`, since the agent gave up on an exchange that ended. A
     deadline reading that showed the VCI past its own limits, which this end does not refute,
     stays `system`. Each rests on what was read, and on a margin a profile gives (ADR-377).
   - Whether a response spans frames (`SINGLE_FRAME_MAX`, 7 bytes): the agent's links use
     normal addressing (the link refuses extended addressing), where a classic CAN frame carries
     at most 7 bytes as a single frame. On a link that carried fewer, a 7-byte response would be
     taken for one frame, so a start-of-message indication would not count as its start. The
     response would then be judged by its end only, which proves less and never more.

## Consequences

- Until the agent reads a VCI profile, no settlement within one agent epoch judges a kept power.
  None does today, since every reopen starts a new agent epoch.
- A hardware run whose profile gives both values judges as before.
- The log's placement of clock times still rests on the defaults (item 2), so that use is
  checked separately.

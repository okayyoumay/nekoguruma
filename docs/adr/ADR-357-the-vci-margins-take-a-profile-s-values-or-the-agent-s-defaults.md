# ADR-357: The VCI Margins Take a Profile's Values or the Agent's Defaults

**Date:** 2026-10-26
**Status:** Accepted (the default timing margin, in items 1, 3 and 5, superseded by ADR-377: without a margin a profile gives, no time is judged)
**Affects:** `agent` communication log (`comm_log.rs` `VciMargins`, `vci_connected`,
`link_opened`), `agent::Job::with_vci_margins`, `ngr-hw-checks` profile (`profile.rs`
`Vci::drift_bound_ppm`, `Vci::clock_margin_us`, `Vci::margins`) and `GuardedClient`,
`crates/ngr-hw-checks/docs/hardware-profile.md`, `crates/agent/docs/ngr-agent.md`; design 8.11
("Timing margin", "Drift bound"), 9.3, 13.5 ("Hardware profile"); ADR-346, ADR-356

## Context

Design 8.11 judges times only beyond three tolerances of the VCI. Each comes from the agent's
configured default or from the value a hardware profile gives for that VCI:

- a link's timing margin;
- the drift bound between the VCI's clock and the agent's;
- the clock margin, the tolerance of one reading of the VCI's clock.

No API reports any of them. Until now the agent had no default and read no profile, so
`link_opened` and `vci_connected` recorded each with `source` `none`. The hardware profile gave
only `timing_margin_us`, which the T2 timing check reads (ADR-356). Aligning and unwrapping VCI
times needs these values first: telling a wrap from a reset takes the drift bound over the gap
plus the clock margin.

The maintainer decided where the values come from.

## Decision

1. **The defaults are built-in constants of the agent** (`agent::VciMargins`):
   - timing margin 2 ms, since a USB VCI's timestamps and their latency vary by up to about a
     millisecond;
   - drift bound 100 ppm, since a common crystal is within 50 ppm on each side;
   - clock margin 1 ms.
   The agent has no configuration file for them.
2. **A hardware profile may give each value.** Its `[vci]` table gains `drift_bound_ppm` and
   `clock_margin_us` beside `timing_margin_us`.
3. **Each value takes its own source.** A value the profile gives is in force with `source`
   `profile`. One it leaves out takes the default, with `source` `default`.
4. **The harness passes the values in the job, not in the worker client.**
   - `Job::with_vci_margins` gives a job the profile's values. The hardware harness's
     `GuardedClient` holds them from its profile (`GuardedClient::vci_margins`).
   - `WorkerApi` does not change: the values describe the VCI, not a call to the worker.
   - A job never told any takes every default.
5. **The log records each value in its unit, with its source**:
   - `link_opened`'s `timing_margin` as `us`;
   - `vci_connected`'s `drift_bound` as `ppm`;
   - `vci_connected`'s `clock_margin` as `us`.
   A margin below a millisecond, as a profile may give, stays exact.
6. **The VCI profile (design 9.3) is the production source.** Its table gains a timing row that
   declares the same three values for a VCI. The agent reads no VCI profile yet, so a
   production job takes the defaults until that profile is implemented.

## Consequences

- Every log now records the three values in force. A reader can tell an agent default from a
  bench's measured value.
- The log's `timing_margin` field changes from `ms` to `us`. No reader of the log exists outside
  the agent's own tests.
- A production job's margins are the defaults. A VCI whose timer spread exceeds 2 ms, or whose
  clock drifts more than 100 ppm, can be judged wrongly there until its VCI profile gives its
  own values.
- The values are recorded only. The exchange entries do not judge times against them yet; that
  is the work of the timing items that follow.

# ADR-331: Debug Builds Treat `iso22900-mock` as the Simulator

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `agent` (`src/policy.rs`, `docs/ngr-agent.md`), `iso22900-service`
(`tests/ngr_agent_definition_end_to_end.rs`), `diag-frontend` (`examples/end-to-end/routine.star`),
design 8.2.4, ADR-247 items 1 and 2

## Context

ADR-247 lets a debug-build agent send any request only when the worker's VCI identifies itself
as `sim-vci` through `j2534-0404-service`. Every other VCI leaves the job read-only.

Since ADR-299, `iso22900-mock` answers the D-PDU worker's ComPrimitives from the same `sim-ecu`
that `sim-vci` uses. M2's end-to-end definition test runs the example ECU's procedures through
both workers. Through the D-PDU worker its routine procedure was refused, because that procedure
opens the programming session first. So the routine ran only through the J2534 worker, although
the mock answers the same way.

Treating the mock as a simulator widens the trust boundary of ADR-247. The ADR left that to the
maintainer, who decided on 2026-10-18 that a debug-build agent may treat `iso22900-mock` as the
simulator.

## Decision

1. **Two simulated VCIs.** In a debug build, the simulator permission of ADR-247 is granted to
   `sim-vci` as before, and also to `iso22900-mock`. ADR-247's other rules are unchanged:
   - the permission exists only in debug builds;
   - it covers the same instructions (ADR-247 item 4, ADR-250, ADR-259);
   - the policy checks at the same checkpoints (item 3).
2. **How the mock is identified.** The VCI counts as `iso22900-mock` only when all four version
   fields that ADR-247 reads have exactly the mock's values:
   - hardware name `Mock Hardware`;
   - firmware name `Mock Firmware`;
   - vendor name `Mock Vendor`;
   - API software name `iso22900-mock`.

   Prefixes are not enough, and case and spacing must match. `iso22900-service` copies the
   library's version data into these fields unchanged.
3. **One worker only.** `j2534-0404-service` cannot report the mock's API name, because it always
   sets that field to `J2534`. So the mock's identity holds only through the D-PDU worker.

## Consequences

- M2's end-to-end definition test now runs the routine procedure through both workers, with the
  same result.
- A debug agent's write paths can also be exercised through the D-PDU worker without hardware.
- A real D-PDU API library that reported the mock's four names would be treated as the
  simulator in a debug build. As in ADR-247, the library would have to mislead deliberately:
  the names belong to this repository's mock, and debug builds are developer builds. Release
  builds are not affected.
- The same holds for `sim-vci`'s names, which ADR-247 did not record: the vendor and API names
  that ADR-247 item 2 takes as proof that `j2534-0404-service` answered are copied from the
  library by `iso22900-service`. A D-PDU library that reported `j2534-0404`, `J2534` and
  `sim-vci`'s prefixes would earn the simulator permission through the D-PDU worker. Only a
  deliberately misleading library does that, so the risk is the one above.
- Without a configured `sim-ecu`, the mock answers each ComPrimitive with a result item that
  carries no data (ADR-299). A write job then reaches no ECU at all.
- Matching exactly, rather than by prefix as ADR-247 does for `sim-vci`, means a change to the
  mock's names must change `ISO22900_MOCK_VERSION` in `crates/agent/src/policy.rs` too. The
  end-to-end test fails if the two drift apart.

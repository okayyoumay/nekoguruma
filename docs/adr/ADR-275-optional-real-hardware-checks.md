# ADR-275: Real-Hardware Checks Are Optional, Opt-In Tests Bounded by a Target Class

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** design 13.5 (new), design 13.4, design 17 ("Items to Confirm Early"), VCI profile (9.3), test layout and `.config/nextest.toml` when the checks are implemented

## Context

CI verifies everything against `sim-vci` and `sim-ecu` (design 13.4), and real VCIs were planned
only for the final field validation. Some behaviour can only be seen on real hardware: how a
vendor library implements J2534 or the D-PDU API, the `unsigned long` width of a Linux library,
real bus timing and response-pending chains, and the per-vendor items design 17 lists. Finding
these only at the end would push vendor quirks into the last milestone.

The maintainer asked for real-hardware checks as optional items throughout the milestones.
Optional means: they add evidence when hardware is at hand, but no change and no CI run may
depend on hardware being present. Real ECUs and vehicles can be damaged by writes, so the checks
also need a limit that does not depend on each check's author being careful.

## Decision

1. **Optional and outside CI.** Hardware checks never run in CI and are never a required check.
   A milestone's exit criteria do not depend on them unless a milestone names them explicitly.
2. **Ordinary tests, opted into explicitly.** A hardware check is a Rust test named `hw_...`,
   marked `#[ignore]` with a reason, and run through a dedicated nextest profile (`hardware`)
   that runs only those tests, one at a time, without retries. Without a hardware profile
   (`NGR_HW_PROFILE`) a hardware check fails instead of passing: running it was a request.
   A separate runner binary was considered and rejected: tests reuse the existing harness,
   assertions and per-crate placement, and nextest already selects ignored tests by name.
3. **Four target classes.** T0 loopback (no ECU), T1 stand-in ECU (`sim-ecu` behind a real CAN
   interface), T2 bench ECU, T3 vehicle. Each check names the least real class it needs; the
   hardware profile declares the class of the attached setup and any extra operations its owner
   allows.
4. **An operation guard, not care, enforces the class.** Every request passes a guard that
   checks the UDS service (and sub-function where it matters) against the class's list before
   anything is sent. T3 uses an allow-list of read-only services. Reprogramming on T2 needs the
   profile to declare the ECU expendable and the design 8.9.1 checks to pass.
5. **A stand-in ECU on a real bus.** `sim-ecu` gets a second front end that talks over a
   SocketCAN interface, so writes, reprogramming and fault injection (including physical ones:
   unplugging the VCI, cutting power through a host-controlled switch) can be checked with a real
   VCI and library without risking an ECU.
6. **Findings go into permanent records.** Each run writes a report outside the repository (it
   can contain serial numbers and VINs). A vendor behaviour becomes a VCI profile item (9.3), an
   answer to a design 17 item is written there, and a defect is fixed or tracked.

## Consequences

- Real-hardware evidence can arrive from the first milestone on, without making CI or
  contributors depend on owning a VCI.
- The checks are only as current as the last time someone ran them; nothing flags a hardware
  check that has started failing. Reports carry the date and versions so a reader can judge this.
- T1 needs new code (the SocketCAN front end of `sim-ecu`), and the guard, the profile parser and
  the report writer are new test infrastructure.
- The device list in design 13.5 is a purchase the maintainer decides; until hardware exists the
  checks are written and listed but not run.

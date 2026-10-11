# ADR-275: Real-Hardware Checks Are Optional, Opt-In Tests Bounded by a Target Class

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** design 13.5 (new), design 13.4, design 17 ("Items to Confirm Early"), VCI profile (9.3), `sim-ecu` (SocketCAN front end), test layout and `.config/nextest.toml` when the checks are implemented

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
3. **Four target classes, not ordered.** T0 loopback (no ECU), T1 stand-in ECU (`sim-ecu`
   behind a real CAN interface), T2 bench ECU, T3 vehicle. They differ in topology and in what may
   be sent, so none stands in for another: each check declares the set of classes it can run on,
   and the hardware profile declares the class of the attached setup. A profile can only narrow
   its class, except that a T2 profile may allow reprogramming of an ECU it declares expendable
   and T1 and T2 profiles may name power-switch commands and the specific hardware controls
   their wiring is safe for. A T3 profile can add nothing.
4. **A guarded handle, not care, enforces the class.** A check reaches the VCI only through a
   handle the harness creates, and the guard behind it sees every operation: UDS requests
   (service and, where it matters, sub-function; T3 uses an allow-list of read-only services),
   raw frames and periodic messages, and hardware controls such as programming voltage, pin
   changes and vendor IOCTLs, which are refused on every class unless a T1 or T2 profile lists
   the control; on T2 a listed control also has to be part of an authorized job, so a profile
   entry alone never allows one on a real ECU. The harness creates the profile's one link, and
   checks cannot create others or change its physical layer. Jobs run through the agent pass
   the same guard: the harness hands the job runner a worker client that wraps the real one. Anything not allowed fails the check before it reaches the device. On T2, writes
   and reprogramming run as jobs through the agent, so the existing safeguards, preconditions
   and authorization (design 5.5, 5.6, 8.9, 8.9.1, section 6) all apply and the class guard is
   a ceiling on top of them. A T2 write check exists only once all of section 6's controls for
   its job type do; before that, writes are checked on T1 only.
5. **A stand-in ECU on a real bus.** `sim-ecu` gets a second front end that talks over a
   SocketCAN interface, so writes, reprogramming and fault injection (including physical ones:
   unplugging the VCI, cutting power through a host-controlled switch) can be checked with a real
   VCI and library without risking an ECU.
6. **Cleanup on every exit.** A check registers its cleanup (default session, channel closed,
   power off where the profile can switch it) before its first request to an ECU, and it runs
   on pass, failure and panic. When the process or host dies, the ECU's own session timeout
   returns it to the default session, and the operator follows design 5.6 after an interrupted
   write.
7. **Findings go into permanent records.** Each run writes a report with its date and time to
   a directory readable only by the running user, outside the repository, with VINs and serial
   numbers masked unless the profile asks for them. A vendor behaviour becomes a VCI profile item (9.3), an
   answer to a design 17 item is written there, and a defect is fixed or tracked.

## Consequences

- Real-hardware evidence can arrive from the first milestone on, without making CI or
  contributors depend on owning a VCI.
- The checks are only as current as the last time someone ran them; nothing flags a hardware
  check that has started failing. Reports carry the date and versions so a reader can judge this.
- T1 needs new code (the SocketCAN front end of `sim-ecu`), and the guard, the profile parser and
  the report writer are new test infrastructure.
- The guarded handle can only stop what goes through it: Rust cannot keep a test from linking
  the worker client or library itself, so that rule rests on review of every `hw_` test.
- The device list in design 13.5 is a purchase the maintainer decides; until hardware exists the
  checks are written and listed but not run.

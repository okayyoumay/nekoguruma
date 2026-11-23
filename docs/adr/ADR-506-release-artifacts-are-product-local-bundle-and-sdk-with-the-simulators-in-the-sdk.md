# ADR-506: Release Artifacts Are the Product, the Local-Profile Bundle and the SDK, With the Simulators in the SDK

**Date:** 2026-11-23
**Status:** Accepted
**Affects:** `docs/release-scope.md`, design 9.8, 13; `crates/sim-vci`, `crates/sim-ecu`, `crates/iso22900-mock`, `crates/j2534-0404-mock`

## Context

The design document decides piece by piece what is bundled (the reference UI, design 9.6; the
local profile's PostgreSQL and RustFS, design 12, 13, ADR-491), what is provided to extension
authors (design 9.8), what is not shipped (vendor libraries, design 16.2) and what is the
framework user's or the operator's responsibility (design 16.1, ADR-229, ADR-237). No document
gathers these into one statement of what a release contains, and the repository has no release
workflow yet, so nothing says which crates become release artifacts.

The question is sharpest for the crates that stand in for hardware. `sim-vci` is a cdylib that
exports the J2534 PassThru API and serves the simulated ECUs of `sim-ecu`; `iso22900-mock` and
`j2534-0404-mock` are cdylibs that export the D-PDU API and J2534 for the repository's tests.
Design 9.8 promises extension authors a conformance test suite on a mock VCI and a vehicle
simulator, which needs one of them. On a production device, though, a simulated library is
only a source of confusion: discovery would list it as a VCI, and a release-build agent sends
it no state-changing request anyway (ADR-247, ADR-331).

## Decision

1. A release consists of three groups of artifacts:
   - the **product**: `ngr-server` with the reference UI embedded, `ngr-agent` with its
     installation files from `packaging/`, and the worker services `j2534-0404-service` and
     `iso22900-service`, one binary per worker ABI (design 7.3);
   - the **local-profile bundle**: PostgreSQL and the pinned RustFS (ADR-491, ADR-492);
   - the **SDK** for extension authors: everything design 9.8 lists.
2. `sim-vci`, with the simulated ECUs of `sim-ecu` it serves, ships in the SDK as the mock VCI
   and vehicle simulator of design 9.8's conformance test suite. It is not part of the product,
   and the product's installation does not register it as a VCI.
3. `iso22900-mock` and `j2534-0404-mock` are test doubles for the repository's own tests and are
   not released. The SDK needs no D-PDU simulator: what extension authors write (IR, vehicle
   definitions, VCI profile data) does not depend on which worker kind runs it, because the agent
   treats every worker alike (design principle 6), so one simulated VCI exercises it fully.
4. `docs/release-scope.md` is the summary of what a release includes, leaves out and leaves to
   the framework user or the operator. It cites the design sections and ADRs that decide each
   line; those stay authoritative, and a change to them updates the summary in the same pull
   request.

## Consequences

- The conformance suite covers the J2534 worker path only. A D-PDU-specific behaviour that leaks
  past the worker interface would not show in it; such a leak is a worker bug under design
  principle 6, found by the repository's own tests with `iso22900-mock`.
- The form of each group (archive layout, installers, the npm package's name, which worker ABIs
  the SDK's `sim-vci` is built for) is decided when the release workflow is written.
- A release-scope change now has two places to update: the deciding section or ADR, and the
  summary.

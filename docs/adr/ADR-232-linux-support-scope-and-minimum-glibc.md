# ADR-232: Linux Support Scope and Minimum glibc 2.17

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** design 2 (Assumptions), 7.1.2, 7.3, 12.1, 17; `.github/workflows/ci.yml` (`worker-linux`)

## Context

Design 17 P3 left open how far Linux is a supported platform. The workers were already built
for four Linux targets (x86_64, i686, aarch64, armhf), but the builds did not pin a glibc
version, although design 12.1 said they did. Whether vendors ship Linux J2534 or D-PDU API
libraries, and for which architectures, is still unconfirmed (design 17, items to confirm per
vendor), so a narrow scope would rule out platforms before anyone knows whether they are needed.
The ABI interpretation for aarch64 and armhf is inferred, not backed by the standards (7.1.2).

The minimum glibc decides which distributions a worker binary runs on. As of 2026-10, the
oldest distributions still under any vendor support (paid extended support included) are
RHEL 7 (glibc 2.17, extended life cycle support until 2029-05), SLES 12 SP5 and Ubuntu 18.04;
the oldest under free standard support is RHEL 8 (glibc 2.28). glibc 2.17 is also the lowest
version the Rust standard library supports on these targets, so no lower floor is possible.

## Decision

1. **Cover as much as current support allows.** Linux is a supported platform on the two host
   architectures design 7.3 defines, x86_64 and aarch64, and all four worker ABIs are built and
   shipped: x86_64 and aarch64 for 64-bit vendor libraries, and i686 and armhf for 32-bit
   vendor libraries on those hosts. A 32-bit-only host is not a supported host.
2. **Minimum glibc 2.17.** The `worker-linux` release builds link against glibc 2.17 through
   `cargo-zigbuild`'s target suffix (`<triple>.2.17`), so one binary per target runs on every
   glibc-based distribution from RHEL 7 on whose kernel meets the Rust standard library's
   Linux floor (3.2; RHEL 7 ships 3.10). Distributions built on another C library, such as
   musl, are not covered. Raising the floor is a new decision, taken when a dependency or
   Rust itself requires it.
3. **The level of backing is reported, not hidden.** x86_64 and i686 use this project's own
   definition (7.1.2); aarch64 and armhf stay "inferred" until a vendor library is confirmed on
   real hardware. The distinction is reported through `capabilities` as 7.1.2 already
   requires, so the scope stays wide without overstating what has been verified. The 32-bit
   workers still need the host's i386 or armhf glibc, which the agent detects and reports
   (7.3) rather than declaring as a package dependency (12.1).

## Consequences

- Design 17 P3 is resolved and removed from the undecided items.
- A dependency that needs a glibc symbol newer than 2.17 now fails the `worker-linux` link on
  main instead of producing a binary that fails to load on older distributions.
- Distribution-specific packaging (RPM, deb) is not decided here; the binaries themselves have
  no distribution dependency beyond glibc.

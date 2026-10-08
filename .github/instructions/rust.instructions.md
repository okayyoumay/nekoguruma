---
applyTo: "**/*.rs,**/*.proto,**/*.h,**/Cargo.toml,**/build.rs,docs/worker-crates.md,.github/workflows/**"
---

# Review guidelines: Rust code, proto and FFI

Flag as P1:

- **`unsafe` placement**: FFI calls belong in the `*-sys` crates and the safe wrappers
  (`iso22900`, `j2534-0404`). Flag new `unsafe` in a crate with no FFI role (server, agent,
  diagnostic-logic and shared crates), and any new direct call into a vendor D-PDU API or J2534
  library from a `*-service` crate: an operation the wrapper lacks is added to the wrapper
  first. `unsafe` is expected in the FFI mocks (`iso22900-mock`, `j2534-0404-mock`) and
  `sim-vci`, which export a C ABI, in service code and tests that look up a mock library's own
  test-control symbols, and in narrow operating-system API calls (for example the Windows Shell
  or Event Log); there, flag only unsound code.
- **Generated code out of step with its source**, in either direction:
  - Proto: a change to `crates/vci-service-interface/src/bindings/vci.service.rs` without the
    matching `service.proto` change (hand edit), or a `service.proto` change without the
    regenerated bindings in the same PR. Proto regeneration is never deferred.
  - FFI hand edit: a change to `crates/*-sys/src/bindings/*.rs` without a header change. No
    exception.
  - FFI missing target: a target added to the target-ABI list in `docs/worker-crates.md` (or a
    CI target list) without its committed `src/bindings/{target}.rs` in every `*-sys` crate.
  - FFI stale bindings: a header change without regenerated bindings. Accepted only when the
    same PR records the deferred regeneration as a backlog item in `work/`; a note in the PR
    description is not enough.
- **Warnings silenced the wrong way**: `#[allow(...)]` for code that is unused only until later
  work lands. Use `#[expect(..., reason = "...")]`, which fails once the code is used.
- **Out-of-scope targets**: Windows workers target `*-pc-windows-gnullvm` only (ADR-227). An MSVC
  worker target or MSVC-only build path is P1.
- **Interrupted-transfer recovery that departs from ADR-229**, in the framework and the bundled
  reference recovery:
  - continuing an interrupted flash transfer from a later or the last journaled block instead of
    redoing it from the erase, or from the RequestDownload when the procedure does not erase
    (items 1 and 3);
  - an ECUReset sent before the VIN and the ECU hardware identity have matched and the declared
    safety preconditions hold (item 2, step 2);
  - an erase before the identity, state and precondition checks that follow the
    default-session confirmation;
  - placing the interruption without the journal's write-ahead intent markers, so a lost
    response could start an automatic restart that the procedure's recovery-required point
    forbids.

  An ECU-specific recovery written as the framework user's own procedure may continue from a
  later address (item 4). Flag it only if it bypasses the journal, the state check before
  resuming or the interruptibility attributes.

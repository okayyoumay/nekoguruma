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
- **Interrupted transfers resumed mid-download**: code that resumes an interrupted flash
  transfer (agent or worker crash, VCI disconnect, power loss) from a later block, or from the
  last journaled block, instead of redoing it from the erase and RequestDownload (ADR-229
  item 1). Also flag a restart that skips ADR-229's order (teardown, then the identity and
  precondition checks, then erase), or that ignores the journal's write-ahead intent markers
  when it decides where the interruption happened, so a lost response could lead to an
  automatic restart that the procedure's recovery-required point forbids.

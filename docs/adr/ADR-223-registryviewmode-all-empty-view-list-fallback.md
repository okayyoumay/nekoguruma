# ADR-223: `RegistryViewMode::All` Falls Back to `Native` When No Architecture-Specific View Exists

**Date:** 2026-09-09
**Status:** Accepted
**Affects:** `j2534-0404-registry/src/lib.rs`, `iso22900-registry/src/lib.rs`

## Context

`j2534-0404-registry` and `iso22900-registry` each define an architecture
enumeration (`LibraryArch`/`RegistryViewMode`) via a `define_arch!` macro,
generating `W32`/`W64` variants only under `#[cfg(windows)]
#[cfg(target_arch = "x86_64")]`. `get_view_mode_list()` (macro-generated)
returns those two variants on x86_64 Windows and an empty slice on every
other target (32-bit Windows, ARM64 Windows, or any non-Windows target).

`enumerate_j2534_devices`/`enumerate_pdu_libraries`, when called with
`RegistryViewMode::All`, fold over `get_view_mode_list()` to query every
architecture-specific view and combine the results:

```rust
if mode == RegistryViewMode::All {
    get_view_mode_list().iter().try_fold(Vec::new(), |mut acc, &view_mode| {
        match enumerate_j2534_devices(view_mode) { /* recurse per view, append */ }
    })
}
```

When the view list is empty, this fold runs zero iterations and returns
`Ok(Vec::new())` — the actual per-view enumeration logic
(`enumerate_devices_impl` for `j2534-0404-registry`;
`root_description_file_path` + RDF parsing for `iso22900-registry`) is
never invoked at all, even though calling it directly via
`RegistryViewMode::Native` works correctly on every target: `Native`
resolves to the platform's one relevant view (Windows: the process's
default/redirected registry view; `iso22900-registry` on non-Windows: a
real read of `/etc/pdu_api_root.xml`).

`vci-service-manager`'s `discover_j2534_0404_libraries`/
`discover_iso22900_libraries` always call `enumerate_libraries(...All)`.
The practical effect: on any 32-bit Windows build, any ARM64 Windows
build, or any non-Windows build, `vci-service-manager`'s library listing
silently omits every registry/RDF-discovered library — only
`config.toml`-configured `library_path` overrides (merged separately,
unaffected) still appear. For `iso22900-registry` specifically this is a
real regression: its non-Windows `/etc/pdu_api_root.xml` discovery is a
genuinely working mechanism that this bug disables outright.
`j2534-0404-registry` has no non-Windows discovery at all (its
`enumerate_devices_impl` returns `Err(RegistryUnsupported)` there), so the
same code shape is comparatively inert on non-Windows for that crate, but
still broken on 32-bit/ARM64 Windows.

This was found and confirmed by direct code reading plus a macro-expansion
repro (cfg-flag substitution), and recorded in a now-deleted scratch memo
(`vci-service-manager/docs/LIBRARY_ARCH_DISCOVERY_ISSUES.md` item 2). That
memo's own draft fix — an early return to `enumerate(Native)` whenever the
view list is empty — was rejected during design review: `All`'s existing
per-view error policy treats a view-specific `NotFound` as "skip it, keep
combining" but propagates any other error. An early return bypasses that
distinction entirely, changing `All`'s behavior when the single `Native`
view is itself absent (e.g. no RDF file on Linux) from "no devices found"
to "propagate an error" — a second, avoidable behavior change bundled into
the fix.

A more complete fix was also considered: real runtime WOW64 detection
(e.g. `IsWow64Process`/`IsWow64Process2` via `windows-sys`, already a
`cfg(windows)` dependency elsewhere in the workspace for other Win32 APIs)
so that a 32-bit process running under WOW64 on 64-bit Windows could
explicitly query the 64-bit registry view too, truly fulfilling `All`'s
doc comment ("queries every architecture-specific view and combines the
results"). This was deferred — see Consequences.

ADR-036's "observable behaviour unchanged" claim for `enumerate_libraries`
held only for x86_64 Windows builds; this ADR corrects that scope
implicitly (ADR-036 is not superseded — its own decision, the config-merge
mechanism, is unaffected).

## Decision

1. Inside the existing `try_fold`, when `get_view_mode_list()` is empty,
   fold over `[RegistryViewMode::Native]` instead of the empty slice — not
   as a separate early-return branch. This keeps `All`'s per-view error
   policy (`NotFound` skipped, other errors propagated) intact for the
   substituted view exactly as it already applies to every other view.
2. `RegistryViewMode::All`'s contract is redefined as: "every
   architecture-specific view this build can enumerate; the process's
   default view when the build enumerates none" (previously undocumented
   for the empty-list case, and silently wrong in practice).
3. `j2534-0404-registry::enumerate_libraries` logs the resulting
   `RegistryUnsupported` (now reachable via `All` on non-Windows, where it
   previously returned `Ok(Vec::new())` unconditionally) at `debug!`
   rather than `warn!` — it is the documented, expected outcome on that
   platform, not a failure.
4. Both `j2534-0404-registry` and `iso22900-registry` receive the
   identical change; they are an already-enumerated duplicate pair (their
   `define_arch!` macros and `All`-folding logic are structurally
   identical), so this is a same-PR propagation edit, not two independent
   fixes.
5. Runtime WOW64/ARM64-secondary-view detection is explicitly deferred,
   not implemented here — see Consequences for the accepted residual and
   the tracked follow-up.

## Consequences

- Registry/RDF discovery works again on every build this bug silently
  broke: 32-bit Windows, ARM64 Windows, and all non-Windows targets
  (concretely restoring `iso22900-registry`'s `/etc/pdu_api_root.xml`
  discovery on Linux).
- **Accepted residual (P2, tracked in
  `vci-service-manager/docs/implementation-notes.md`'s Prioritized
  Backlog):** a 32-bit process running as WOW64 on 64-bit Windows still
  only sees its own default (32-bit, redirected) registry view via the
  `Native` fallback — it does not additionally query the 64-bit view the
  way a genuinely complete `All` would. The same asymmetry applies in
  reverse to an ARM64 Windows build, which sees only its native view, not
  an x86 WOW64 view. Closing this needs runtime OS-level view detection
  (e.g. `IsWow64Process2` via `windows-sys`'s `Win32_System_Threading`
  feature, with a dynamic `GetProcAddress` fallback since that API is
  absent before Windows 10 1511 and `windows-sys` links it statically) and
  turning `get_view_mode_list()`/`get_native_view_mode()`/
  `LibraryArch::get_native()` from compile-time into runtime-conditional
  logic — a larger change better done once, in the consolidated
  `RegistryViewMode`/`LibraryArch` machinery a separate, still-undecided
  refactor (moving this duplicated machinery into `vci-service-config`)
  would produce. Doing runtime detection now would mean implementing it
  twice (once per registry crate) and then relocating it; deferred until
  that consolidation happens or the residual's impact grows large enough
  to warrant it standalone.
- `RegistryViewMode::All` on non-Windows `j2534-0404-registry` now returns
  `Err(RegistryUnsupported)` instead of `Ok(Vec::new())`. Only
  `enumerate_libraries` (which already documents treating any enumeration
  error as "no registry-discovered devices") is affected in practice; any
  other caller of `enumerate_j2534_devices(RegistryViewMode::All)`
  expecting a bare empty `Ok` on non-Windows must now handle this `Err`
  the same way `Native` mode already required.
- No change to `library_path` resolution priority, the TOML schema, or
  `LibraryArch`'s string-form/ID semantics — this ADR touches only which
  views `All` queries, not how a discovered device's `arch`/name is
  represented.

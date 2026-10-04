# ADR-035: Consolidate the `find_library_path` Fallback into `resolve_library_path` on Each Registry Crate

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `iso22900-registry/src/lib.rs`, `j2534-0404-registry/src/lib.rs`, `iso22900-service/src/service/rpc.rs`, `j2534-0404-service/src/service.rs`

## Context

ADR-034 gave `iso22900-registry` and `j2534-0404-registry` each a
`find_library_path(arch, library_name)` wrapper around
`vci-service-config::find_library_path`, but the two-step priority — try
the configured override first, fall back to hardware/RDF auto-discovery
otherwise — stayed in the `*-service` crates' `VciServer::new()`
implementations:

```rust
// iso22900-service/src/service/rpc.rs (before)
let library_path = match iso22900_registry::find_library_path(arch, &startup_config.library_name) {
    Some(path) => path,
    None => find_pdu_libraries(&startup_config.library_name).map_err(map_registry_error)?.library_path,
};

// j2534-0404-service/src/service.rs (before)
let library_path = match j2534_0404_registry::find_library_path(arch, &startup_config.library_name) {
    Some(path) => path,
    None => j2534_0404_registry::find_j2534_device(&startup_config.library_name)?.library_path,
};
```

Both call sites duplicated the same `match`/fallback shape, and each
registry crate already owns both halves of the priority (`find_library_path`
for the config override, `find_pdu_libraries`/`find_j2534_device` for
platform auto-discovery) — the `*-service` crates had no reason to know
that priority exists, only that "resolve this library name to a path" is a
single question with a single answer per API.

## Decision

Add one consolidated function per registry crate that owns the full
priority internally:

```rust
// iso22900-registry
pub fn resolve_library_path(arch: Option<&str>, library_name: &str) -> Result<PathBuf, RegistryError> {
    if let Some(path) = find_library_path(arch, library_name) {
        return Ok(path);
    }
    Ok(find_pdu_libraries(&library_name)?.library_path)
}

// j2534-0404-registry
pub fn resolve_library_path(arch: Option<&str>, library_name: &str) -> Result<PathBuf, RegistryError> {
    if let Some(path) = find_library_path(arch, library_name) {
        return Ok(path);
    }
    Ok(find_j2534_device(library_name)?.library_path)
}
```

`Iso22900Service::new()` and `J2534Service::new()` now call only
`iso22900_registry::resolve_library_path(...)` /
`j2534_0404_registry::resolve_library_path(...)` respectively, each still
mapped to their existing error type at the call site
(`.map_err(map_registry_error)?` for iso22900, `?` for j2534-0404, both
unchanged from before). `find_library_path`, `find_pdu_libraries`, and
`find_j2534_device` remain public — `resolve_library_path` is additive, not
a replacement for callers that need one half of the priority on its own
(e.g. `vci-service-manager`'s discovery, which still calls
`list_configured_libraries()` and the enumerate/find functions directly).

## Consequences

- The config-override-then-auto-discovery priority for a library name now
  has exactly one implementation per API, inside the registry crate that
  already owns both of its inputs, instead of being re-implemented at each
  `*-service` call site.
- `iso22900-service/src/service/rpc.rs` no longer imports
  `iso22900_registry::find_pdu_libraries` directly; `j2534-0404-service`'s
  call site no longer references `find_j2534_device` directly. Both files
  keep their existing error-mapping call convention unchanged.
- No behavior change: `resolve_library_path` performs the identical
  priority check the two call sites already ran, so the same
  `library_path` is resolved for the same inputs before and after this
  change.

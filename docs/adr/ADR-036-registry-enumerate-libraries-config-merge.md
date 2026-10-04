# ADR-036: `enumerate_libraries` — Merge Config-File Libraries into Registry Enumeration

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `vci-service-config/src/lib.rs`, `iso22900-registry/src/lib.rs`, `j2534-0404-registry/src/lib.rs`, `j2534-0404-registry/Cargo.toml`, `vci-service-manager/src/main.rs`

## Context

ADR-034 gave `vci-service-manager` visibility into config-only libraries
(`library_path` set, no matching RDF/registry entry), but only by exposing
their *names* (`list_configured_libraries()`). `vci-service-manager`'s
`discover_iso22900_libraries()` / `discover_j2534_0404_libraries()` called
this alongside `enumerate_pdu_libraries()` / `enumerate_j2534_devices()` and
flattened both results down to `Vec<String>` immediately — the richer
`PduLibraryInfo` / `J2534DeviceInfo` fields (`library_path`, `description`,
`arch`, ...) that hardware-discovered libraries already carry were never
available for config-only libraries at all, and callers had no way to tell
which source (registry vs. config) a given name came from, or whether a
hardware-discovered library also had a config override in effect.

`vci-service-config::list_configured_libraries` itself only ever collected
names, discarding the `library_path` it had just read off each entry to
determine inclusion — there was no way to ask "what path is configured for
this name" without also knowing which api/arch table it lived in.

## Decision

Add `list_configured_library_paths(api_name) -> Vec<(String, PathBuf)>` to
`vci-service-config`: the same api-level + arch-level `libs` traversal as
`list_configured_libraries`, but keeping each entry's `library_path`.
`list_configured_libraries` is reimplemented on top of it (`.map(|(name,
_)| name)`), so the two never drift. Like `list_configured_libraries`, this
is a display/discovery enumeration, not a resolution: it does not apply
`find_library_path`'s arch-then-api priority for one specific runtime arch —
if the same name is configured at multiple levels, the
lexicographically-first path wins (sorted-and-`dedup_by`-name).

Add one `enumerate_libraries(mode) -> Vec<PduLibraryInfo>` /
`Vec<J2534DeviceInfo>` function per registry crate that merges hardware
auto-discovery with `list_configured_library_paths`:

```rust
// iso22900-registry (j2534-0404-registry mirrors this with J2534DeviceInfo/device_name)
pub fn enumerate_libraries(mode: RegistryViewMode) -> Vec<PduLibraryInfo> {
    let mut libraries = match enumerate_pdu_libraries(mode) {
        Ok(libs) => libs,
        Err(e) => { warn!(...); Vec::new() }
    };
    for (name, path) in vci_service_config::list_configured_library_paths("iso22900") {
        if let Some(existing) = libraries.iter_mut().find(|lib| lib.short_name == name) {
            existing.library_path = path;
            existing.source = LibrarySource::Both;
        } else {
            libraries.push(PduLibraryInfo { short_name: name, library_path: path, source: LibrarySource::Config, /* other fields None/native-arch */ });
        }
    }
    libraries.sort_by(|a, b| a.short_name.cmp(&b.short_name));
    libraries
}
```

A new `source: LibrarySource` field (`Registry` / `Config` / `Both`) is
added to `PduLibraryInfo` and `J2534DeviceInfo` so callers can tell which
source(s) produced an entry. When a name is discovered by both sources, the
existing (hardware-discovered) entry is kept — preserving its
`description`/`supplier_name`/`module_description_path`/
`cable_description_path`/`arch` — but its `library_path` is overwritten with
the configured override, matching the priority `resolve_library_path`
already applies at service startup (ADR-035): the enumeration and the actual
runtime resolution never disagree about which path wins for a name present
in both places.

`enumerate_libraries` is infallible (`Vec<T>`, not `Result`): auto-discovery
errors (missing/malformed RDF, `RegistryUnsupported` on non-Windows, etc.)
are logged via `tracing::warn!` and treated as "no hardware-discovered
libraries," so a config-only library is always listed regardless of
platform or RDF/registry health. This matches how `vci-service-manager`'s
prior hand-rolled merge already behaved (`if let Ok(...) = enumerate(...)`
silently dropping errors) — nothing regresses, and config-only libraries are
now *more* robust than before, since even a hard `RegistryError` (not just
`NotFound`) no longer hides them.

`vci-service-manager`'s `discover_iso22900_libraries()` /
`discover_j2534_0404_libraries()` now call `enumerate_libraries(...All)`
once each and map to names, instead of separately calling
`enumerate_pdu_libraries`/`enumerate_j2534_devices` and
`list_configured_libraries` and merging the two lists by hand. Observable
behavior (the final sorted, deduplicated `Vec<String>` of names) is
unchanged.

`enumerate_pdu_libraries`, `enumerate_j2534_devices`,
`list_configured_libraries`, `find_library_path`, `find_pdu_libraries`,
`find_j2534_device`, and `resolve_library_path` are all unchanged and remain
public — `enumerate_libraries` is additive, for callers that want the
full merged picture in one call.

## Consequences

- `vci-service-manager` (and any future caller) can get one call's worth of
  fully-merged library info — including config-only entries' resolved
  `library_path` and which source(s) produced each entry — instead of
  re-deriving the merge from three separate lower-level functions.
- Config-only libraries are now visible even when hardware auto-discovery
  hard-fails (not just when it cleanly reports "not found"), a strict
  improvement over the prior hand-rolled merge in `vci-service-manager`.
- `j2534-0404-registry` gains a `tempfile` dev-dependency (mirroring
  `iso22900-registry`, which already had one) to test `enumerate_libraries`
  against a controlled `config.toml` via a runtime `VCI_CONFIG_PATH`
  override, in its own integration-test binary (same rationale as
  `vci-service-config/tests/runtime_config_path_override.rs`: avoids racing
  other tests over the process-global environment variable).
- No change to `library_path` resolution priority or the TOML schema —
  `enumerate_libraries` and `list_configured_library_paths` are pure
  enumerations over the same data `resolve_library_path`/`find_library_path`
  already read.

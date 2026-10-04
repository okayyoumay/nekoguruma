# ADR-034: Wire `library_path` Lookup into `iso22900-registry` / `j2534-0404-registry`; Add `list_configured_libraries` for `vci-service-manager` Discovery

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `vci-service-config/src/lib.rs`, `iso22900-registry/src/lib.rs`, `iso22900-registry/Cargo.toml`, `j2534-0404-registry/src/lib.rs`, `j2534-0404-registry/Cargo.toml`, `iso22900-service/src/service/rpc.rs`, `j2534-0404-service/src/service.rs`, `vci-service-manager/src/main.rs`, `vci-service-manager/Cargo.toml`

## Context

ADR-033 extracted `vci-service-launcher`'s config-file loading
(`find_logging_config`, `find_library_path`, and the underlying
config-root/`VCI_CONFIG_PATH` resolution) into a standalone crate,
`vci-service-config`, with no dependency on `tonic`/`tokio`/
`vci-service-interface`. `vci-service-launcher` re-exports it unchanged
(`pub use vci_service_config as config;`), so `iso22900-service` and
`j2534-0404-service` kept calling
`vci_service_launcher::config::find_library_path("iso22900"|"j2534-0404",
arch, library_name)` exactly as before. That ADR explicitly deferred two
things: actually wiring `vci-service-manager` to this crate, and any new
capability beyond what already existed.

`vci-service-manager` discovers installed libraries for its `/vci-libs`
listing by calling `iso22900-registry::enumerate_pdu_libraries()` and
`j2534-0404-registry::enumerate_j2534_devices()` — pure hardware/RDF
auto-discovery. A library defined only via a `library_path` config entry
(no RDF or Windows-registry entry — the scenario ADR-029/030 exist for,
e.g. a non-Windows `.so` under test) is invisible to `vci-service-manager`,
even though `iso22900-service`/`j2534-0404-service` would successfully
resolve and start it. Wiring `vci-service-manager` directly to
`vci-service-config` (as ADR-033 sketched) would require it to duplicate
the api-name string (`"iso22900"` / `"j2534-0404"`) and priority logic
already encapsulated once per API in the `*-service` crates' call sites —
and `vci-service-config` has no per-library-name *enumeration* function to
begin with, only a lookup by known name (`find_library_path`).

## Decision

Add `list_configured_libraries(api_name: &str) -> Vec<String>` to
`vci-service-config`: enumerates every library name with a `library_path`
set under `config.apis.<api>` (both the api-level `libs` table and every
arch-level `libs` table), sorted and deduplicated. This is a pure addition
to `vci-service-config`'s existing schema and priority model — no change to
`find_logging_config` or `find_library_path`.

`iso22900-registry` and `j2534-0404-registry` each add a direct dependency
on `vci-service-config` and expose two thin wrappers scoped to their own
API name:

```rust
// iso22900-registry
pub fn find_library_path(arch: Option<&str>, library_name: &str) -> Option<PathBuf> {
    vci_service_config::find_library_path("iso22900", arch, library_name)
}
pub fn list_configured_libraries() -> Vec<String> {
    vci_service_config::list_configured_libraries("iso22900")
}
// j2534-0404-registry: identical shape, api name "j2534-0404"
```

`iso22900-service::Iso22900Service::new()` and
`j2534-0404-service::J2534Service::new()` switch from
`vci_service_launcher::config::find_library_path("iso22900"|"j2534-0404",
...)` to `iso22900_registry::find_library_path(...)` /
`j2534_0404_registry::find_library_path(...)` respectively — the api-name
string is no longer passed at every call site, since each registry crate
already only ever means its own API. `vci_service_launcher::config` keeps
re-exporting `find_library_path` from ADR-033 for any other caller, but the
two services that motivated it no longer use that path.

`vci-service-manager`'s `discover_iso22900_libraries()` /
`discover_j2534_0404_libraries()` call the new
`list_configured_libraries()` wrapper and merge the result into the
existing hardware-auto-discovery list (sort + dedup, same pattern already
used to combine per-architecture registry views). `vci-service-manager`
also gains the same five `config-root-*` re-export features as
`vci-service-launcher` (pointed at `vci-service-config` directly, since the
manager has no `vci-service-launcher` dependency to route through), so a
deployment can pass one `--features config-root-*` flag to keep the manager
and the services it spawns reading the same `config.toml`.

This relies on Cargo's feature unification: `vci-service-config` is reached
from multiple paths in the same build (`vci-service-launcher` →
`vci-service-config`, `iso22900-registry` → `vci-service-config`, and now
`vci-service-manager` → `vci-service-config` directly), so enabling a
`config-root-*` feature on any edge activates it for every consumer in that
build — logging-config resolution and library-path resolution always agree
on which root they resolve, and the manager's discovery agrees with what
the spawned services will actually resolve at startup.

## Consequences

- `vci-service-manager`'s `/vci-libs` listing now includes config-only
  libraries (`library_path` set, no matching RDF/registry entry) for both
  `iso22900` and `j2534-0404`, closing the discovery gap described above.
- `library_path` resolution is now reached through the registry crates
  (`iso22900-registry`, `j2534-0404-registry`) at the two call sites that
  actually need it, rather than through the more general-purpose
  `vci-service-launcher` re-export — each registry crate is the natural
  place for "how do I resolve this API's library name to a path," since it
  already owns the platform-specific fallback (RDF / Windows registry) for
  the same question.
- `vci-service-config`'s `find_library_path` and the
  `vci_service_launcher::config` re-export are unchanged and still public;
  this ADR does not remove them, only stops routing the two known call
  sites through the api-name-string form now that a same-crate wrapper
  exists.
- No behavior change to the `library_path` priority rules or TOML schema —
  `list_configured_libraries` is a pure enumeration on top of the same
  data `find_library_path` already reads.

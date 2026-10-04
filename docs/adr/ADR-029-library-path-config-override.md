# ADR-029: `library_path` Config Override for J2534 v04.04 Library Resolution

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `vci-service-launcher/src/config.rs`, `j2534-0404-service/src/service.rs`

## Context

`j2534-0404-service` receives a **library name** (not a path) in its startup
argument (`j2534-0404:<library name>?port=<u16>&...`). Until now, the only
way to resolve that name to an actual DLL path was
`j2534_0404_registry::find_j2534_device()`, which enumerates
`HKLM\SOFTWARE\PassThruSupport.04.04` in the Windows registry.

This has two consequences:

- On non-Windows builds, `enumerate_devices_impl()` always returns
  `RegistryError::RegistryUnsupported`, so no library name can ever be
  resolved — there was no way to run the service against a Linux `.so` (e.g.
  for local testing) or any deployment where the vendor library isn't
  registered under `PassThruSupport.04.04`.
- Even on Windows, there was no way to point a library name at a path
  without a registry entry, or to override a stale/incorrect registry entry
  without editing the registry itself.

`vci-service-launcher` already reads a TOML config file
(`config.toml`) with a `config.apis.<api>.libs.<lib>` table, used so far only
for per-library logging overrides (`find_logging_config`, see ADR-024 /
ADR-025 for how the file's location is resolved). This table is a natural
place to add manually-configured library paths, since it is already keyed by
API name and library name.

## Decision

Add an optional `library_path` field to the same `libs.<lib>` (and
`arch.<arch>.libs.<lib>`) TOML tables used for logging config, and a new
`vci_service_launcher::config::find_library_path(api_name, arch,
library_name) -> Option<PathBuf>` lookup function with a 2-level priority
(highest first):

1. `config.apis.<api>.arch.<arch>.libs.<lib>.library_path` (Windows only)
2. `config.apis.<api>.libs.<lib>.library_path`

Unlike `find_logging_config`'s 5-level hierarchy, there is no api-level or
root-level fallback: a path is meaningless without a specific library name
attached, so only `libs.<lib>` entries are consulted.

`j2534-0404-service::J2534Service::new()` calls `find_library_path()` before
`j2534_0404_registry::find_j2534_device()`. **A configured `library_path`
takes priority over the registry result** when present for that library
name; only when no config entry exists does the service fall back to
registry auto-discovery. This makes the config file capable of adding
libraries the registry doesn't know about *and* overriding entries it does
know about, with one consistent rule ("explicit config wins") rather than
two different behaviors depending on whether the name happens to collide
with a registry entry.

Schema support (`library_path` field, `find_library_path()`) is added
generically in `vci-service-launcher`, but only `j2534-0404-service` is
wired to consult it — `iso22900-service` and `j2534-0500-service` are
unaffected until they opt in.

## Consequences

- Deployers and testers can resolve any library name on any platform by
  adding a `library_path` entry to `config.toml`, without touching the
  Windows registry.
- Non-Windows builds of `j2534-0404-service` can now start successfully
  against a real path (e.g. `j2534-0404-mock`'s `cdylib` output), provided
  a `library_path` is configured — the registry path remains unreachable
  there (`RegistryUnsupported`) exactly as before.
- A misconfigured `library_path` silently shadows a correct registry entry
  for the same name; this is the expected trade-off of "explicit config
  wins" and mirrors how logging config already overrides defaults.
- `iso22900-registry`'s RDF-based resolution for `iso22900-service` is
  unchanged; extending the same override pattern there is future work if
  needed.

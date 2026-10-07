# vci-service-config Implementation Note

## Scope

Loads and parses the shared `config.toml` file used by all VCI gRPC
services: logging settings (`find_logging_config`), `library_path`
overrides (`find_library_path`), library-name discovery
(`list_configured_libraries`), library-name-plus-path discovery
(`list_configured_library_paths`, ADR-036), the CAN channel operating
mode (`find_can_channel_mode`, ADR-046/ADR-047 — returned as an opaque string;
validation of the mode names is the consuming service's responsibility),
J2534 v04.04 device-selection module entries (`find_modules`, ADR-107 —
`Vec<ModuleConfigEntry>` from a `[[...modules]]` array-of-tables, each entry
an opaque `{ label, pname }` pair; validation — non-empty, ASCII `pname`
with no embedded NUL — is the consuming service's responsibility), and
standalone service-manager runtime settings (`manager_config`, ADR-073 —
`bind`/`root_path`/`ipc` (`ipc` added by ADR-226 for the local IPC listener
endpoint) from the flat `[config.manager]` table, no priority hierarchy),
the J2534 registration-definition directory (`j2534_definition_dir()`,
ADR-228 — `/etc/nekoguruma/j2534` by default, fixed at build time through
`NGR_J2534_DEFINITION_DIR` and resolved against the same fixed system
directory as `system_config_dir()`, never against a `config-root-*` root),
and a fixed, platform-specific system config directory
(`system_config_dir()`, ADR-226 SS3 amendment — `%ProgramData%\vci-service-launcher`
on Windows, `/private/etc/vci-service-launcher` on macOS,
`/etc/vci-service-launcher` on every other Unix; not gated by any
`config-root-*` Cargo feature and not itself the same root `config.toml`
resolves against). An earlier version of this function was
`config_root_dir()`, a thin wrapper around the same feature-selectable
`config_root()` `config.toml` itself resolves against — removed by the
ADR-226 SS3 amendment once it was found that two of `config_root()`'s four
Windows options resolve to a directory the manager's own identity owns
outright (permanently failing its own trust-root check), and that the
admin-oriented options and the non-Windows default had independent problems
of their own (see `system_config_dir()`'s own doc comment for the full
reasoning). `system_config_dir()` has no `#[cfg(test)]` override hook,
unlike `config_root()` — its only caller always wants the real value.

Every lookup above other than `find_modules` is deliberately lenient: a
missing config file, or one that fails to read/parse, is logged to stderr
and treated as "nothing configured" (`load_toml_config()`). `find_modules`
alone is fallible — `Result<Option<Vec<ModuleConfigEntry>>, ConfigFileError>`
via `try_load_toml_config()` — because a malformed `modules` entry must fail
startup rather than silently look like "modules not configured at all" and
let `j2534-0404-service` synthesize the default single-device
`PassThruOpen(NULL, ...)` behavior (ADR-107 addendum (j)). `NotFound` still
resolves to "nothing configured" even for `find_modules`; only a file that
exists but cannot be read or parsed is an `Err`.
See `docs/logging-config.md` for the full TOML schema and lookup-priority
tables.

Extracted from `vci-service-launcher` (ADR-033) so the config-loading logic
has no dependency on `tonic`/`tokio` or any other part of the gRPC/JSON-RPC
service bootstrap, and can be reused by consumers that only need
configuration, not a running service.

## Current Consumers

- `vci-service-launcher` re-exports this crate as `vci_service_launcher::config`
  (`pub use vci_service_config as config;` in `src/lib.rs`), so
  `iso22900-service` and `j2534-0404-service` keep
  using `vci_service_launcher::config::*` unchanged for logging.
  `j2534-0404-service` also reads `find_can_channel_mode()` through this
  re-export at startup (ADR-046), and `find_modules()` the same way (ADR-107).
- `iso22900-registry` and `j2534-0404-registry` depend on this crate
  directly and each expose a thin wrapper — `find_library_path(arch, lib)`
  and `list_configured_libraries()` — scoped to their own API name
  (`"iso22900"` / `"j2534-0404"`). `j2534-0404-service` and
  `iso22900-service` call their respective registry's `find_library_path`
  (not the launcher re-export) to resolve a library name to a path before
  falling back to registry/RDF auto-discovery (see ADR-034).
- Each registry crate's `enumerate_libraries(mode)` calls
  `list_configured_library_paths()` directly (not just
  `list_configured_libraries()`) so it can merge each config entry's
  `library_path` into the returned `PduLibraryInfo` / `J2534DeviceInfo`,
  not just its name (ADR-036).
- `manager_config()` and `system_config_dir()` serve a standalone service
  manager process. nekoguruma has none (the agent launches workers through
  `worker-host`), so nothing in this workspace calls them; they are kept so
  the crate's config format stays complete.

## Design Policy

- Zero dependency on `tonic`, `tokio`, or `vci-service-interface` — this
  crate must stay usable from a plain synchronous binary.
- Preserve the `config-root-*` / `eventlog` / `journald` Cargo feature names
  exactly as they existed in `vci-service-launcher` before extraction, since
  `iso22900-service` and `j2534-0404-service` forward
  them by name in their own `Cargo.toml`.
- The default `VCI_CONFIG_PATH` build-time value is `nekoguruma/config.toml`,
  resolved against `/etc` on Linux and the selected known folder on Windows
  (`%ProgramData%\nekoguruma\config.toml` by default), the fixed,
  administrator-only locations of ADR-228. The registration-definition
  directory (`j2534_definition_dir()`, `NGR_J2534_DEFINITION_DIR`, default
  `nekoguruma/j2534`) is fixed the same way. Each has a runtime override in
  debug builds only (ADR-073). Before ADR-228 the default was
  `vci-service-launcher/config.toml`, which the Linux root `/` turned into
  `/vci-service-launcher/config.toml`.

# ADR-033: Extract Config File Loading into `vci-service-config`

**Date:** 2026-07-02
**Status:** Accepted (kept default config locations superseded by ADR-228)
**Affects:** `vci-service-launcher` (new dependency, `src/lib.rs`, `Cargo.toml`, `build.rs` removed), `vci-service-config` (new crate), `Cargo.toml` (workspace)

## Context

`vci-service-launcher::config` (`src/config.rs`) resolved and parsed the
shared `config.toml` file and exposed `find_logging_config` /
`find_library_path` (see ADR-024, ADR-025, ADR-029, ADR-030). It had no
dependency on the rest of `vci-service-launcher` — not on `tonic`, `tokio`,
or `vci-service-interface` — but lived inside a crate whose primary purpose
is bootstrapping a combined gRPC + JSON-RPC service (`start_service::<T:
VciServer>()`).

`vci-service-manager` is planned to read `config.toml` directly (rather than
only launching `*-service` child processes that read it themselves), for
example to validate configuration before starting a child, or to resolve a
`library_path` override itself. `vci-service-manager` is a plain Axum/Hyper
HTTP process with no gRPC server of its own; depending on
`vci-service-launcher` for this would pull in `tonic`, `tokio`'s full
feature set, and the JSON-RPC stdio machinery for no reason.

## Decision

Move `vci-service-launcher/src/config.rs`, its `build.rs` (which embeds
`VCI_CONFIG_PATH` via `env!()`), and its config-file-location tests into a
new workspace member, `vci-service-config`. The new crate has no dependency
on `vci-service-interface`, `tonic`, or `tokio` — only `serde`, `toml`, and
(Windows-only) `windows-sys` for `SHGetKnownFolderPath`/`CoTaskMemFree`.

All Cargo features that gate config behavior move with the code:
`config-root-exe-dir`, `config-root-win-local-app-data`,
`config-root-win-roaming-app-data`, `config-root-win-program-files`,
`config-root-win-program-files-x86`, `eventlog` (gates the
`LogOutputKind::Eventlog` enum variant), and `journald` (gates
`LogOutputKind::Journald`).

`vci-service-launcher` depends on `vci-service-config` and re-exports it
unchanged:

```rust
pub use vci_service_config as config;
```

so every existing `vci_service_launcher::config::*` call site —
`iso22900-service`, `j2534-0404-service`, internal `lib.rs`/`logging.rs`
usage — keeps compiling with no changes. `vci-service-launcher`'s own
`eventlog`/`journald`/`config-root-*` features now forward to the
same-named features on `vci-service-config` (e.g. `eventlog =
["vci-service-config/eventlog"]`), so the `iso22900-service` /
`j2534-0404-service` / `j2534-0500-service` `Cargo.toml` files, which
already forward these features to `vci-service-launcher`, need no changes
either.

The default `VCI_CONFIG_PATH` build-time value
(`vci-service-launcher/config.toml`) and the deployed config file locations
(`%ProgramData%\vci-service-launcher\config.toml`,
`/etc/vci-service-launcher/config.toml`) are kept as-is — these name an
on-disk deployment layout, not the crate that reads it, and changing them
would break already-deployed installs for no benefit.

`vci-service-launcher/docs/logging-config.md` moves to
`vci-service-config/docs/logging-config.md`, since it documents the
`config.toml` schema and lookup functions that now live there.

## Consequences

- `vci-service-manager` can add `vci-service-config` as a direct dependency
  and call `find_logging_config` / `find_library_path` without pulling in
  `tonic`/`tokio`/`vci-service-interface`. This wiring itself is not done by
  this ADR — it is deferred until `vci-service-manager` actually needs to
  read `config.toml`.
- No behavior change for any existing consumer: `vci_service_launcher::config`
  resolves to the exact same types and functions as before, just re-exported
  from a different crate; the TOML schema, lookup priority, feature names,
  and default file paths are all unchanged.
- `vci-service-launcher`'s own `Cargo.toml` sheds the `serde`, `toml`, and
  `tempfile` (dev) dependencies, and its `windows-sys` feature list drops
  `Win32_System_Com`/`Win32_UI_Shell` (used only by config-root resolution) —
  it keeps `Win32_Foundation`/`Win32_System_EventLog` for the `eventlog`
  Windows Event Log writer, which stays in `vci-service-launcher::logging`
  since it is specific to that crate's `tracing-subscriber` setup, not to
  config loading.
- A pre-existing, unrelated bug survives this move unchanged: building
  `vci-service-launcher` (or any `*-service` crate) with `--features
  eventlog` on a non-Windows target fails to compile
  (`logging.rs`'s `eventlog` module unconditionally imports the
  Windows-only `FieldVisitor` type). This was true before the extraction
  and is out of scope here.

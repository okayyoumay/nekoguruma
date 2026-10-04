# ADR-073: `vci-service-manager` Runtime Settings Move from Env Vars to `[config.manager]`

**Date:** 2026-07-09
**Status:** Accepted (`base_url`, one of the three settings this ADR moved into `[config.manager]`, was removed outright by ADR-221 along with the manager's gRPC proxy it configured — `bind`/`root_path`, this ADR's other two settings and its actual env-to-config migration mechanism, remain in force; `[config.manager]` gained a fourth field, `ipc`, under ADR-226, via the same table and env-to-config-migration mechanism this ADR established)
**Affects:** `vci-service-config` (src/lib.rs), `vci-service-manager` (src/main.rs), `docs/QUICKSTART.md`, `vci-service-manager/docs/implementation-notes.md`, `docs/worker-crates.md`

## Context

`vci-service-manager::main()` read three environment variables at startup:

- `VCI_MANAGER_BIND` (default `127.0.0.1:8080`)
- `VCI_MANAGER_ROOT_PATH` (default empty)
- `VCI_MANAGER_BASE_URL` (default `http://{VCI_MANAGER_BIND}`)

Meanwhile every other VCI process in this workspace (`iso22900-service`,
`j2534-0404-service`, `j2534-0500-service`, and `vci-service-manager` itself
for its discovery/logging lookups) already reads `config.toml` via
`vci-service-config` (ADR-033), with a priority-ordered lookup scheme for
logging, `library_path`, and `can_channel_mode` settings. Deployers therefore
had to configure the manager's own bind/base-URL/root-path through a second,
inconsistent mechanism (process environment) rather than the file already
used for everything else the manager and its child services need.

Separately, `vci-service-manager` also reads three env vars used to override
the *service binary path* it launches per library type
(`VCI_MANAGER_ISO22900_SERVICE_BIN`, `VCI_MANAGER_J2534_0404_SERVICE_BIN`,
`VCI_MANAGER_J2534_0500_SERVICE_BIN`; see ADR-032). Those are a deliberate
development/debug escape hatch — pointing at a `target/debug/` build, or a
non-standard install layout — not a piece of steady-state runtime
configuration, so they are out of scope for this migration.

## Decision

Move `bind`, `root_path`, and `base_url` off environment variables and into a
new `[config.manager]` table in the shared `config.toml`, resolved through
`vci-service-config` the same way as every other setting in that file:

- `vci-service-config` gains a `ManagerConfig` struct (`bind: Option<String>`,
  `root_path: Option<String>`, `base_url: Option<String>`) and a
  `manager: Option<ManagerConfig>` field on the internal `RootConfig`, plus a
  public `manager_config() -> ManagerConfig` accessor. Unlike the api-scoped
  lookups (`find_logging_config`, `find_library_path`,
  `find_can_channel_mode`), `[config.manager]` is a single flat table with no
  priority hierarchy to resolve — there is exactly one manager process per
  config file, so there is nothing to disambiguate between.
- `vci-service-manager::main()` calls `vci_service_config::manager_config()`
  once at startup and applies the same defaults as before
  (`"127.0.0.1:8080"`, empty root path, `http://{bind}`) when a field is
  `None`, whether because the file is missing, unparseable, or simply omits
  `[config.manager]` or an individual key. `vci-service-config`'s existing
  degrade-to-defaults-with-an-eprintln-warning behavior on a missing/bad file
  therefore applies here unchanged.
- Environment variables remain, narrowly, for debug/test-scoped overrides
  only, and this ADR refines ADR-024 and ADR-032 by additionally gating both
  remaining overrides behind `#[cfg(debug_assertions)]` so they are compiled
  only into debug builds:
  - `VCI_MANAGER_ISO22900_SERVICE_BIN` / `VCI_MANAGER_J2534_0404_SERVICE_BIN`
    / `VCI_MANAGER_J2534_0500_SERVICE_BIN` stay exactly as introduced in
    ADR-032 — a development/debug escape hatch for pointing at a binary
    outside the colocated-binary production default. Their doc comment now
    says so explicitly. `resolve_service_bin_path()` reads them only when
    `debug_assertions` holds; in release builds the code path (and the
    error-message hint naming the env var) does not exist.
  - The runtime `VCI_CONFIG_PATH` override in `vci-service-config` (ADR-024)
    is otherwise untouched; it selects *which* `config.toml` is read (for
    IDE/debug launches), which is orthogonal to what is read from within
    that file. `config_file_path()` now reads it only when
    `debug_assertions` holds; release builds use solely the build-time
    embedded value.
  - Build-time/test-only variables (e.g. the `VCI_CONFIG_PATH` embedded via
    `build.rs`, or anything scoped to `#[cfg(test)]`) were never part of the
    manager's runtime configuration surface and are unaffected.

### Alternatives considered

- **Keep both env vars and config-file keys, env vars taking precedence.**
  Rejected: a dual-source setting with a silent precedence rule is exactly
  the kind of surprise this migration is meant to remove — a deployer
  editing `config.toml` and observing no effect because a stale env var is
  still set in their shell/service unit is a worse failure mode than a single
  authoritative source.
- **Move the `VCI_MANAGER_*_SERVICE_BIN` overrides into `config.toml` too.**
  Rejected: those overrides exist specifically to bypass the deployed/config
  surface for local development (e.g. `cargo build`'s `target/debug`
  directory not being the colocated-binary layout the config describes) per
  ADR-032; putting them in the same file as production settings would blur
  that boundary and risk being accidentally committed/deployed.

## Consequences

- A single `config.toml`, already required for logging/library-path/
  can-channel-mode settings, now also fully configures `vci-service-manager`'s
  own bind address, base URL, and root path — one configuration surface
  instead of two.
- Existing deployments that relied on `VCI_MANAGER_BIND` /
  `VCI_MANAGER_ROOT_PATH` / `VCI_MANAGER_BASE_URL` must move those values into
  `[config.manager]` in `config.toml`; the env vars are no longer read for
  this purpose. This is a breaking change for any deployment script that set
  them.
- `docs/QUICKSTART.md` and `vci-service-manager/docs/implementation-notes.md`
  are updated to describe `[config.manager]` and to clearly separate it from
  the remaining debug/development-only env vars.
- Because the retained overrides (`VCI_MANAGER_*_SERVICE_BIN` and runtime
  `VCI_CONFIG_PATH`) are compiled only into debug builds, release binaries
  read no environment variables at runtime anywhere in this configuration
  surface; a release deployment selects the config file location entirely
  at build time via `--features config-root-*` (see
  `vci-service-config/docs/implementation-notes.md`).
- Tests that exercise these overrides (e.g.
  `resolve_service_bin_path_uses_env_override_file_when_present`,
  `vci-service-config/tests/runtime_config_path_override.rs`, and the
  registry/gRPC-mock integration tests that rely on redirecting
  `VCI_CONFIG_PATH`) are gated with `#[cfg(debug_assertions)]` /
  `#![cfg(debug_assertions)]` and are skipped under
  `cargo test --release`.

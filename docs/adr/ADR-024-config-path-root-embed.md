# ADR-024: Build-time Configuration of the Config File Root

**Date:** 2026-06-30
**Status:** Accepted
**Affects:** `vci-service-launcher/build.rs`, `vci-service-launcher/Cargo.toml`, `vci-service-launcher/src/config.rs`, `iso22900-service/Cargo.toml`, `j2534-0404-service/Cargo.toml`, `.vscode/launch.json`

## Context

`config_file_path()` previously hard-coded both the root directory and the
relative path to the TOML config file:

- Windows: `%ProgramData%\vci-service-launcher\config.toml`
- Non-Windows: `/etc/vci-service-launcher/config.toml`

When the same binary needs to be deployed in multiple layouts — e.g. a portable
installation that sits next to the executable, or a Windows installation that
stores user data under `FOLDERID_LocalAppData` instead of `FOLDERID_ProgramData`
— the hard-coded paths force a rebuild with source changes.

## Decision

### Config path (all platforms)

A `build.rs` captures a build-time environment variable and embeds it via
`cargo:rustc-env` so it is accessible with `env!()` in `config.rs`:

| Variable | Default | Purpose |
|---|---|---|
| `VCI_CONFIG_PATH` | `vci-service-launcher/config.toml` | Path to the config file: relative to the selected root, or absolute to bypass root resolution entirely |

`resolve_config_path()` checks `Path::is_absolute()` on this value first. If
absolute, it is returned as-is and `config_root()` is never called — so an
absolute override also skips `SHGetKnownFolderPath` / `current_exe()`
entirely, not just their result. (This variable was originally named
`VCI_CONFIG_RELATIVE_PATH`; it was renamed when absolute-path support was
added, since "relative" was no longer accurate.)

`config_file_path()` also checks `VCI_CONFIG_PATH` as a **runtime**
environment variable before falling back to the build-embedded value above;
a runtime value, if set, takes precedence. This mirrors the runtime
env-var-override pattern already used by `iso22900-registry`'s
`root_description_file_path()`, and exists specifically so an IDE debug
launch configuration can point a service at a dedicated config file (via the
top-level `env` key in `.vscode/launch.json`, which reliably applies to the
debuggee process) without needing the build step itself to see the
variable — `cargo`-integrated debuggers (e.g. CodeLLDB) do not reliably
propagate launch-config `env` into the `cargo build` invocation that runs
`build.rs`, so a build-time-only embed would not be controllable per debug
session.

### Feature propagation to `*-service` packages

`iso22900-service` and `j2534-0404-service` each re-export the five
`config-root-*` features from `vci-service-launcher`, the same way they
already re-export `eventlog` and `journald`:

```toml
config-root-exe-dir = ["vci-service-launcher/config-root-exe-dir"]
config-root-win-local-app-data = ["vci-service-launcher/config-root-win-local-app-data"]
config-root-win-roaming-app-data = ["vci-service-launcher/config-root-win-roaming-app-data"]
config-root-win-program-files = ["vci-service-launcher/config-root-win-program-files"]
config-root-win-program-files-x86 = ["vci-service-launcher/config-root-win-program-files-x86"]
```

This lets deployers select a config root layout with
`cargo build -p j2534-0404-service --features config-root-exe-dir` (etc.)
without depending on `vci-service-launcher` directly.

### Root directory selection (Cargo features)

`config_file_path()` is split into two parts: a `config_root()` function that
returns the base directory, and the path join. Exactly one definition of
`config_root()` is compiled for any given (target, feature) combination:

| Condition | Root |
|---|---|
| `config-root-exe-dir` (all platforms) | Directory containing the running executable |
| Windows, no `config-root-exe-dir` | Windows known folder from `SHGetKnownFolderPath` |
| Non-Windows, no `config-root-exe-dir` | `/` |

### Windows known-folder selection

On Windows (when `config-root-exe-dir` is not active), the root is obtained at
**runtime** by calling `SHGetKnownFolderPath`. Which `FOLDERID_*` to pass is
selected at **build time** by the first active feature in this priority chain:

| Feature | FOLDERID |
|---|---|
| `config-root-win-local-app-data` | `FOLDERID_LocalAppData` |
| `config-root-win-roaming-app-data` | `FOLDERID_RoamingAppData` |
| `config-root-win-program-files` | `FOLDERID_ProgramFiles` |
| `config-root-win-program-files-x86` | `FOLDERID_ProgramFilesX86` |
| *(default, none of the above)* | `FOLDERID_ProgramData` |

`windows-sys` is declared as a `[target.'cfg(windows)'.dependencies]` entry
(non-optional on Windows, never compiled on other platforms). It provides
`Win32_UI_Shell` (for `SHGetKnownFolderPath` and all `FOLDERID_*` constants),
`Win32_System_Com` (for `CoTaskMemFree`), `Win32_Foundation`, and
`Win32_System_EventLog` (for the `eventlog` feature). The `eventlog` feature
therefore no longer needs to pull in `dep:windows-sys` explicitly.

## Consequences

- **No behavioural change** when building without any new feature or env var:
  the default `FOLDERID_ProgramData` path returned by `SHGetKnownFolderPath`
  is the same folder as the previous `%ProgramData%` environment variable
  lookup (`C:\ProgramData`), while the non-Windows `/` root combined with the
  default relative path reproduces `/etc/vci-service-launcher/config.toml`.
- Portable deployments can be built with `--features config-root-exe-dir`.
- Windows per-user deployments can be built with
  `--features config-root-win-local-app-data` (non-roaming) or
  `--features config-root-win-roaming-app-data` (roaming).
- `config-root-win-*` features are intended to be mutually exclusive; if
  multiple are enabled (e.g. via transitive dependency resolution), the
  priority chain in `windows_known_folder_root()` determines which one wins.
- The non-Windows "root" is always `/`; any other non-Windows base directory
  requires `config-root-exe-dir` or a `VCI_CONFIG_PATH` that spells out an
  absolute path (see "Config path" above — this is now an explicit,
  short-circuiting code path rather than an incidental consequence of
  `PathBuf::join`'s absolute-path-replaces-base behavior).
- The "Debug executable 'j2534-0404-service'" configuration in
  `.vscode/launch.json` sets `VCI_CONFIG_PATH` to
  `.vscode/j2534-0404-service.debug-config.toml`, so debug sessions read
  that file (checked into the repo) instead of the real deployed config
  location — no admin rights or manual `%ProgramData%`/`/etc` setup needed
  to get useful logging while debugging.

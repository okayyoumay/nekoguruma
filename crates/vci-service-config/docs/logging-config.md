# vci-service-config Configuration

This document covers the full `config.toml` schema loaded by
`vci-service-config`: logging settings and (for services that consult it,
currently `j2534-0404-service` and `iso22900-service`) `library_path`
overrides. `vci-service-launcher` re-exports this crate as
`vci_service_launcher::config` (see ADR-033), so every service built on the
launcher template picks this schema up automatically.

## Config File Location

Each service looks for a single TOML file at startup:

| Platform | Path |
|----------|------|
| Windows | `%ProgramData%\nekoguruma\config.toml` (typically `C:\ProgramData\nekoguruma\config.toml`) |
| Linux and other Unix | `/etc/nekoguruma/config.toml` (`/private/etc` is where `/etc` points on macOS) |

These are the locations of a build with the default features, fixed at build time and writable
by administrators only (ADR-228). A `config-root-*` feature moves the root: with
`config-root-exe-dir` the file is `nekoguruma/config.toml` next to the executable, and with
`config-root-win-local-app-data` or `config-root-win-roaming-app-data` it is in the user's own
profile, which the user can write.

The J2534 registration definitions on Linux (design 7.1.1) are read from
`/etc/nekoguruma/j2534/` in every build: no `config-root-*` feature moves them
(`j2534_definition_dir()`; `NGR_J2534_DEFINITION_DIR` at build time, with a runtime override in
debug builds only). The D-PDU API root description file on Linux is likewise fixed
(`pdu_api_root_file()`, `NGR_PDU_API_ROOT_FILE`, default `/etc/pdu_api_root.xml`).

If the file does not exist, all logging defaults to `stderr` at the `info` level. Parse errors in the file are printed to `stderr` and defaults are used.

### Overriding the config file location

In **debug builds only**, the path above can be changed without a rebuild by
setting the `VCI_CONFIG_PATH` environment variable at **runtime** — it takes
precedence over the build-embedded default (see ADR-024). It may be a path
relative to the selected root (see `--features config-root-*` in
`vci-service-config/Cargo.toml`, re-exported by
`vci-service-launcher/Cargo.toml` and each `*-service/Cargo.toml`)
or an absolute path, which is used as-is. Release builds compile this
override out entirely (`debug_assertions`, see ADR-073) and read no
environment variable at runtime.

This is how the "Debug executable 'j2534-0404-service'" launch configuration
in `.vscode/launch.json` points debug sessions at
`.vscode/j2534-0404-service.debug-config.toml` instead of the real deployed
config file.

For release deployments, `VCI_CONFIG_PATH` is set at **build time** (read by
`build.rs` and embedded via `env!()`) to change the compiled-in default path
itself, and the `config-root-*` features select the root it is resolved
against — fixed custom deployment layouts are baked in at build time rather
than selected per run.

---

## TOML Schema

```toml
[config]
# Root-level fallback — applies when no more-specific entry matches.
[config.logging]
level  = "info"           # tracing-subscriber EnvFilter directive (string)
output = "stderr"         # output destination (see below)

# Per-API settings — applies to all instances of a given API type.
[config.apis.<api-name>]
[config.apis.<api-name>.logging]
level  = "debug"
output = "stderr"

# Per-API, per-library settings (cross-platform) — highest priority on Linux.
[config.apis.<api-name>.libs.<library-name>]
library_path = "/opt/vci/mylib.so"   # optional; see "Library Path Overrides" below
[config.apis.<api-name>.libs.<library-name>.logging]
level  = "warn"
output = { file = "/var/log/vci/mylib.log" }

# Per-API, per-architecture settings (Windows only).
[config.apis.<api-name>.arch.<arch-name>]
[config.apis.<api-name>.arch.<arch-name>.logging]
level  = "debug"
output = "eventlog"

# Per-API, per-architecture, per-library settings (Windows only, highest priority).
[config.apis.<api-name>.arch.<arch-name>.libs.<library-name>]
library_path = "C:/Program Files/Vendor/mylib-x86_x64.dll"   # optional
[config.apis.<api-name>.arch.<arch-name>.libs.<library-name>.logging]
level  = "trace"
output = { file = "C:/ProgramData/nekoguruma/logs/specific-lib.log" }
```

### Field: `level`

A `tracing-subscriber` EnvFilter directive string. Defaults to `"info"` when omitted.

Examples:

```toml
level = "info"
level = "debug"
level = "warn"
level = "j2534_0404_service=debug,info"  # module-specific level
level = "iso22900_service=trace,warn"
```

### Field: `output`

Specifies where log records are written. Defaults to `"stderr"` when omitted.

| Value | Description |
|-------|-------------|
| `"stderr"` | Standard error (default). `stdout` must not be used — it is reserved for JSON-RPC. |
| `"null"` | Discard all log output. |
| `"eventlog"` | Windows Event Log. Requires the `eventlog` Cargo feature; falls back to `stderr` on non-Windows. |
| `"journald"` | systemd journal. Requires the `journald` Cargo feature; falls back to `stderr` on non-Linux. |
| `{ file = "<path>" }` | Append to a file. The file is created if it does not exist. Falls back to `stderr` if the file cannot be opened. |

---

## Lookup Priority

`find_logging_config` applies the following priority order (highest first) to find the logging config for a service instance:

| Priority | Key path | Platform |
|----------|----------|----------|
| 1 | `config.apis.<api>.arch.<arch>.libs.<lib>` | Windows only |
| 2 | `config.apis.<api>.arch.<arch>` | Windows only |
| 3 | `config.apis.<api>.libs.<lib>` | All |
| 4 | `config.apis.<api>` | All |
| 5 | `config` (root) | All |

The first level that has a `[logging]` entry wins. Levels do not merge; the entire `LoggingConfig` (both `level` and `output`) comes from the winning entry.

### Key values

- `<api-name>`: the service API identifier. Current values:
  - `iso22900` for `iso22900-service`
  - `j2534-0404` for `j2534-0404-service`
- `<arch-name>`: the target architecture string (Windows only). Example values: `x86_64`, `i686`.
- `<lib-name>`: the library name as passed in the startup argument (e.g., `D_PDU_API_Bosch_6531_Bosch`, `OpenPort2`).

---

## Library Path Overrides

The `libs.<library-name>` and `arch.<arch-name>.libs.<library-name>` tables
accept an optional `library_path` field: an explicit filesystem path to the
library, keyed by the same library name passed in the startup argument.

Unlike logging, `library_path` has no api-level or root-level fallback —
it is only ever read from a `libs.<lib>` entry, since a path is meaningless
without a specific library name to attach it to.

| Priority | Key path | Platform |
|----------|----------|----------|
| 1 | `config.apis.<api>.arch.<arch>.libs.<lib>.library_path` | Windows only |
| 2 | `config.apis.<api>.libs.<lib>.library_path` | All |

**Lookup is exposed through `iso22900-registry::find_library_path()` and
`j2534-0404-registry::find_library_path()`** (both thin wrappers around this
crate's `find_library_path()`, scoped to their own API name), not called
directly from this crate by `iso22900-service` / `j2534-0404-service` — see
ADR-034. `iso22900-service` and `j2534-0404-service` do not call
`find_library_path()` directly either: each calls its registry's single
`resolve_library_path()` function, which applies the priority itself (see
ADR-035). When set for a library name, it is used directly and the
platform-specific fallback — the Windows registry (`j2534-0404-registry`,
`HKLM\SOFTWARE\PassThruSupport.04.04`) for `j2534-0404-service`, or RDF-based
lookup (`iso22900-registry`) for `iso22900-service` — is not consulted for
that name. When unset, `resolve_library_path()` falls back to its own
registry's auto-discovery mechanism as before — so `library_path` is the
only way to resolve a library on non-Windows platforms, where the J2534
registry is unavailable and (unlike RDF, which still has a hardcoded
`/etc/pdu_api_root.xml` path) there is no other way to point
`iso22900-service` at a library without touching that fixed path or the
Windows registry.

The registry crates' `enumerate_libraries()` also reads this table (via
`list_configured_library_paths()`), so config-only libraries — ones with no
hardware auto-discovery entry — are included in discovery (ADR-034, ADR-036).

### Example: add a library manually and point another at a specific build

```toml
# Not present in the Windows registry (e.g. a Linux .so under test, or a
# vendor DLL installed without a PassThruSupport.04.04 registry entry).
[config.apis.j2534-0404.libs."OpenPort2"]
library_path = "/opt/vci/openport2.so"

# Overrides the registry-resolved path for this specific architecture only;
# other architectures still fall back to the registry.
[config.apis.j2534-0404.arch.x86_x64.libs."MongoosePro GM II"]
library_path = "C:/Program Files/Drew Technologies/MongoosePro GM II/mongoose-x64.dll"

# Not present in the RDF pointed to by the Windows registry (or not present
# at all on non-Windows, where no RDF-based lookup is reachable without a
# file at /etc/pdu_api_root.xml).
[config.apis.iso22900.libs."D_PDU_API_Bosch_6531_Bosch"]
library_path = "/opt/vci/bosch-dpdu.so"
```

---

## CAN Channel Mode (`can_channel_mode`)

The `libs.<library-name>`, `arch.<arch-name>.libs.<library-name>`, and
api-level tables accept an optional `can_channel_mode` string that selects
how CAN-family ComLogicalLinks are mapped onto J2534 physical channels.
This crate treats the value as an opaque string (`find_can_channel_mode()`);
validation and interpretation belong to the consuming service —
`j2534-0404-service` accepts `"dual-channel"`, `"single-channel"` (default
when unset), `"software-isotp"`, and `"auto"` (probes dual-channel
capability at connect time instead of requiring it up front), and fails
startup on any other value. See ADR-046, ADR-047, and the "CAN Channel
Operating Modes" section of `docs/j2534-0404-architecture.md` for what each
mode does.

| Priority | Key path | Platform |
|----------|----------|----------|
| 1 | `config.apis.<api>.arch.<arch>.libs.<lib>.can_channel_mode` | Windows only |
| 2 | `config.apis.<api>.libs.<lib>.can_channel_mode` | All |
| 3 | `config.apis.<api>.can_channel_mode` | All |

Unlike `library_path`, an api-level default is supported (priority 3):
a channel-mapping policy can meaningfully apply to every library of an API,
whereas a filesystem path cannot.

### Example: software ISO-TP for one library, dual-channel default

```toml
# Every j2534-0404 library defaults to dual-channel operation…
[config.apis.j2534-0404]
can_channel_mode = "dual-channel"

# …except this device, whose ISO15765 channel support is unreliable:
# run a raw CAN channel and let the service do ISO-TP itself.
[config.apis.j2534-0404.libs."FlakyVci"]
can_channel_mode = "software-isotp"
```

---

## J2534 v04.04 Module Selection (`modules`)

The `libs.<library-name>` and `arch.<arch-name>.libs.<library-name>` tables
accept an optional `modules` array-of-tables, each entry pre-declaring one
selectable physical device behind a `j2534-0404` vendor DLL: a human-readable
`label` and a `pname` connection-target string. SAE J2534-1 v04.04 has no
in-spec way to select which device `PassThruOpen` opens (`pName` must be
`NULL` per spec §7.2.1); `j2534-0404-service` passes a configured `pname`
verbatim as `PassThruOpen`'s `pName` argument instead — an out-of-spec
vendor extension some J2534 DLLs support (see ADR-107). This crate treats
`label`/`pname` as opaque strings (`find_modules()`); validation (non-empty
array, ASCII `pname` with no embedded NUL byte) belongs to the consuming
service.

| Priority | Key path | Platform |
|----------|----------|----------|
| 1 | `config.apis.<api>.arch.<arch>.libs.<lib>.modules` | Windows only |
| 2 | `config.apis.<api>.libs.<lib>.modules` | All |

Unlike `can_channel_mode`, there is no api-level default: a `modules` list
only makes sense tied to one specific library (same contract as
`library_path`). When `modules` is omitted entirely, `j2534-0404-service`
falls back to a single default module (`PassThruOpen(NULL, ...)`), matching
pre-ADR-107 behavior. `module_handle` is the entry's 1-based position in the
array; deployers should keep the array order stable across restarts so
persisted client handles keep referring to the same device.

### Example: two physical devices behind one vendor DLL

```toml
[config.apis.j2534-0404.libs."OpenPort2"]
library_path = "/opt/vci/openport2.so"

[[config.apis.j2534-0404.libs."OpenPort2".modules]]
label = "Bench 1"
pname = "USB:1"

[[config.apis.j2534-0404.libs."OpenPort2".modules]]
label = "Bench 2"
pname = "USB:2"
```

`GetModuleIds` reports two rows (`module_handle` 1 and 2, `vendor_module_name`
`"Bench 1"`/`"Bench 2"`); `ModuleConnect(module_handle=2)` opens the device
with `PassThruOpen("USB:2", ...)`. Only one of the two devices can be open at
a time — connecting the other handle while one is open requires
`ModuleDisconnect` first.

---

## Examples

### Minimal: all services log at `info` to `stderr`

```toml
[config.logging]
level = "info"
output = "stderr"
```

### Enable debug logging for all J2534 services

```toml
[config.logging]
level = "info"

[config.apis.j2534-0404.logging]
level = "debug"
```

### Log a specific library to a file

```toml
[config.logging]
level = "info"

[config.apis.iso22900.libs."D_PDU_API_Bosch_6531_Bosch".logging]
level = "debug"
output = { file = "/var/log/vci/bosch.log" }
```

### Windows: use Event Log for all iso22900 services on i686

```toml
[config.logging]
level = "warn"
output = "stderr"

[config.apis.iso22900.arch.i686.logging]
level = "info"
output = "eventlog"
```

### Silence a noisy library while keeping everything else at info

```toml
[config.logging]
level = "info"

[config.apis.j2534-0404.libs."NoisyVendor.dll".logging]
level = "warn"
output = "null"
```

---

## Notes

- `stdout` is always reserved for JSON-RPC communication with the parent process (the agent, via `worker-host`). Do not configure log output to `stdout`.
- When the config file is missing or unparseable, a warning is printed to `stderr` and the default (`info` + `stderr`) is used. The service still starts normally.
- File output appends; the file is never truncated on startup. Rotate externally (e.g., `logrotate` on Linux).
- A missing or unparseable config file also means no `library_path` overrides are applied — `j2534-0404-service` falls back to registry auto-discovery for every library name in that case, which fails with `RegistryUnsupported` on non-Windows, and `iso22900-service` falls back to RDF-based lookup, which fails unless `/etc/pdu_api_root.xml` (or, on Windows, the registry-configured RDF) happens to list that name.

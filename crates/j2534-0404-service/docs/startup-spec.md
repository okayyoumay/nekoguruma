# Startup Argument and Stdio JSON-RPC Spec for `j2534-0404-service`

Related maintenance note: [comparam-mapping.md](comparam-mapping.md)

Date: 2026-07-02

## Scope

This document defines the startup argument format and the JSON-RPC contract on standard input/output for `j2534-0404-service`. The contract mirrors that of `iso22900-service` (see `iso22900-service/docs/grpc-instance-spec.md`) with the scheme identifier changed to `j2534-0404`.

## Startup Argument

- The first program argument must use this format:
  - `j2534-0404:<library name>?mode=<device|user>&port=<grpc port number>&...`
- The query string is required, since it carries `mode`.
- `<library name>` is required and must be non-empty.
- Query parameter `mode` is required: the device's operating mode (design 6.2), `device` or `user` in lowercase (compared after URL percent-decoding, like every query parameter). The agent passes it; the worker does not infer it and no configuration file sets it (ADR-281 Decision item 2). A missing, repeated or unknown `mode` is rejected as invalid input. Design 7.2 gives the mode its meaning for a failed library check: device mode refuses the library, user mode reports the findings and loads it. The worker logs the mode at startup and applies it to the checks it runs before loading ("Library Name Resolution" below).
- Query parameter `port` is optional.
- Unknown query parameters are ignored by the current implementation.
- Repeating `port` is rejected as invalid input.
- `port` must parse as a valid `u16` value.

### Examples

- `j2534-0404:OpenPort2?mode=user`
- `j2534-0404:OpenPort2?mode=device&port=60124`
- `j2534-0404:OpenPort2?mode=user&port=60124&profile=test`

## Process Startup Behavior

1. The process parses the first startup argument.
2. The gRPC server starts immediately during process startup.
3. If `port` is provided, the gRPC server attempts to bind that port.
4. If `port` is omitted, the gRPC server binds an ephemeral local port.
5. The process owns one local gRPC server instance.

### Library Name Resolution

`<library name>` resolves through `ngr_library_resolver::j2534_0404::resolve()`
(design 7.1.1, ADR-228 Decision item 4): the registration definition whose
`Name` matches on Linux, or the registry key of that name under
`HKLM\SOFTWARE\PassThruSupport.04.04` (native view) on Windows, gives the
library path. In a debug build, `NGR_J2534_DEFINITION_DIR` replaces both with a
definition directory (ADR-282), which is how the tests point the service at the
mock library or `sim-vci`. No `config.toml` key names the library (ADR-281
Decision item 1).

The same name is the `"<lib>"` key of the library's settings in `config.toml`
(`config.apis.j2534-0404.libs."<lib>"`: `modules`, `can_channel_mode`,
`vendor_ioctls`), so the settings read are those of the library resolved.

Before it reads any setting, the launcher (`vci_service_launcher::start_service`)
checks `config.toml` (`vci_service_config::config_file()`) with the design 7.2
writability rules, against the system owners (ADR-270 Decision item 5,
ADR-285). In device mode a file that fails the check is not read: logging
starts with its defaults and the service does not start. In user mode the
findings are logged as a warning and the file is used. The launcher passes its
verdict to `J2534Service::new` (`vci_service_launcher::ConfigFile`), which
reads `modules`, `can_channel_mode` and `vendor_ioctls` only when
`ConfigFile::settings_apply` says so. When `config.toml` does not exist at
startup, the service uses the defaults for its lifetime, since a file created
later passed no check.

Immediately before loading, the service runs the design 7.2 pre-load checks
(`src/preload.rs`, through `ngr_library_resolver::check_library` with the
system owners as the trusted set) on the library and the definition that named
it. A registry hit has no naming file to check, since HKLM is trusted by
premise (7.2). In device mode a failed check refuses the library with
`FAILED_PRECONDITION` and a message that names no path; in user mode the
findings are logged as a warning and the library is loaded. What the check
opened stays open until the library is mapped (ADR-278).

## Stdio Framing

- JSON-RPC uses one-line JSON framing on stdin/stdout.
- Each request is one JSON document terminated by a newline.
- Each response is one JSON document terminated by a newline.

## Supported JSON-RPC Methods

### `ping`

- Purpose: connectivity check.
- Result:
  - `{ "message": "pong" }`

### `get_status`

- Purpose: query current server status.
- Result:
  - `{ "running": <bool>, "library_name": <string|null>, "endpoints": [<string>, ...]|null, "error": <string> }`
  - `library_name` is the resolved library name passed on the command line.
  - `endpoints` is the non-empty list of loopback addresses (e.g. `"127.0.0.1:54321"`, `"[::1]:54321"`) the gRPC server is bound to when `running` is `true`; `null` otherwise. On a dual-stack host it holds the IPv4 and IPv6 loopback addresses sharing one port; on an IPv4-only or IPv6-only host it holds the single available family (ADR-052) — clients must pick a reachable address from the list rather than assume a fixed `[v4, v6]` pair.
  - `error` is present only when the gRPC server failed to start (for example, missing or unresolvable library name, or bind/listener setup failure); the key is omitted entirely when there is no startup failure to report.
  - `error`'s text is a sanitized, generic message — it never echoes the raw library-loading error (`libloading::Error`) or a filesystem path, since those can contain local path/OS detail. The full error is logged server-side via `tracing` instead (see ADR-089).

### `stop`

- Purpose: stop the gRPC server owned by the current process.
- Result:
  - `{ "stopped": true }` when the local server was running and stop was initiated.
  - `{ "stopped": false }` when the local server was already not running.
- After sending the `stop` response, the process exits.

## Unsupported JSON-RPC Methods

- `start` is not supported.
- Requests to start or stop another process via stdio are not part of this specification.

## Process Exit Rules

- The process exits when standard input is closed.
- On process exit caused by stdin closure, the process requests local gRPC server shutdown before terminating.
- The process exits after returning a response to `stop`.
- The stdio loop only reacts to stdin closure or a `stop` request; it does not poll the gRPC server task, so an independent gRPC server exit does not by itself terminate the stdio loop.

## JSON-RPC Notification Behavior

- Requests without `id` are treated as notifications and receive no response body.
- Notification methods can still affect lifecycle state (for example, `stop` notification triggers shutdown and exit).

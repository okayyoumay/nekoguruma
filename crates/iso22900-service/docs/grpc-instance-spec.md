# Revised Spec: Startup Argument and Stdio JSON-RPC for `iso22900-service`

Related maintenance note: [implementation-notes.md](implementation-notes.md)

Date: 2026-05-20

## Scope

This note defines the startup argument format and the JSON-RPC contract on standard input/output for `iso22900-service`.

## Startup Argument

- The first program argument must use this format:
  - `iso22900:<library name>?port=<grpc port number>&...`
- The query string is optional.
- `<library name>` is required.
- Query parameter `port` is optional.
- Unknown query parameters are ignored by the current implementation.
- Repeating `port` is rejected as invalid input.
- `port` must parse as a valid `u16` value.

### Examples

- `iso22900:MyPduLibrary`
- `iso22900:MyPduLibrary?port=60123`
- `iso22900:MyPduLibrary?port=60123&profile=test`

## Process Startup Behavior

1. The process parses the first startup argument.
2. The gRPC server starts immediately during process startup.
3. If `port` is provided, the gRPC server attempts to bind that port.
4. If `port` is omitted, the gRPC server binds an ephemeral local port.
5. There is no process-group identifier concept.
6. The process owns one local gRPC server instance.

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
  - `error` is present only when the gRPC server failed to start (for example, missing library name in RDF or bind/listener setup failure); the key is omitted entirely when there is no startup failure to report.
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
- If the local gRPC server task ends independently, the stdio loop exits as well.

## JSON-RPC Notification Behavior

- Requests without `id` are treated as notifications and receive no response body.
- Notification methods can still affect lifecycle state (for example, `stop` notification triggers shutdown and exit).

## Notes

- This specification replaces the previous startup-URI-based stdio contract.
- Cross-process grouping by identifier is not used.
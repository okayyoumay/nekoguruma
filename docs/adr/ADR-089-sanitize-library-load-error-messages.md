# ADR-089: Sanitize Library-Construction/Registry Error Messages (No Raw Filesystem Path / OS Error Text)

**Date:** 2026-07-16
**Status:** Accepted
**Affects:** `iso22900-service/src/error.rs`, `iso22900-service/tests/stdio_startup.rs`, `j2534-0404-service/src/error.rs`, `j2534-0404-service/src/service.rs`, `iso22900-registry/src/lib.rs`

## Context

A security review of error messages surfaced by the VCI services found that
`iso22900-service::error::map_construct_error`/`map_registry_error` and
`j2534-0404-service::error::map_construct_error` formatted the underlying
error's `Display` (or, for `map_construct_error` in `iso22900-service`,
`Debug`) directly into the `tonic::Status` message returned when the native
library fails to load or the registry/RDF lookup fails.

The wrapped error types (`iso22900_sys::libloading::Error`,
`j2534_0404_sys::libloading::Error`, `std::io::Error`, `url::ParseError`,
XML parse errors) render the full path the process attempted to open and,
for `libloading::Error`, the raw OS loader error text (e.g.
`dlopen`/`LoadLibraryW` failure strings). Both can contain local filesystem
detail — including, on a real deployment, a user's home directory name in
the library path.

Tracing the call sites showed both mapping functions are reached from
exactly one place each: `Iso22900Service::new()` / `J2534Service::new()`,
i.e. only during service construction, before the gRPC server starts
listening. A live gRPC client therefore never receives this `Status` over
the network — it can only reach the stdio JSON-RPC `get_status` response
that `vci-service-launcher` returns to the process that spawned this
service instance (see `vci-service-launcher/src/lib.rs`'s
`spawn_vci_context` call site, which explicitly passes the startup error
through "so callers can still query status even if the service failed to
start").

That JSON-RPC channel is local-only (inherited stdio pipes between a
launcher process and the service subprocess it spawned) and, in the case
exercised by `iso22900-service/tests/stdio_startup.rs`'s
`get_status_returns_startup_error_details_when_grpc_start_fails` (added by
ADR-030), the leaked path was one the same launcher had itself configured.
This is a weaker trust boundary than an external network gRPC client, but
still crosses a process boundary and the mapping functions are ordinary,
reusable error-mapping utilities with no guarantee a future caller won't
also reach them from a genuine per-request gRPC handler. Given that, and to
avoid a local filesystem path (username, directory layout) leaking into
whatever consumes `get_status`, we chose to sanitize this channel too
rather than carve out an exception for it.

## Decision

`map_construct_error` and `map_registry_error` (`iso22900-service`) and
`map_construct_error` (`j2534-0404-service`) now:

1. Log the full underlying error via `tracing::error!` (server-side only).
2. Return a generic, fixed message in the `Status`/JSON-RPC `error` field —
   the same category description as before (e.g. "failed to construct
   ISO22900 API", "I/O error while searching for PDU libraries"), but with
   no interpolated path or raw OS error text.

`iso22900_registry::RegistryError::NotFound(name)` is passed through
`map_registry_error` as-is (`Status::not_found`) when `name` is the PDU
library short name the caller requested — not a filesystem path, so echoing
it back is not a disclosure. However, `iso22900_registry`'s
`From<io::Error> for RegistryError` also produced a `NotFound` this way: any
`io::Error` with `ErrorKind::NotFound` (e.g. a missing
`/etc/pdu_api_root.xml`, or a missing Windows registry `Root File` value)
was converted to `RegistryError::NotFound(e.to_string())`, embedding the raw
OS error text (e.g. `"No such file or directory (os error 2)"`) in the same
variant the by-name lookups use — which `map_registry_error`'s `NotFound`
arm then forwarded to the client unsanitized (caught in Codex review of this
PR). Fixed by having that `From` impl log the real `io::Error` via
`tracing::debug!` and construct `NotFound` with a fixed, generic string
("D-PDU API root description file not found") instead of `e.to_string()`.

A further audit pass found `j2534-0404-service::Service::new()` had a second,
unmapped call: `j2534_0404_registry::resolve_library_path(...)?` used a bare
`?`, converting its `RegistryError` (whose `Io` variant wraps `std::io::Error`
and renders raw OS registry-lookup error text via `Display`) straight into
`BoxError` and on to the same JSON-RPC startup-status channel, bypassing
`map_construct_error` entirely. Added `map_registry_error` in
`j2534-0404-service/src/error.rs`, mirroring `iso22900-service`'s function:
`Io`/`RegistryUnsupported` are logged server-side and sanitized;
`RegistryError::NotFound(name)` is passed through as-is, since
`j2534-0404-registry`'s `NotFound` is only ever constructed from the
caller-supplied device name (`find_j2534_device_on_registry`, unlike
`iso22900-registry`'s, it has no `From<io::Error>` path that populates
`NotFound` with OS error text). Wired in via `.map_err(map_registry_error)?`
at the `resolve_library_path` call site.

Runtime error mapping (`map_runtime_error` in both services, and the
J2534/D-PDU `ApiStatus`/protocol-code error variants generally) is
unaffected — those carry protocol-level status codes and vendor-supplied
`GetLastError` text that callers need for normal operation, and in practice
never wrap a `LibraryLoad`/`libloading::Error` variant once construction has
already succeeded.

`stdio_startup.rs`'s `get_status_returns_startup_error_details_when_grpc_start_fails`
(ADR-030) was updated: it now asserts the `error` field does **not** contain
the configured library name/path, and does contain the generic sanitized
message, instead of asserting the opposite.

## Consequences

- A local launcher process that previously received the failing library's
  resolved path back via `get_status` (useful for diagnosing its own
  misconfiguration) must now consult the service subprocess's own
  `tracing` output (stderr/log sink) for that detail instead of the
  JSON-RPC response.
- `j2534-0404/src/error.rs` and `j2534-0500/src/lib.rs`'s `Error::Display`
  impls for `LibraryLoad` are unchanged (they still forward the raw
  `libloading::Error`) — sanitization is applied at the service/gRPC
  boundary (`error.rs` in each `*-service` crate), not the library crates
  themselves, since those crates are also usable directly (tests, future
  non-networked tooling) where full diagnostic detail is appropriate.
- `j2534-0500-service` has no equivalent mapping function yet (ADR-031's
  stub has no library-construction path reachable today); the same policy
  applies when one is added.

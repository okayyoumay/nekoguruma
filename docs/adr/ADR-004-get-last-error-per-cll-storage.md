# ADR-004: GetLastError — Per-CLL Last-Error Storage

**Date:** 2026-06-28  
**Status:** Superseded by ADR-105  
**Affects:** `j2534-0404-service/src/service.rs` (`LogicalLinkState`),
             `j2534-0404-service/src/service/events.rs` (`send_error_event`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_last_error`)

## Context

ISO 22900-2 §9.11 defines `PDU_GetLastError` as returning the most recent error
event for a given handle.  Callers that do not subscribe to event streams
(e.g. polling clients or test tools) rely on `GetLastError` to learn why a
primitive failed.

The original implementation always returned `PDU_ERR_EVT_NOERROR` regardless of
what had actually happened.

## Decision

`LogicalLinkState` gains a `last_error: Option<PduErrorEvent>` field, initialised
to `None` on link creation.

The `send_error_event` function (in `events.rs`) is extended to accept
`&Arc<Mutex<HashMap<u32, LogicalLinkState>>>` and to write the error into the
corresponding link's `last_error` field before emitting the gRPC event
notification.

`rpc_get_last_error` reads `last_error` when the request handle is a `CllHandle`.
System-level and module-level handles return `PDU_ERR_EVT_NOERROR` (no
service-level error storage is implemented).

## Alternatives Considered

1. **Ring buffer of recent errors per CLL** — more complete for ISO compliance but
   adds memory overhead and complexity for a feature that is rarely needed.
2. **Separate error map keyed by (cll_handle, cop_handle)** — useful when the
   caller needs the error for a specific COP, but `GetLastError` is defined as
   per-handle, not per-COP.

## Consequences

- `last_error` is only updated by the poll task (via `send_error_event`).
  Direct RPC-path errors (e.g. invalid arguments) are not stored.
- The stored error is not cleared between calls; repeated `GetLastError` calls
  return the same value until a new error occurs.
- `last_error` is `Copy` (`PduErrorEvent` is a C-like enum), so storing it in
  `LinkView` is cheap.

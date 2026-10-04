# ADR-105: Rich Error Model via `grpc-status-details-bin` Replaces `GetLastError`

**Date:** 2026-07-19
**Status:** Accepted (amended by ADR-112 — async error events now carry
`cop_handle` attribution; `SET_PROG_VOLTAGE`'s mapping corrected by
conformance-audit fix A2-21 — `ERR_PIN_INVALID` now maps to
`PduErrMuxRscNotSupported`, not folded into the blanket
`PduErrVoltageNotSupported` override)
**Affects:** `vci-service-interface` (proto, `rich_error.rs`), `iso22900-service`
             (`error.rs`, `service/rpc.rs`, `service/rpc_primitive.rs`,
             `service/rpc_link.rs`, `service/handles.rs`,
             `service/convert.rs`), `j2534-0404-service` (`error.rs`,
             `service.rs`, `service/rpc_primitive.rs`, `service/rpc_misc.rs`,
             `service/rpc_link.rs`, `service/rpc_module.rs`,
             `service/events.rs`), `j2534-0500-service`

## Context

`GetLastError` required a client to make a second RPC after any failure to
learn more about it: the synchronous `PDUError` returned directly by the
failing call was already known from that call's own `Status`, but a client
polling for the asynchronous error event associated with a Module/CLL handle
(ADR-004, ADR-020) had to issue a follow-up `GetLastError(module_handle)` /
`GetLastError(cll_handle)`. This is a race (the handle's tracked error can
change between the failing call and the follow-up query) and a redundant
round trip, since the same asynchronous error information is already
delivered continuously via `SubscribeEvent`/`GetEventItem`'s `error_data`
`EventItem` variant.

Separately, `iso22900-service`'s `map_runtime_error` discarded the exact
native `PDUError` code every failing D-PDU call already carries
(`DPduApiError::PduError(u32)`), collapsing every runtime failure to a bare
`Status::internal(...)` string. `j2534-0404-service` had no equivalent
conversion at all for its adapter-emulated D-PDU error codes.

## Decision

1. **Remove `GetLastError`, `GetLastErrorRequest`, and `ErrorEventResponse`
   from `service.proto`.** Async/background errors detected with no RPC in
   flight for a handle (e.g. a poll-task-detected hard channel error) are no
   longer independently queryable at all -- they remain visible only through
   `SubscribeEvent`/`GetEventItem`. This is an intentional capability
   removal, not an oversight: a client that needs to observe them must
   already be draining the event queue. In `j2534-0404-service`, this means a
   CLL-scoped async error event is enqueued into the same per-CLL event
   queue as received frames (`LogicalLinkState::rx_buf`, retyped to
   `CllQueueItem::{Frame, Error}`) -- matching ISO 22900-2's single typed
   per-handle event queue (`PDU_IT_RESULT`/`PDU_IT_ERROR`), not two
   independent streams -- and drained by `GetEventItem` in true arrival
   order, under the CLL's `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`
   capacity/eviction policy, exactly like received frames (Codex review,
   round 12: the first implementation only notified an active `SubscribeEvent`
   subscriber, leaving `GetEventItem` pollers with no way to observe these
   events at all). In `PDU_QUE_LIMITED` (`DiscardNewest`) mode, a full queue
   drops the error item for pollers the same way it would drop a frame --
   `SubscribeEvent` subscribers and a later failing RPC's
   `ErrorDetail.error_event_data` (`LogicalLinkState::last_error`, tracked
   independently of the queue) still observe it either way. `GetEventItem`
   pollers experience the same behavior `PDU_IOCTL_CLEAR_RX_QUEUE` already
   had for frames: a clear discards queued error items along with frames.
   **Accepted residual, not fixed by this revision (partially closed by a
   later P2 follow-up):** `send_cll_status`/`send_cop_status` (CLL/COP status
   transition events) remained subscription-only, the same architectural gap
   this revision closes for error events, one item-kind over. A later P2
   follow-up added `CllQueueItem::Status` and re-routed `send_cll_status`
   through `deliver_or_enqueue` (the same `push_cll_event`-backed mechanism
   `Error` uses), so a CLL status transition now reaches `GetEventItem`
   pollers too. `send_cop_status` is still subscription-only: unlike `Error`,
   virtually every call site holds `primitives` locked across the emission
   (ADR-128's atomicity guarantee for `primitives`/`terminal_cops`), and at
   least one (`dispatch_tx_item`'s WAITING/CANCELLED tail) also holds
   `logical_links` -- wiring it the same way would violate this crate's
   documented `logical_links` -> `primitives` lock hierarchy (self-deadlock
   at that one site), not just be a small follow-up. See
   `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog for
   the current state of both halves.

2. **Add `ErrorDetail`/`ErrorEventData` messages**, attached to a failing
   RPC's `Status` instead:

   ```proto
   message ErrorEventData {
       PDUErrorEvent error_event = 1;
       optional ComPrimitiveHandle cop_handle = 2;
       uint32 timestamp = 3;
       uint32 extra_error_info = 4;
   }

   message ErrorDetail {
       PDUError pdu_error = 1;
       optional ErrorEventData error_event_data = 2;
       optional string detail_text = 3;
   }
   ```

   `pdu_error` is the synchronous function-return code (what the failing
   call itself means to report). `error_event_data` is the
   `PDUGetLastError`-equivalent -- the most recent asynchronous error event
   for the Module/CLL handle in scope at failure time, fetched or read
   best-effort, so a client gets what a follow-up `GetLastError` call would
   have told it in the same response. `detail_text` carries vendor/adapter
   free text (J2534 `PassThruGetLastError`, ADR-026) when available.

3. **Wire mechanism: hand-written `google.rpc.Status` + `google.protobuf.Any`**,
   not a vendored second `.proto` file, not the `tonic-types` crate.
   `tonic-types`'s `ErrorDetail` enum is fixed to ten standard Google detail
   types with no variant for an arbitrary/custom `Any`-packed message, so it
   cannot carry our own `ErrorDetail`. `tonic::Status::with_details(code,
   message, details: Bytes)` stores opaque bytes as the
   `grpc-status-details-bin` trailer; the gRPC rich-error-model convention is
   that those bytes are a serialized `google.rpc.Status{code, message,
   details: repeated Any}`. Both `google.rpc.Status` and `google.protobuf.Any`
   are fixed, external, 2-3 field schemas, so `vci-service-interface/src/rich_error.rs`
   hand-writes them as plain `#[derive(prost::Message)]` structs rather than
   adding a second protoc compilation unit -- the wire encoding is identical
   either way. `status_with_error_detail`/`error_detail_from_status` are the
   sole encode/decode entry points, living in `vci-service-interface` since
   both service crates already depend on it.

4. **Scope rule: attach `ErrorDetail` only where ISO 22900-2 actually defines
   a return code for the failure** -- native D-PDU/J2534 call failures, and
   emulated D-PDU handle/state-machine rejections (unknown handle, CLL
   not-connected/not-started, resource locked, queue full, ...). Pure
   request-shape/transport validation failures (a missing required proto
   field, an unresolvable oneof, a malformed shortname before any native call
   or state check was attempted) get a plain `Status` with no `ErrorDetail`,
   exactly as before -- they never had a `GetLastError`-queryable analog
   either, and inventing a `PDUError` for them would be exactly the
   speculative-abstraction the project's design principles prohibit.

5. **`iso22900-service` integration:** `map_runtime_error` now builds an
   `ErrorDetail` from `DPduApiError` (`DPduApiError::PduError(code)` casts
   directly onto `ErrorDetail.pdu_error` -- both are the same ISO 22900-2
   values, and prost enums are open `i32`s so unmapped/vendor codes still
   round-trip; every other `DPduApiError` variant falls back to
   `PduErrFctFailed` and keeps its ADR-089 message sanitization). A new
   `with_api_for_link`/`map_runtime_error_for_link` path additionally makes a
   best-effort native `PDUGetLastError` call (module+CLL scope) to populate
   `error_event_data`, applied to every RPC whose Module+CLL handle is
   unconditionally real (Create/Destroy/Connect/DisconnectComLogicalLink,
   Lock/UnlockResource, Get/SetComParam, Start/CancelComPrimitive).
   `GetStatus`/`GetEventItem`/`SubscribeEvent`/`IoCtl`/`Get`/`SetUniqueRespIdTable`
   were deliberately left on the plain (no `error_event_data`) path: their
   handle can be a System/Module/Cop scope with no real CLL, or (for the
   UniqueRespIdTable pair) making that call link-aware would require
   threading a Module/CLL pair through `convert.rs` helpers shared with
   non-CLL-scoped call sites for a marginal benefit. If a future client need
   emerges, extending coverage there is a small, isolated follow-up.

6. **`j2534-0404-service` integration:** no return-type refactor of fallible
   functions -- `error.rs` adds `pdu_error_for(StatusCode) -> PduError` (an
   exhaustive match table, see below), `map_native_error_for_link`/
   `map_native_error_as` (native call failures, optionally attaching the
   already-tracked `LogicalLinkState::last_error`/`ModuleState::last_error`
   `PduErrorEvent` -- no new native call needed here, unlike
   `iso22900-service`, since this adapter already tracks that state per
   ADR-004/ADR-020), `unknown_handle_status` (`PDU_ERR_INVALID_HANDLE` +
   `NotFound`, for an unrecognized handle *or* filter-number reference), and
   `state_guard_status` (caller-supplied `Code`+`PduError`, for emulated
   D-PDU state-machine rejections). Roughly 70 call sites across
   `rpc_primitive.rs`/`rpc_misc.rs`/`rpc_link.rs`/`rpc_module.rs`/`service.rs`
   were converted; the ~84 pure `invalid_argument` proto-validation sites
   were left untouched per the scope rule above.

   `StatusCode` &rarr; `PduError` table (fallback `PduErrFctFailed` for
   anything unmapped/vendor-specific):

   | J2534 `StatusCode` | `PduError` | | J2534 `StatusCode` | `PduError` |
   |---|---|---|---|---|
   | `ERR_FAILED` | `FctFailed` | | `ERR_NULL_PARAMETER` | `InvalidParameters` |
   | `ERR_INVALID_FLAGS` | `InvalidParameters` | | `ERR_INVALID_MSG` | `InvalidParameters` |
   | `ERR_INVALID_TIME_INTERVAL` | `InvalidParameters` | | `ERR_MSG_PROTOCOL_ID` | `InvalidParameters` |
   | `ERR_NOT_UNIQUE` | `InvalidParameters` | | `ERR_DEVICE_NOT_CONNECTED` | `CommPcToVciFailed` * |
   | `ERR_TIMEOUT` | `FctFailed` * | | `ERR_BUFFER_FULL` | `TxQueueFull` |
   | `ERR_BUFFER_EMPTY` | `EventQueueEmpty` | | `ERR_CHANNEL_IN_USE` | `ResourceBusy` |
   | `ERR_DEVICE_IN_USE` | `SharingViolation` | | `ERR_NOT_SUPPORTED` | `FctFailed` * |
   | `ERR_INVALID_IOCTL_ID` | `IdNotSupported` | | `ERR_INVALID_PROTOCOL_ID` | `IdNotSupported` * |
   | `ERR_EXCEEDED_LIMIT` | `ResourceError` | | `ERR_BUFFER_OVERFLOW` | `ResourceError` |
   | `ERR_PIN_INVALID` | `PinNotConnected` * | | `ERR_INVALID_IOCTL_VALUE` | `ValueNotSupported` * |
   | `ERR_INVALID_BAUDRATE` | `ValueNotSupported` * | | `ERR_INVALID_CHANNEL_ID` | `InvalidHandle` |
   | `ERR_INVALID_DEVICE_ID` | `InvalidHandle` | | `ERR_INVALID_MSG_ID` | `InvalidHandle` * |
   | `ERR_INVALID_FILTER_ID` | `InvalidHandle` | | `ERR_NO_FLOW_CONTROL` | `FctFailed` * |

   `*` marks a judgment call, not a literal 1:1 spec mapping: `ERR_DEVICE_NOT_CONNECTED`
   means "device dropped after a successful open" (SAE J2534-1 §7.2), distinct
   from the adapter's own `PDU_ERR_MODULE_NOT_CONNECTED` not-yet-connected
   guard; ISO 22900-2's `PDUError` has no synchronous timeout code (async
   timeouts are `PDU_ERR_EVT_RX_TIMEOUT` on the event queue), so
   `ERR_TIMEOUT` folds onto `FctFailed`; J2534's `ERR_NOT_SUPPORTED` is
   function-scoped while ISO 22900-2's `PDU_ERR_ID_NOT_SUPPORTED` is
   id-scoped, so they are not the same condition; `ERR_NO_FLOW_CONTROL`
   escaping to a client would indicate an adapter filter-management bug
   (ADR-039/048 own FLOW_CONTROL_FILTER management), not a client-actionable
   condition.

   `PDU_IOCTL_SET_PROG_VOLTAGE` is a partial override (`map_native_error_as`):
   `ERR_PIN_INVALID` maps to `PduErrMuxRscNotSupported` (Table 49's code for
   a pin or resource that the module does not support), and every other native
   failure means `PduErrVoltageNotSupported`, matching this crate's
   pre-existing `"PDU_ERR_VOLTAGE_NOT_SUPPORTED: ..."`-prefixed message text
   for that default case (conformance-audit fix A2-21, corrected from this
   ADR's original one-code-for-everything mapping).

   Four sites (two in `rpc_misc.rs`, two in `rpc_link.rs`) carried
   pre-existing message text prefixed `"PDU_ERR_FUNCTION_NOT_SUPPORTED:"` --
   that name was never an actual `PDUError` enum variant (it does not appear
   in `service.proto`). These are adapter-level structural limitations (e.g.
   "cannot scope `PDU_IOCTL_START_MSG_FILTER` to one CLL when its physical
   channel is shared with another CLL"), not a value/id rejection or a
   resource-lock condition, so they now use `PduErrFctFailed` (the generic
   function-failed code) via `state_guard_status`, and the message text
   prefix was corrected to `"PDU_ERR_FCT_FAILED:"` to match. A fifth site (a
   `not_found` for an unrecognized client filter `FilterNumber`) was folded
   into `unknown_handle_status` alongside the CLL/COP/module handle-lookup
   sites, since a filter number is the same "client referenced an ID this
   service does not currently track" shape.

## Consequences

- **Breaking wire change.** Any client that called `GetLastError` must be
  updated to read `ErrorDetail` off a failing call's `Status` (via
  `google.rpc.Status` in the `grpc-status-details-bin` trailer, or this
  repo's `error_detail_from_status` helper for Rust callers) instead, and
  must switch to `SubscribeEvent`/`GetEventItem` for the async/background
  polling use case ADR-004/ADR-020 previously served through `GetLastError`.
- **ADR-004 is superseded by this ADR.** Its RPC read path
  (`rpc_get_last_error`) no longer exists; the per-CLL `last_error` *storage*
  it introduced is retained unchanged in `j2534-0404-service` and now feeds
  `ErrorDetail.error_event_data` instead of a `GetLastError` response.
- **ADR-020 is amended, not superseded.** Its module-level `ModuleState`
  tracking survives fully intact (still read by `GetStatus`/`GetEventItem`);
  only its `GetLastError(module_handle)` read path is gone, replaced the same
  way as ADR-004's CLL-level path.
- **ADR-026 is unaffected.** Its auto-fetched `PassThruGetLastError`
  description is exactly what now feeds `ErrorDetail.detail_text` for
  J2534-originated failures.
- `iso22900-service` cannot supply vendor free text for `detail_text` the way
  `j2534-0404-service` can -- the real D-PDU API's `PDUGetLastError` has no
  text field, only structured codes/timestamps. This is a pre-existing
  platform capability difference, not something introduced here.
- Some interop risk: a small number of intermediary gRPC proxies strip
  binary (`-bin`-suffixed) trailer metadata. A client behind such a proxy
  would see the plain `Status` code/message but not `ErrorDetail`, same as
  before this change existed at all (no functional regression, just no new
  capability through that path).

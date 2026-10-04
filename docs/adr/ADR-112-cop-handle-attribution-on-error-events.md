# ADR-112: `cop_handle` Attribution on Async Error Events and the Last-Error Snapshot

**Date:** 2026-07-22
**Status:** Accepted (amends ADR-105; corrects a claim in ADR-022)
**Affects:**
- `j2534-0404-service/src/service.rs` (`TrackedError`, `CopRef`, `ModuleState::default`)
- `j2534-0404-service/src/service/events.rs` (`send_error_event`,
  `send_tester_present_once`, `frame_tester_present_data`,
  `handle_channel_hard_error`, `handle_send_recv`, `handle_start_comm`,
  `handle_stop_comm`, `handle_update_param`,
  `wait_for_expected_response_inner`, `dispatch_due_tester_present`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_event_item`)
- `j2534-0404-service/src/service/rpc_misc.rs` (`PDU_IOCTL_CLEAR_PERIODIC_MSGS`)
- `j2534-0404-service/src/error.rs` (`error_event_data_for`)

## Context

Conformance audit finding A2-1
(`j2534-0404-service/docs/iso22900-2-conformance-audit.md`) identified that
async error events never carried `cop_handle` attribution:

- ISO 22900-2 §9.6.2 — an async error may concern the MVCI protocol
  module (for instance a hardware fault), a ComLogicalLink (for instance a
  CAN bus error) or one particular ComPrimitive (for instance an ECU
  timeout), and the spec says that this association is conveyed through the
  corresponding handle.
- ISO 22900-2 §9.4.7 / §9.4.7.1 — the last-error snapshot's `phCoP` field is
  `PDU_HANDLE_UNDEF` when the error is not COP-related, and may legitimately
  reference a COP that has since finished (chronological attribution is the
  event queue's job, not the snapshot's).
- Native precedent: `PDU_EVENT_ITEM.hCop`
  (`iso22900-sys/src/bindings/d_pdu_api_defs.h:374`) carries this on every
  event item, `PDU_HANDLE_UNDEF` when not COP-related.

`error_event_data_for` (`error.rs`) hardcoded `cop_handle: None`
unconditionally, `send_error_event` (`events.rs`) took no COP-handle
parameter at all, and the `GetEventItem` drain
(`rpc_primitive.rs::rpc_get_event_item`) also hardcoded `cop_handle: None`
for queued error items. Concretely: two concurrent `CoptSendrecv` COPs on one
CLL, one hits an N_Bs-style receive timeout — the client's error event had no
way to tell which COP failed.

ADR-105 introduced the `ErrorEventData.cop_handle` proto field and the
`TrackedError`/`last_error` snapshot mechanism itself, but its own scope
never addressed attribution — this is a gap in that ADR's coverage, not a
documented tradeoff, and is amended here.

Separately, ADR-022 ("ReceivedFrame cop_handle Attribution") states as
settled: *"`CllStatus` and `ErrorData` notifications (`send_cll_status`,
`send_error_event`) always pass `None` — they are not COP-result items."*
This decision made that sentence's `ErrorData`/`send_error_event` half
factually incorrect going forward; ADR-022's own subject (`ReceivedFrame`
attribution for `PDU_IT_RESULT` items) is unaffected and remains correct.

## Decision

### `TrackedError` gains COP attribution, not just `send_error_event`'s parameter list

```rust
pub(super) struct TrackedError {
    pub(super) event: vci_service_interface::PduErrorEvent,
    pub(super) timestamp: u32,
    pub(super) cop: Option<CopRef>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CopRef {
    pub(super) cll_handle: u32,
    pub(super) cop_handle: u32,
}
```

A parameter threaded only through `send_error_event`'s own body could not
reach the `GetEventItem` drain path: `CllQueueItem::Error` *stores* a
`TrackedError`, and `rpc_get_event_item` rebuilds the proto `EventItem` from
that stored value later, on a separate call. Putting `cop` on `TrackedError`
itself means the queued-poll path, the live `SubscribeEvent` notification,
and the RPC-fallback snapshot (`error_event_data_for`) all derive from one
source of truth instead of three independently-threaded parameters that
could drift out of sync.

### `send_error_event` takes `cop_handle: Option<u32>`

Matching the existing precedent of `make_cll_notification`
(`events.rs::make_cll_notification`) and `send_cop_status`, which already
take/build `Option<u32>` → `ComPrimitiveHandle` with `DEFAULT_MODULE_HANDLE`
+ the CLL handle. Every call site was updated to pass the objectively correct
value for its calling context:

- `None` — genuinely module/CLL-scoped, no COP executing:
  `handle_channel_hard_error`'s lost-comm-to-VCI broadcast, and
  `PDU_IOCTL_CLEAR_PERIODIC_MSGS`'s channel-wide administrative error
  (`rpc_misc.rs`) — it clears tester-present for every CLL sharing the
  channel, not one COP's own.
- `Some(cop_handle)` — every error raised from inside a specific COP's own
  execution: `handle_send_recv`, `handle_start_comm`, `handle_stop_comm`,
  `handle_update_param`, `wait_for_expected_response_inner`, each already
  had the relevant COP's own `cop_handle` parameter in scope.

### The tester-present dual-context split

`send_tester_present_once` and `frame_tester_present_data` are called from
**two** different contexts, discovered while implementing this fix (the
original brief assumed a single answer for both):

- Inside `handle_start_comm`'s initial arm and `handle_update_param`'s
  re-arm — driven by that COP's own execution, correctly `Some(cop_handle)`.
- From `dispatch_due_tester_present`'s periodic/idle-triggered background
  tick — not driven by any currently-executing COP (the COP that originally
  armed tester-present has long since finished), correctly `None`.

Both helpers gained their own `cop_handle: Option<u32>` parameter so each
caller supplies the value correct for its own context, rather than hardcoding
one answer for a function invoked from two semantically different call
chains. A repo-wide sweep for other functions shared between a COP-driven
path and a COP-less administrative/background path
(`transmit_request`/`transmit_request_inner`, `isotp_send`,
`write_can_frame`, `wait_for_p3_gap`, `poll_rx`/`poll_rx_inner`) found none
that call `send_error_event` internally — they return outcome enums and let
the (already correct) caller decide, so this dual-context class is not
believed to recur elsewhere in this revision's scope.

### The RPC-fallback snapshot inherits the same value, staleness included

`error_event_data_for` (`error.rs`) and the `GetEventItem` drain
(`rpc_primitive.rs`) both build `cop_handle` from `tracked.cop` via
`ComPrimitiveHandle { module_handle: DEFAULT_MODULE_HANDLE, cll_handle,
cop_handle }` instead of hardcoding `None`. The single CLL-level `last_error`
slot remains last-writer-wins across concurrent COPs (unchanged from
ADR-105) — per §9.4.7.1 this snapshot is explicitly allowed to be stale or
reference an already-finished COP; a later, unrelated RPC failure on the
same CLL may legitimately surface the last tracked error's COP handle even
if that COP has since finished. No "clear `cop` when the COP is torn down"
logic was added — that would contradict the spec-permitted staleness this
ADR relies on.

The audit's own concurrency scenario (two COPs, one CLL-level snapshot slot)
is resolved by the *event queue* carrying per-event attribution — each
`CllQueueItem::Error` keeps its own `cop`, so COP A's queued event is
unaffected by COP B's error later overwriting the shared `last_error` slot —
not by adding multiple snapshot slots.

## Amendment to ADR-022

ADR-022's Decision section states: *"`CllStatus` and `ErrorData`
notifications (`send_cll_status`, `send_error_event`) always pass `None` —
they are not COP-result items."* This is superseded for the `ErrorData`/
`send_error_event` half by this ADR: `send_error_event` now passes
`Some(cop_handle)` whenever the error originates from a specific COP's own
execution, per the rule above. ADR-022's `ReceivedFrame`/`CllStatus` subject
matter is otherwise unaffected.

## Consequences

- `GetEventItem`, `SubscribeEvent`, and the RPC-fallback `ErrorDetail`
  snapshot now agree on `cop_handle` for COP-scoped async errors, closing
  audit finding A2-1.
- `TrackedError` grows by one `Option<CopRef>` (two `u32`s behind a niche
  tag); remains `Copy`.
- `DEFAULT_MODULE_HANDLE` (`service.rs`) is now `pub(crate)` (was private) so
  `error.rs` can build the same `ComPrimitiveHandle` shape as the async
  paths — a minimal, crate-internal visibility increase, no external API
  change.
- A later, unrelated RPC failure on a CLL may report a stale COP's handle in
  `ErrorDetail.error_event_data.cop_handle` if that COP's error was the last
  one tracked — accepted, spec-permitted (§9.4.7.1), not a defect. Do not
  "fix" this by clearing `cop` on COP teardown.

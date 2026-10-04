# ADR-022: ReceivedFrame cop_handle Attribution

**Date:** 2026-06-29
**Status:** Accepted (the `ErrorData`/`send_error_event` claim below is
corrected by ADR-112 — see note)
**Affects:**
- `j2534-0404-service/src/service.rs` (`ReceivedFrame`)
- `j2534-0404-service/src/service/events.rs` (`make_cll_notification`, `poll_rx`,
  `handle_start_comm`, `poll_rx_and_check_match`, `wait_for_expected_response`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_event_item`)

## Context

Per ISO 22900-2, `PDUGetEventItem` returns a `PDU_EVENT_ITEM` whose `hCop` field
identifies the ComPrimitive that generated a `PDU_IT_RESULT` item, when the item
is attributable to one. The J2534 v0404 adapter's `EventItem.cop_handle` was
always `None` for `ResultData` items, on both the `GetEventItem` (polling) and
`SubscribeEvent` (streaming) paths — there was no way to tell which `CoptSendrecv`
or `CoptStartcomm` call, if any, a given received frame belonged to.

This mirrors the gap ADR-009 closed for `unique_resp_identifier`/`acceptance_id`:
`ReceivedFrame` (the struct buffered in each CLL's `rx_buf`) had no field to
carry COP attribution, so it could not be reconstructed later by either
consumption path.

## Decision

### ReceivedFrame gains a cop_handle field

```rust
pub(super) struct ReceivedFrame {
    pub(super) rx_status: u32,
    pub(super) timestamp: u32,
    pub(super) data: Vec<u8>,
    pub(super) unique_resp_identifier: u32,
    pub(super) acceptance_id: u32,
    pub(super) cop_handle: Option<u32>,  // ← new
}
```

`make_cll_notification` (used by both the `rx_buf` push and the
`SubscribeEvent` notification at every call site) takes a matching
`cop_handle: Option<u32>` parameter, keeping the two paths consistent.

### Attribution rule

A frame is attributed to a specific COP only when it can be deterministically
tied to one in-flight ComPrimitive:

- **`poll_rx`** (background 10 ms timer poll, outside any COP's wait loop) —
  always `None`. There is no COP actively awaiting a response when this path
  runs.
- **`handle_start_comm`**'s init-response frame — always `Some(cop_handle)` of
  the `CoptStartcomm` call that triggered it. The frame is the direct, sole
  result of that COP.
- **`poll_rx_and_check_match`** (driving `wait_for_expected_response` during
  `CoptSendrecv`) — `Some(cop_handle)` only for the frame on `target_cll` that
  either satisfies an `ExpectedResponse` (the final match) or carries a
  pending-response NRC (0x78/0x21/0x23, per ADR-018). All other frames —
  traffic on other CLLs sharing the channel, or non-matching/non-pending
  frames on `target_cll` once a match has already been recorded — remain
  `None`.

This rule is safe because each physical channel has exactly one `tx_queue`
processed strictly FIFO (see `TxItem` dispatch in `poll_channel_events`): at
most one COP per channel is ever "awaiting a response" at a time, so there is
no ambiguity about which COP a matched or pending-RC frame belongs to.

### GetEventItem and SubscribeEvent both surface it

`rpc_get_event_item`'s CLL branch now maps `frame.cop_handle` into
`EventItem.cop_handle` via `ComPrimitiveHandle { module_handle:
DEFAULT_MODULE_HANDLE, cll_handle, cop_handle }`, identical to how
`rpc_start_com_primitive` builds the same handle type. `CllStatus`
notifications (`send_cll_status`) always pass `None` — CLL status is never
COP-scoped. **`ErrorData` notifications (`send_error_event`) originally
always passed `None` here too, but this is corrected by ADR-112**: an error
raised from inside a specific COP's own execution (e.g. an N_Bs-style
receive timeout) now carries that COP's handle, per ISO 22900-2 §9.4.7 c) /
§9.6.2 — only errors that are genuinely module- or CLL-scoped (no COP
executing) still pass `None`.

## Alternatives Considered

1. **Attribute every frame on target_cll to cop_handle while a COP is waiting**
   — Rejected. A CLL can receive unsolicited or unrelated traffic (e.g. other
   ECUs on a shared bus) while a COP is waiting for its specific response;
   attributing all of it to that COP would be misleading.

2. **Track attribution only on the streaming path, leave GetEventItem as
   `None`** — Rejected for the same reason ADR-009 rejected it: it leaves
   polling clients permanently unable to correlate result items to the COP
   that produced them, contrary to the ISO 22900-2 contract this service
   adapts to.

## Consequences

- `GetEventItem` and `SubscribeEvent` now agree on `cop_handle` for every
  `ResultData` item, mirroring the existing parity for
  `unique_resp_identifier`/`acceptance_id`.
- `ReceivedFrame` grows by one `Option<u32>` (effectively a `u32` plus a niche
  tag) per buffered frame; at 64 frames max per CLL this is negligible.
- Frames outside an active request/response cycle (background poll, or
  non-matching/non-pending traffic during a wait) still report
  `cop_handle: None`. Callers must not treat `None` as an error — it correctly
  reflects that ISO 22900-2 only ties `hCop` to result items where the
  originating COP is known.

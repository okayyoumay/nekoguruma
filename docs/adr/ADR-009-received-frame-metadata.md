# ADR-009: ReceivedFrame Extended Metadata (unique_resp_identifier / acceptance_id)

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:**
- `j2534-0404-service/src/service.rs` (`ReceivedFrame`)
- `j2534-0404-service/src/service/events.rs` (`poll_rx`, `poll_rx_and_check_match`, `handle_start_comm`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_event_item`)

## Context

ADR-007 introduced UniqueRespIdTable routing in `poll_rx`: each received frame is
matched against the CLL's routing table and the matching `unique_resp_identifier`
is forwarded in `ResultData` notifications to `SubscribeEvent` streaming clients.

However, `ReceivedFrame` — the struct stored in the per-CLL `rx_buf` ring buffer
for `GetEventItem` polling clients — contained only `rx_status`, `timestamp`, and
`data`.  `GetEventItem` therefore always returned `unique_resp_identifier: 0` and
`acceptance_id: 0`, making the polling path inconsistent with the streaming path.

The same inconsistency affected `poll_rx_and_check_match` (used by
`wait_for_expected_response` during `CoptSendrecv`): it did not apply
UniqueRespIdTable routing at all, so frames were delivered to all CLLs regardless
of their routing table, and `unique_resp_identifier` was always 0.

## Decision

### ReceivedFrame gains two new fields

```rust
pub(super) struct ReceivedFrame {
    pub(super) rx_status: u32,
    pub(super) timestamp: u32,
    pub(super) data: Vec<u8>,
    pub(super) unique_resp_identifier: u32,  // ← new
    pub(super) acceptance_id: u32,           // ← new
}
```

`unique_resp_identifier` is populated from the UniqueRespIdTable routing result.
`acceptance_id` is populated from the matching `ExpectedResponse` descriptor when
the frame is received during a `CoptSendrecv` request/response cycle; it is 0 for
all other frames (including those received via the background `poll_rx` timer).

### poll_rx and poll_rx_and_check_match unified via shared helpers

Both functions now use:

- **`CllRxEntry`** (module-level struct) — per-CLL routing snapshot.
- **`route_frame(entry, frame_can_id) -> Option<u32>`** — determines the
  `unique_resp_identifier` for a given frame/CLL pair, returning `None` to drop.
- **`build_cll_rx_entries(...) -> Vec<CllRxEntry>`** — snapshots all CLLs on a
  channel under a single double-lock (logical_links + subscriptions).

`poll_rx_and_check_match` now applies the same routing as `poll_rx`, and restricts
the expected-response match check to frames that are actually routed to
`target_cll`.  The `acceptance_id` of the first matching `ExpectedResponse`
descriptor is set in both the `ReceivedFrame` stored in `rx_buf` and the
`ResultData` notification sent to `SubscribeEvent` subscribers.

### GetEventItem returns the stored metadata

`rpc_get_event_item` now reads `frame.unique_resp_identifier` and
`frame.acceptance_id` from the popped `ReceivedFrame` instead of hard-coding 0.

## Alternatives Considered

1. **Store metadata only in SubscribeEvent; keep GetEventItem returning 0** —
   Simpler but inconsistent.  Polling clients cannot correlate frames to ECU
   routing entries or expected-response descriptors.

2. **Store only `unique_resp_identifier`, not `acceptance_id`** — `acceptance_id`
   is only non-zero in a request/response cycle, so its overhead is minimal.
   Omitting it would require callers to re-derive the matching descriptor from
   the raw frame data.

3. **Merge poll_rx and poll_rx_and_check_match into a single function with an
   optional match callback** — Cleaner single point of truth but harder to read.
   The shared helper approach (`build_cll_rx_entries` / `route_frame`) achieves
   the same reuse without merging the two call sites.

## Consequences

- `GetEventItem` polling clients and `SubscribeEvent` streaming clients now see
  identical `unique_resp_identifier` and `acceptance_id` values for the same frame.
- `ReceivedFrame` is slightly larger (two additional u32 fields per buffered frame).
  At 64 frames max per CLL the added cost is 512 bytes per CLL — negligible.
- `acceptance_id` is 0 for all frames received outside a `CoptSendrecv` cycle.
  Callers should not interpret `acceptance_id: 0` as "no match" when
  `expected_response_array` was empty (fire-and-forget mode).

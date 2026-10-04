# ADR-014: unique_resp_ids Filter in Expected-Response Matching

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`poll_rx_and_check_match`)

## Context

`ExpectedResponse.unique_resp_ids: Vec<u32>` carries a list of
`unique_resp_identifier` values drawn from the CLL's `UniqueRespIdTable`.
When non-empty it restricts which ECUs can satisfy the expected-response
descriptor: only frames routed to the target CLL whose
`unique_resp_identifier` appears in the list are eligible to trigger
`PduCopstFinished`.

Before this fix the field was populated from the proto (via `StartComPrimitive`)
and stored in `ExpectedResponse` but never consulted during frame matching in
`poll_rx_and_check_match`.  Every frame that passed UniqueRespIdTable routing and
matched the mask/pattern was accepted, regardless of which ECU it came from.

**Bug (L-POST-2):**  
A caller that configured per-ECU routing entries (e.g. ECU-A at
`unique_resp_identifier = 1`, ECU-B at `unique_resp_identifier = 2`) and set
`unique_resp_ids = [1]` on an `ExpectedResponseData` expected to wait only for
ECU-A's response.  Instead the service accepted whichever ECU responded first.

## Decision

In `poll_rx_and_check_match`, the `find` predicate is extended to gate on
`unique_resp_identifier` before evaluating the mask/pattern:

```rust
if let Some(d) = expected.iter().find(|e| {
    (e.unique_resp_ids.is_empty()
        || e.unique_resp_ids.contains(&unique_resp_identifier))
        && e.matches(&data)
}) {
```

The `unique_resp_identifier` for the frame is already in scope — it is returned
by `route_frame(entry, frame_can_id)` earlier in the same loop iteration.

Key properties of this approach:

1. **Empty `unique_resp_ids` → no restriction** — any frame that passes routing
   and mask/pattern matching triggers the match, preserving backward-compatible
   fire-and-wait semantics.  Callers that do not set `unique_resp_ids` are
   unaffected.

2. **Non-empty `unique_resp_ids` → ECU-identity gate** — the frame's
   `unique_resp_identifier` (derived by `route_frame` from the CLL's
   `UniqueRespIdTable`) must appear in the list before the mask/pattern is
   evaluated.  This matches the ISO 22900-2 intent: `unique_resp_ids` identifies
   the UniqueRespIdTable entries whose ECUs are eligible to respond.

3. **Frame delivery is unaffected** — frames are still pushed into `rx_buf` and
   forwarded to `SubscribeEvent` subscribers regardless of `unique_resp_ids`.
   The filter only governs whether the frame causes `PduCopstFinished` for the
   in-progress `CoptSendrecv` operation.

4. **No-table mode (`unique_resp_identifier = 0`)** — when the CLL has no
   `UniqueRespIdTable`, all frames receive `unique_resp_identifier = 0`.
   A caller that sets `unique_resp_ids = [non-zero]` will never match.
   Callers must either leave `unique_resp_ids` empty or configure a
   `UniqueRespIdTable` before using this field.

## Alternatives Considered

1. **Add `unique_resp_identifier` parameter to `ExpectedResponse::matches()`** —
   `matches()` is a pure data function (mask-and-pattern comparison on raw bytes);
   mixing routing state into it conflates two separate concerns and makes the
   method harder to test in isolation.  Rejected.

2. **Filter at the routing step (`route_frame`)** — `route_frame` decides whether
   to deliver a frame to a CLL at all; it has no knowledge of which `CoptSendrecv`
   operation is in progress.  Injecting `unique_resp_ids` awareness there would
   require passing the current expected-response list into every `poll_rx` call,
   including the background timer path where no `CoptSendrecv` is active.
   Rejected.

3. **Store `unique_resp_ids` in `LogicalLinkState` during an active `CoptSendrecv`**
   — Avoided adding shared mutable state when the information is already available
   in the `expected` slice passed directly to `poll_rx_and_check_match`.  Any state
   stored in `LogicalLinkState` requires a lock acquisition; the current approach
   is lock-free for this check.

## Consequences

- `ExpectedResponse.unique_resp_ids` is now fully functional; the `dead_code`
  compiler warning for the unused field is eliminated (resolves L-POST-2).
- The check is `O(n)` in `unique_resp_ids.len()`.  Lists are typically short
  (1–3 ECUs per diagnostic operation) so the overhead is negligible.
- Callers that previously set `unique_resp_ids` to non-empty values without a
  `UniqueRespIdTable` will now correctly receive no match (timeout), which is
  the intended behavior (they specified ECUs that cannot be identified without
  routing information).

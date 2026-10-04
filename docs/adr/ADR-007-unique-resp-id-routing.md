# ADR-007: UniqueRespIdTable-Based Frame Routing in poll_rx

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:** `j2534-0404-service/src/service/events.rs` (`poll_rx`)

## Context

Multiple CLLs may share a single J2534 physical channel (e.g., two UDS CLLs on
the same CAN bus targeting different ECUs).  ISO 22900-2 §9.3.2 specifies that
received frames must be routed to the correct CLL based on the
`UniqueRespIdTable` — a per-CLL mapping from `unique_resp_identifier` to ECU
addressing ComParams (`CP_CanRespUSDTId`, `CP_CanRespUUDTId`, etc.).

The original `poll_rx` delivered every received frame to **all** CLLs sharing
the channel.  In a multi-ECU scenario this caused each CLL to receive responses
intended for other ECUs, polluting the receive buffer and confusing clients that
compare frame data against an ECU-specific expected response.

## Decision

`poll_rx` now extracts the CAN ID from the first 4 bytes of each received frame
(CAN and ISO15765 frames embed the CAN ID as a big-endian u32 at the start of
the data field) and compares it against each CLL's `UniqueRespIdTable` entries:

1. If the CLL's `unique_resp_id_table` is **empty**, the frame is delivered
   unconditionally (legacy / no-table mode — all frames go everywhere).
2. If the table is **non-empty**, the CAN ID is matched against
   `CP_CanRespUSDTId` (physically addressed USDT) and `CP_CanRespUUDTId`
   (functionally addressed UUDT) of each entry.
   - **Match found** → frame is delivered to this CLL; `unique_resp_identifier`
     is set to the matching entry's value in the `ResultData` notification.
   - **No match** → frame is silently dropped for this CLL.
3. If the frame is shorter than 4 bytes (unusual), routing falls back to
   unconditional delivery (`unique_resp_identifier = 0`).

The `unique_resp_identifier` value is forwarded in `ResultData.unique_resp_identifier`
so clients can correlate received frames with table entries.

## Alternatives Considered

1. **Byte-level mask/pattern matching per UniqueRespIdTable entry** — ISO 22900-2
   allows arbitrary mask/pattern matching for non-CAN protocols.  Simplified to
   CAN ID (4-byte prefix) matching for now since only CAN/ISO15765 CLLs share
   channels in this implementation.  Full mask/pattern matching is a future
   enhancement.

2. **Fan-out to all CLLs; let the client filter** — Simpler server logic but
   violates the ISO 22900-2 routing contract.  Clients relying on
   `unique_resp_identifier` to identify ECUs would see spurious frames.

3. **Hardware CAN ID filters per CLL** — Would eliminate unwanted traffic at the
   adapter level but requires per-address filter installation and dynamic updates
   on every `SetUniqueRespIdTable` call (see ADR-005 §Alternatives).

## Consequences

- CLLs with a non-empty `UniqueRespIdTable` will only receive frames from ECUs
  whose CAN response address is in the table.  Broadcast and functional-address
  frames (e.g., OBD-II $7DF responses) will be dropped unless a matching entry
  with the broadcast ID is added.
- `unique_resp_identifier` is now populated in `ResultData` notifications,
  enabling clients to route responses without inspecting raw CAN IDs.
- Short frames (< 4 bytes) bypass CAN ID matching; all CLLs receive them.  This
  is intentional for K-line protocols (ISO9141 / ISO14230) where CAN IDs do not
  apply.

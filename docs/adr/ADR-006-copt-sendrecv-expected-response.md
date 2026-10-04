# ADR-006: CoptSendrecv Expected-Response Waiting

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:** `j2534-0404-service/src/service.rs` (`TxItem::SendRecv`, `ExpectedResponse`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`),
             `j2534-0404-service/src/service/events.rs` (`wait_for_expected_response`)

> **Amended by ADR-053 and ADR-058:** `Time` is no longer the response
> timeout (ADR-053 moved that to the Active `CP_P2Max`), and the "empty
> `expected_response_array` = fire-and-forget" rule described below in
> `Decision` is no longer how fire-and-forget is selected. ADR-058 makes
> `NumReceiveCycles == 0` the sole trigger for skipping the receive phase;
> an empty array with `NumReceiveCycles > 0` now times out instead of
> completing immediately. See ADR-053/ADR-058 for current behaviour.

## Context

ISO 22900-2 §9.4.1 defines `CoptSendrecv` as a "request/response" primitive: after
the request message is transmitted, the service must wait for a response matching
`ComPrimitiveCtrlData.expected_response_array` before emitting `PduCopstFinished`.

The original implementation emitted `PduCopstFinished` immediately after
`PassThruWriteMsgs`, ignoring `expected_response_array` entirely.  Clients that
rely on `PduCopstFinished` as the signal that the ECU has responded (e.g., to
know when to issue the next request) saw premature completion and issued
back-to-back requests before the ECU had time to reply.

## Decision

`TxItem::SendRecv` gains two new fields:

- `expected_response: Vec<ExpectedResponse>` — converted from proto
  `ComPrimitiveCtrlData.expected_response_array`.
- `response_timeout_ms: u32` — from `ComPrimitiveCtrlData.time` when non-zero,
  or 50 ms as a fallback (conservative P2Max default).

When `expected_response` is **non-empty** and the write succeeds, the poll task
calls `wait_for_expected_response` instead of emitting `PduCopstFinished`
immediately.  `wait_for_expected_response` polls RX in `POLL_INTERVAL_MS`-sized
chunks (shared with the CoptDelay pattern) until one of:

1. A frame matching any descriptor's `mask/pattern` arrives → emit `PduCopstFinished`.
2. `response_timeout_ms` elapses without a match → emit `PduErrEvtResponseError`
   then `PduCopstFinished`.
3. A hard channel error occurs → emit `PduCopstFinished` (the error already
   turned CLLs offline via `PduCllstOffline`).

All received frames, including the matching one, are fanned out to `rx_buf` and
`SubscribeEvent` streams through the same path as `poll_rx`.

When `expected_response` is **empty** (fire-and-forget), the original behaviour
is preserved: `PduCopstFinished` is emitted immediately after `PassThruWriteMsgs`.

## Matching Logic

A frame matches a descriptor when for every byte position `i` within the shorter
of `mask` and `pattern`:

```
(frame[i] & mask[i]) == pattern[i]
```

An empty `mask`/`pattern` matches any frame.

`unique_resp_ids` in each descriptor restricts which entries of the CLL's
`UniqueRespIdTable` may provide the matching frame.  This constraint is enforced
at the frame-delivery level (see ADR-007); `wait_for_expected_response` currently
ignores `unique_resp_ids` and matches on raw frame data only.

## Alternatives Considered

1. **Block `PassThruReadMsgs` with a non-zero timeout** — Simpler but holds the
   `api` lock for the full timeout duration, blocking all other API calls on the
   channel.

2. **Dedicated oneshot channel from `poll_rx` to the COP handler** — Cleaner
   separation, but adds cross-task synchronisation complexity.

3. **Immediate `PduCopstFinished` always (prior behaviour)** — Non-compliant for
   clients using `PduCopstFinished` as response-received signal.

## Consequences

- `CoptSendrecv` with non-empty `expected_response_array` now blocks the poll
  task's TX branch for up to `response_timeout_ms` before the next queued item
  is processed.  The RX path is not blocked because `wait_for_expected_response`
  polls in POLL_INTERVAL_MS chunks (same technique as CoptDelay).
- If the caller provides `expected_response_array` but not `time`, the service
  uses 50 ms as the default timeout.  Callers with long response times (ISO9141
  P2Max = 25 ms, extended P2Star = 5000 ms) must set `time` explicitly.
- `unique_resp_ids` filtering is not yet enforced in the response matcher
  (tracked as a follow-up to ADR-007).

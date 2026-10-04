# ADR-018: Automatic Handling of Response-Pending NRCs (0x78 / 0x21 / 0x23)

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:**
- `j2534-0404-service/src/service.rs` (`RcHandlingConfig` struct)
- `j2534-0404-service/src/service/events.rs` (`wait_for_expected_response`, `poll_rx_and_check_match`)

## Context

ISO 22900-2 D-PDU API defines a set of ComParams that allow the service layer to
automatically handle response-pending negative response codes (NRCs) on behalf of
the caller:

| NRC  | Meaning                          | Handling ComParam    |
|------|----------------------------------|----------------------|
| 0x78 | ResponsePending (UDS)            | `CP_RC78Handling`    |
| 0x21 | BusyRepeatRequest (KWP/older)    | `CP_RC21Handling`    |
| 0x23 | ConditionsNotCorrect (KWP/older) | `CP_RC23Handling`    |

Without this feature, the caller must implement its own retry/wait logic on top of
the raw `CoptSendrecv` result.  With handling enabled, the service absorbs the
pending response transparently.

## Decisions

### 1 — RC detection is independent of the expected_response pattern

The pending-NRC check runs on every frame routed to the target CLL, regardless of
whether the frame satisfies any `ExpectedResponse` descriptor.  When handling is
enabled for a given code, the service detects the NRC solely by reading
`data[CP_RCByteOffset]` and comparing it against 0x78 / 0x21 / 0x23.

Rationale: a caller's `ExpectedResponse` typically matches only the final positive
response.  Pending responses would not match the pattern, yet the service still
needs to detect them.  Conversely, if a caller's pattern happens to be broad enough
to match a pending NRC, the service must not treat that NRC as the final response
when handling is enabled.

### 2 — Pending frames are delivered to rx_buf and subscribers

Frames that trigger RC handling are still pushed to `rx_buf` and forwarded to
`SubscribeEvent` streams with `acceptance_id = 0`.  The caller remains informed
about what the ECU sent; the service only suppresses the premature
`PduCopstFinished` event.

### 3 — Match priority within one poll batch

If a single `PassThruReadMsgs` call returns both a pending-NRC frame and a final
matching response (e.g. after an ECU fast-path), the batch is reported as
`Matched` (the final response), not `PendingRc`.  This avoids an unnecessary
extra poll cycle.

### 4 — RC78 deadline extension (each 0x78 resets the clock)

When NRC 0x78 is received and `CP_RC78Handling = 1`, the COP deadline is reset to
`now + CP_RC78CompletionTimeout`.  Each subsequent 0x78 resets the deadline again.
If no final response arrives before the completion timeout expires, the COP ends
with `PduErrEvtRxTimeout` (same as a normal timeout).

Default for `CP_RC78CompletionTimeout`: **5000 ms** (absent an explicit `SetComParam`).

**Amended by ADR-056:** the actual deadline-extension value now comes from
`CP_P2Star` (the D-PDU-standard ComParam for exactly this purpose), not
`CP_RC78CompletionTimeout` — `CP_RC78CompletionTimeout` remains get/settable
but no longer has any effect. The 5000 ms default is unchanged.

**Amended by ADR-102:** `CP_P2Star` now reloads the deadline on every 0x78
occurrence (ISO 14229-2 §7.3), not once; `CP_RC78CompletionTimeout` is
reinstated as an independent total-duration ceiling — it is no longer
inert as the paragraph above (still accurate as of ADR-056) describes.

### 5 — RC21 / RC23 re-request sequence

When NRC 0x21 or 0x23 is received and the corresponding handling is enabled:

1. Sleep `CP_RC21RequestTime` / `CP_RC23RequestTime` ms (default 25 ms).
2. Re-send the original request data using the same `protocol_id` and `tx_flags`
   as the initial `PassThruWriteMsgs` call.
3. Reset the COP deadline to `now + CP_RC21CompletionTimeout` /
   `CP_RC23CompletionTimeout` (default 5000 ms each).

If the re-send fails (`PassThruWriteMsgs` error), the COP ends with
`PduErrEvtTxError` / `PduErrEvtFrameStruct` and `PduCopstFinished`.

### 6 — `CP_RCByteOffset` is relative to J2534 frame data

The byte offset is interpreted as an index into the raw data byte array returned
by `PassThruReadMsgs`, which for CAN / ISO15765 protocols includes the 4-byte CAN
ID prefix.  Example for UDS over ISO15765:

```
data[0..4]  = CAN response ID (big-endian)
data[4]     = 0x7F  (negative response service ID)
data[5]     = requested service ID
data[6]     = NRC   ← CP_RCByteOffset should be 6
```

**Superseded by ADR-051, corrected by ADR-057:** ADR-051 (after this ADR) made
`ResultData.data_bytes` payload-only for CAN/ISO15765/ISO9141/ISO14230/J1850,
splitting the header (the 4-byte CAN ID, for CAN/ISO15765) into
`extra_info.header_bytes` — and RC-byte-offset detection runs against that
same payload-only slice, not the pre-split raw frame this section describes.
For the UDS-over-ISO15765 example above, the NRC is therefore at
`payload[2]` (`0x7F`, SID, NRC — no CAN ID prefix), and `CP_RCByteOffset`
should be **2**, not 6.

Callers are responsible for setting `CP_RCByteOffset` to match their protocol and
addressing mode.  The default value is 0 when not configured, which is safe because
RC handling modes default to disabled (0).

### 7 — RC config is snapshotted at COP start

`RcHandlingConfig` is read from `LogicalLinkState.active` at the moment
`CoptSendrecv` begins (after the initial write, before the wait loop).  This
snapshot is immutable for the lifetime of the wait loop; a `CoptUpdateparam` that
arrives while waiting will not change the in-flight COP's RC behaviour.

## Consequences

- UDS workflows that receive 0x78 ResponsePending from an ECU now work correctly
  with a single `CoptSendrecv` when `CP_RC78Handling = 1` is set.
- KWP 0x21 / 0x23 retry behaviour is similarly automated when enabled.
- Callers that do not set any `CP_RC*Handling` param see no change in behaviour
  (all three default to 0 = disabled).
- `poll_rx_and_check_match` now carries `&RcHandlingConfig`; passing
  `&RcHandlingConfig::default()` (all disabled) restores previous behaviour for
  any future call sites.

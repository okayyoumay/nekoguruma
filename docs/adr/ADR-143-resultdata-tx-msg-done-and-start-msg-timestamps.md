# ADR-143: ResultData.tx_msg_done_timestamp/start_msg_timestamp Population

**Date:** 2026-07-28
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`cll_queue_item_to_event_item`)

## Context

ADR-098 forwards J2534's 5 low `RxStatus` bits into ISO 22900-2's
`ResultData.rx_flag`, but left `PDU_RESULT_DATA`'s two dedicated,
flag-gated-valid timestamp fields — `TxMsgDoneTimestamp`/`StartMsgTimestamp`
(`ResultData.tx_msg_done_timestamp`/`start_msg_timestamp` in the gRPC proto,
both `optional uint32`, microsecond units) — unpopulated as an accepted
residual, tracked as a P2 backlog item in
`j2534-0404-service/docs/implementation-notes.md`. The proto already models
both as `optional uint32` (presence = valid), so no schema change is needed
here; only the mapping logic at the one construction site was missing.

This mirrors a convention `iso22900-service`'s adapter already implements
against the native D-PDU library's own equivalent fields — flag-gated
validity, one bit per field
(`iso22900-service/src/service/events.rs:77-99` reads a `TimestampFlags`
byte and only surfaces each timestamp when its own bit is set). This ADR
applies the same shape to `j2534-0404-service`, but from a different data
source (J2534 `RxStatus`, not a native `TimestampFlags` field), so the two
adapters compute "is this timestamp valid" independently even though the
resulting proto shape looks identical.

SAE J2534-1 v04.04 §8.2 draws a distinction this decision hinges on: a
TxDone/transmit-completion indication is timestamped at the end of
transmission, while a start-of-message indication is timestamped at the
start of the first bit/frame — two different instants in time, both
reported through the same single `PASSTHRU_MSG.Timestamp` field, with which
one applies determined entirely by which `RxStatus` indication bit
accompanies the frame. That makes this a protocol-interpretation decision,
not a mechanical field copy: naively populating `tx_msg_done_timestamp`
whenever *any* transmit-related bit is present risks mislabeling a
start-time as a done-time. §8.7.1 defines the `RxStatus` bit table this
decision reads (`TX_INDICATION` = bit 3, `START_OF_MESSAGE` = bit 1); the
§A.2.1/§A.2.2 conversation examples illustrate both indication kinds
appearing on distinct frames of an exchange.

## Decision

At `cll_queue_item_to_event_item`'s sole `ResultData` construction site,
populate the two fields as two independent bit checks against
`frame.rx_status_flags`, with no combination validation — matching
ADR-098's existing trust-the-hardware, bit-copy-unconditionally idiom for
`rx_flag`:

```rust
tx_msg_done_timestamp: (frame.rx_status_flags & RX_TX_INDICATION as u8 != 0)
    .then_some(frame.timestamp),
start_msg_timestamp: (frame.rx_status_flags & RX_START_OF_MESSAGE as u8 != 0)
    .then_some(frame.timestamp),
```

`tx_msg_done_timestamp` keys off `TX_INDICATION` alone; `start_msg_timestamp`
keys off `START_OF_MESSAGE` alone. Neither check depends on the other, and
neither depends on any of the other `RxStatus` bits (`TX_MSG_TYPE`,
`RX_BREAK`, `ISO15765_PADDING_ERROR`) — a frame can set neither, either, or
(per ADR-097's documented `TX_MSG_TYPE|START_OF_MESSAGE`, `0x03`, combo)
both alongside other bits, and each field only ever looks at its own bit.

A deliberate exclusion follows from this: a bare `TX_MSG_TYPE`-only
(loopback) echo, with no `TX_INDICATION` bit, does **not** populate
`tx_msg_done_timestamp`, even though such an echo is itself the record of
a transmission. This is a considered abstention, not an oversight.
ADR-097's own `TX_MSG_TYPE|START_OF_MESSAGE` combo is the decisive
counter-example: that frame's timestamp is a start-of-first-bit time per
§8.2, not a done time, so keying `tx_msg_done_timestamp` off `TX_MSG_TYPE`
would mislabel it for that combo. Safely distinguishing "loopback alone" from
"loopback plus SOM" for this purpose would require the same kind of
multi-bit combination-exclusion logic ADR-098 already declined to add for
`rx_flag`/matching eligibility — and a bare loopback echo's timestamp is not
actually needed for that: the client can already derive it from
`EventItem.timestamp` (already delivered on every event) combined with the
existing `TX_MSG_TYPE` bit already present in `rx_flag`. So it is left
`None`, deliberately.

The synthetic fast-init response frame (`j2534-0404-service`'s own
constructed reply to an `IOCTL_FAST_INIT` call, not a real bus frame) always
has `rx_status_flags: 0` — no real `RxStatus` was ever captured for it.
Both fields therefore come out `None`/`None` naturally under this logic,
with no special-casing needed: correct per the flag-gated-validity
semantics, since no captured indication timing exists for a synthesized
frame.

## Consequences

- Closes the ADR-098-tracked P2 backlog item in
  `j2534-0404-service/docs/implementation-notes.md`, and amends ADR-098's own
  Context/Consequences text to note the residual as resolved here.
- Accepted residual: a bare loopback (`TX_MSG_TYPE`-only) echo never
  populates `tx_msg_done_timestamp` — derivable instead from
  `EventItem.timestamp` plus the `TX_MSG_TYPE` `rx_flag` bit, per the
  Decision above.
- Accepted residual: the synthetic fast-init response frame always reports
  `None`/`None` for both fields (no real `RxStatus` indication exists for
  it).
- Accepted residual, unwidened from ADR-098: software-ISO-TP reassembled
  frames and raw-passthrough frames never carry `START_OF_MESSAGE`/
  `TX_INDICATION` (this driver does not synthesize its own indications), so
  they stay `None`/`None` too — consistent with, not an extension of,
  ADR-098's existing no-synthesized-indications residual.
- Tests added/extended in `j2534-0404-service/tests/grpc_mock/`:
  - `response_distribution.rs`:
    `iso15765_start_of_message_frame_does_not_complete_a_pending_sendrecv`
    (extended: asserts `start_msg_timestamp`/`tx_msg_done_timestamp` on the
    SOM frame),
    `iso15765_tx_done_frame_does_not_complete_a_pending_sendrecv` (extended:
    asserts `tx_msg_done_timestamp`/`start_msg_timestamp` on the TxDone
    frame),
    `iso15765_loopback_echo_of_own_request_does_not_register_phantom_pending_rc`
    (extended: asserts both fields stay `None` on a bare loopback echo, with
    the abstention rationale in a comment),
    `iso15765_loopback_and_som_combo_populates_start_msg_timestamp_only`
    (new: the ADR-097 `0x03` combo),
    `iso15765_tx_indication_combined_with_padding_error_populates_tx_msg_done_timestamp`
    (new: `TX_INDICATION|TX_MSG_TYPE|ISO15765_PADDING_ERROR`, `0x19`).
  - `startcomm_comparam.rs`: `iso9141_explicit_fast_init_setting_succeeds`
    (extended: asserts both fields are `None` on the synthetic fast-init
    response).

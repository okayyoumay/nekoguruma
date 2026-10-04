# ADR-061: J2534 RxStatus Is Not Forwarded as ISO 22900 ResultData.rx_flag

**Date:** 2026-07-05
**Status:** Superseded by ADR-097
**Affects:** `j2534-0404-service/src/service.rs` (`ReceivedFrame`),
             `j2534-0404-service/src/service/events.rs` (`poll_rx_inner`,
             `handle_start_comm`), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_get_event_item`)

## Context

`j2534-0404-service` populated `vci_service_interface::ResultData.rx_flag`
(`bytes`, proto `service.proto` — "Raw RxFlag bytes as returned by VCI") with
the raw J2534 `PASSTHRU_MSG.RxStatus` `u32`, little-endian-encoded, at every
site that constructs a `ResultData`: the `poll_rx_inner` fan-out to
`SubscribeEvent` subscribers, the synthetic fast-init response frame in
`handle_start_comm`, and the buffered-frame path in `GetEventItem`
(`rpc_get_event_item`).

This conflated two unrelated bitfields. SAE J2534-2's `RxStatus` is a
specific, standardized 32-bit layout:

| Bit(s) | Meaning |
|---|---|
| 8 | `CAN_29BIT_ID` — 0 = 11-bit, 1 = 29-bit CAN identifier |
| 7 | `ISO15765_ADDR_TYPE` — 0 = no extended address, 1 = extended address is the first payload byte |
| 4 | `ISO15765_PADDING_ERROR` — a CAN frame with fewer than 8 data bytes was received under ISO15765 |
| 3 | `TX_INDICATION` — ISO 15765 TxDone indication |
| 2 | `RX_BREAK` — break received (SCI/J1850 VPW only) |
| 1 | `START_OF_MESSAGE` — first byte/frame of a message |
| 0 | `TX_MSG_TYPE` — 0 = received, 1 = transmit-loopback echo |

ISO 22900-2's D-PDU API, by contrast, represents `PDU_RESULT_DATA.RxFlag` as
a `PDU_FLAG_DATA` (length + byte pointer) with **no bit-position standard at
all** — the D-PDU spec leaves its content entirely VCI/protocol-defined.
Nothing in `iso22900-sys`'s headers defines what any bit of a genuine D-PDU
RxFlag means; it is whatever the underlying vendor D-PDU library chooses to
report. `j2534-0404-service` has no such vendor D-PDU library behind it — it
adapts J2534 hardware — so there was no real "VCI RxFlag" to report in the
first place. Forwarding the raw `RxStatus` bits verbatim presented J2534-2's
bit layout to gRPC clients as if it were meaningful D-PDU RxFlag content,
which it is not: a client bit-testing the received bytes against any D-PDU
RxFlag convention (its own vendor's, or none at all, since none is
standardized) would be reading J2534-specific bits under a false pretense.

A related question this ADR also resolves: does any of this service's own
CAN-ID interpretation logic (header/footer splitting, `UniqueRespIdTable`
routing, addressing resolution) depend on reading bits out of the raw
`RxStatus` value — in particular `CAN_29BIT_ID`, since an 11-bit and a
29-bit CAN ID can share the same numeric value in the low range and J2534
encodes both as a 4-byte field regardless of width? Audited every read of
`RxStatus`/`rx_status()` in the crate: none exists outside the three
verbatim-copy sites above. All CAN-ID and addressing decisions in this
service (`header_footer_len`'s fixed 4-byte CAN/ISO15765 header assumption,
`route_frame`/`route_frame_uudt_only`'s `UniqueRespIdTable` matching,
extended-addressing resolution) are driven by ComParams
(`CP_CanRespUSDTFormat`/`CP_CanPhysReqFormat`/etc.) and the J2534 hardware
filter the adapter was configured with at connect time (ADR-039/040/041),
never by `RxStatus`. The hardware `FLOW_CONTROL_FILTER`/hardware ID+mask
already constrains which frames reach this service to the exact
ID+format combination configured, so there is no live ambiguity here to
correct.

## Decision

`ResultData.rx_flag` is now always empty (`Vec::new()`) at all three
construction sites in `j2534-0404-service`, instead of
`rx_status.to_le_bytes().to_vec()`. `ReceivedFrame.rx_status` (which existed
solely to carry the value to those three sites) is removed, along with the
now-unused `msg.rx_status()` read in `poll_rx_inner`.

No replacement encoding is introduced. ISO 22900-2 does not standardize
RxFlag bit positions, so there is no "correct" D-PDU-native value this
adapter could compute from `RxStatus` even if it wanted to invent one; doing
so unilaterally would fabricate a convention no client has any standard
reason to expect, for no more correctness than reporting nothing. A client
that needs any of the seven facts `RxStatus` exposes (29-bit vs. 11-bit,
extended addressing, padding error, TX-done/loopback, break, start-of-message)
must be served by dedicated fields if this service ever needs to expose
them — not by smuggling J2534's own bit layout through a field with
unrelated semantics.

## Consequences

- `ResultData.rx_flag` is empty for every response this service returns; it
  was never meaningful to a D-PDU-aware client in the first place, so no
  existing correct client behavior depended on its previous (incorrect)
  content. No test asserted on `rx_flag`'s value.
- The seven facts `RxStatus` exposes (see the Context table) are not
  currently surfaced to gRPC clients through any field. In particular,
  `TX_MSG_TYPE` (transmit-loopback echo detection) is not used by this
  service to filter loopback frames out of `SubscribeEvent`/`GetEventItem`
  delivery — a pre-existing gap, unrelated to and not introduced by this
  ADR, noted here in case a future reader wonders whether `rx_flag` was the
  intended mechanism for it (it was not, and being D-PDU-facing it
  wouldn't have been the right one regardless).
- Confirmed (not changed): no CAN-ID or addressing interpretation logic in
  this service reads `RxStatus` bits; all such logic is ComParam- and
  hardware-filter-driven.

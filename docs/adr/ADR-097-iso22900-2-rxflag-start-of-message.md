# ADR-097: ISO 22900-2 RxFlag START_OF_MESSAGE Forwarding

**Date:** 2026-07-17
**Status:** Superseded by ADR-098
**Affects:** `j2534-0404-service/src/service.rs` (`ReceivedFrame`, `ExpectedResponse::matches`),
             `j2534-0404-service/src/service/events.rs` (`poll_rx_inner`,
             `handle_start_comm`, `rx_flag_bytes`), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_get_event_item`)

## Context

ADR-061 made `ResultData.rx_flag` always empty, on the premise that ISO
22900-2's `PDU_RESULT_DATA.RxFlag` has "no bit-position standard at all" and
so there was no D-PDU-native value this adapter could compute from J2534's
`RxStatus`. That premise is wrong: ISO 22900-2 defines a 4-byte `RxFlag`
layout, bit-compatible with SAE J2534-2's `RxStatus` under big-endian
serialization. Concretely, `RxStatus`'s bits 0-8 map onto `RxFlag`'s bytes 2
(bit 0 only) and 3 (bits 0-7):

| RxFlag byte | RxFlag bit | RxStatus bit | Meaning |
|---|---|---|---|
| 2 | 0 | 8 | `CAN_29BIT_ID` — 0 = 11-bit, 1 = 29-bit CAN identifier |
| 3 | 7 | 7 | `ISO15765_ADDR_TYPE` — 0 = no extended address, 1 = extended address is the first payload byte |
| 3 | 4 | 4 | `ISO15765_PADDING_ERROR` — a CAN frame with fewer than 8 data bytes was received under ISO15765 |
| 3 | 3 | 3 | `TX_INDICATION` — ISO 15765 TxDone indication |
| 3 | 2 | 2 | `RX_BREAK` — break received (SCI/J1850 VPW only) |
| 3 | 1 | 1 | `START_OF_MESSAGE` — first byte/frame of a message |
| 3 | 0 | 0 | `TX_MSG_TYPE` — 0 = received, 1 = transmit-loopback echo |

This is exactly ADR-061's original seven-row `RxStatus` table, re-expressed
with each bit's `RxFlag` byte/bit position under the alignment above.
`RxStatus` bits 5 and 6 have no defined meaning in SAE J2534-2 (ADR-061 did
not list them either), so `RxFlag` byte 3 bits 5 and 6 are left out of this
table rather than assigned a guessed meaning.

ISO 22900-2 additionally names `REMOTE_FRAME`, `SPD_CHG_EVENT`,
`ECU_TIMING_CHANGE`, `CAN_SEGMENTATION`, and `SW_CAN_HV_RX` as `RxFlag`
concepts with no `RxStatus` counterpart identified above — this ADR does not
claim or fabricate byte/bit positions for them (consistent with ADR-061's
"no fabricated bits" principle); they are out of scope, unimplemented, and
left reserved/zero (see Decision and Consequences).

J2534 SOM (`START_OF_MESSAGE`) indications carry a `Data` payload that is
either header-only (ISO15765: the 4-byte CAN ID, or 5 bytes with extended
addressing — no payload bytes) or completely empty (ISO9141/ISO14230). This
service's existing `route_frame`/`header_footer_len` (ADR-051) already
handle both shapes correctly: `header_footer_len` treats the whole frame as
header for a bare-CAN-ID ISO15765 frame, and returns `(0, 0)` for an empty
ISO9141/ISO14230 frame (`kwp_header_and_payload_len` returns `None` on empty
input) — no routing change is needed for this ADR.

A real bug is surfaced by this fact, though: `ExpectedResponse::matches`
(`j2534-0404-service/src/service.rs`) is vacuously `true` whenever `cmp_len`
(`mask.len().min(pattern.len()).min(data.len())`) is `0` — in particular
whenever `data` (here, the post-header-split payload) is empty, regardless
of `mask`/`pattern`. Since a SOM frame's payload is always empty after the
header/footer split, an in-flight `CoptSendrecv`/pending-RC wait (`poll_rx_inner`'s
`MatchProbe` arm) would otherwise treat the SOM frame itself as the awaited
response (or a pending-RC interim frame), falsely completing/advancing the
wait before the real reassembled response arrives.

## Decision

`ResultData.rx_flag` is a 4-byte ISO 22900-2 `RxFlag` buffer with only byte 3
bit 1 (`START_OF_MESSAGE`) populated. `events::rx_flag_bytes(start_of_message:
bool)` emits `[0x00, 0x00, 0x00, 0x02]` when the source J2534 `RxStatus &
0x00000002 != 0`, and `Vec::new()` (no flags asserted) otherwise — preserving
ADR-061's "empty means nothing asserted" convention for every frame that is
not a SOM indication. `ReceivedFrame` gains a `start_of_message: bool` field
(ADR-009 pattern) computed once per polled message (`msg.rx_status() &
RX_START_OF_MESSAGE != 0`) and carried to both `ResultData` construction
sites: the `SubscribeEvent` fan-out in `poll_rx_inner`, and the buffered
`GetEventItem` path in `rpc_get_event_item`, which reads it off the buffered
`ReceivedFrame`. The synthetic fast-init response frame in `handle_start_comm`
has no real `RxStatus` and is therefore never a SOM indication (`start_of_message:
false`, `rx_flag: Vec::new()`, unchanged from ADR-061).

SOM frames are excluded from `ExpectedResponse` matching and pending-RC
detection: `poll_rx_inner`'s `MatchProbe` guard gains `&& !start_of_message`,
gating both the pending-RC-detection sub-branch and the mask/pattern-matching
sub-branch, so a SOM frame's `frame_cop_handle` is always `None` — it falls
through to delivery as an ordinary unsolicited indication (like any frame
outside a request/response cycle), never attributed to the waiting COP. This
is a narrower, more targeted fix than changing `ExpectedResponse::matches`
itself: SOM frames are indications, not responses, so excluding them by kind
is correct regardless of what any particular descriptor's mask/pattern would
have matched (a non-empty mask/pattern could vacuously match an empty payload
too, for other `cmp_len == 0` reasons — this ADR does not attempt to close
that general case, only the SOM-specific one it introduces new certainty
about).

Routing (`route_frame`, `route_frame_uudt_only`, `header_footer_len`) is
unchanged — already correct for both the header-only (ISO15765) and
completely-empty (ISO9141/ISO14230) SOM `Data` shapes.

ADR-061's "no fabricated bits" principle is preserved for the other 10
`RxFlag` bits ISO 22900-2 defines: the six `RxStatus`-aligned ones this ADR
does not populate (`CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`,
`ISO15765_PADDING_ERROR`, `TX_INDICATION`, `RX_BREAK`, `TX_MSG_TYPE`) and the
five with no known `RxStatus` counterpart (`REMOTE_FRAME`, `SPD_CHG_EVENT`,
`ECU_TIMING_CHANGE`, `CAN_SEGMENTATION`, `SW_CAN_HV_RX`) all remain
unreported/zero. Implementing any of them is explicitly future scope, not
part of this ADR.

## Consequences

- `CAN_29BIT_ID` (`RxFlag` byte 2 bit 0) and `ISO15765_ADDR_TYPE` (`RxFlag`
  byte 3 bit 7) read `0` on a SOM indication even when true on the wire —
  accepted residual, not implemented by this ADR. A client already has both
  facts through other channels for a SOM frame specifically: the CAN ID's
  own width is visible in `extra_info.header_bytes`, and addressing is
  ComParam-configured (`CP_CanRespUSDTFormat`/`CP_CanRespUUDTFormat`) rather
  than frame-derived. Populating these bits is mechanical follow-up if a
  future reader ever needs them for non-SOM frames too, in which case this
  ADR's byte-3 encoding is directly extensible.
- CONFIG_LOOPBACK echoes of a SOM frame (`RxStatus` bit 0 `TX_MSG_TYPE` and
  bit 1 `START_OF_MESSAGE` both set) are still delivered with `RxFlag` byte 3
  bit 0 (`TX_MSG_TYPE`) reading `0` — this is ADR-061's pre-existing
  loopback-echo gap (that ADR's Consequences section already noted
  `TX_MSG_TYPE` is not surfaced anywhere), unrelated to and not widened by
  this change.
- Software-ISO-TP reassembly (`RxEntryKind::SoftwareIsoTp`,
  `process_frame_for_entry`) does not synthesize its own SOM indications —
  reassembled/raw-passthrough frames it emits never carry `start_of_message
  = true` even when the underlying raw CAN frames that fed reassembly did.
  Latent gap, out of scope here (would require synthesizing an indication at
  reassembly start, which is a routing/behavior change, not just an
  encoding one).
- No test asserted on `rx_flag`'s value before this ADR (`assert_result_data`
  only checked it was empty); tests now cover both the empty default and the
  `[0, 0, 0, 2]` SOM encoding, plus the `ExpectedResponse`/pending-RC
  exclusion.
- The START_OF_MESSAGE behavior for ISO15765 in this implementation was
  specified directly by the requester per SAE J2534-2/ISO 22900-2 (not
  independently re-derived from a primary spec text in this repo), and is now
  covered by both native-ISO15765 and software-ISO-TP test paths
  (`j2534-0404-service/tests/grpc_mock/rx_header_split.rs`).

## Supersession

Supersedes [ADR-061](ADR-061-rxstatus-rxflag-no-direct-passthrough.md) in
full: ADR-061's premise (no ISO 22900-2 `RxFlag` bit-position standard) was
incorrect, and this ADR replaces its "always empty" decision with the
conditional `START_OF_MESSAGE`-only encoding above. ADR-061's other
findings — that no CAN-ID/addressing interpretation logic in this service
reads `RxStatus` bits, and that `TX_MSG_TYPE` is not otherwise surfaced —
still hold and are not revisited here.

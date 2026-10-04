# ADR-051: Split Protocol Header/Footer Bytes into ResultData.extra_info, not data_bytes

**Date:** 2026-07-03
**Status:** Accepted (RX-direction K-line header/footer parsing extended for CARB address mode
             by ADR-167; the J1850 Decision row's fixed-1-byte CRC footer and its supporting
             "no self-describing length" rationale superseded by ADR-171 — the footer is the
             native `ExtraDataIndex`-reported trailing IFR byte count, not a fabricated CRC byte;
             scope narrowed to RawMode=OFF CLLs by ADR-196 (RawMode=ON delivers the whole
             frame in `data_bytes` with no split); this Decision's ISO15765 extended-addressing
             detection amended by ADR-197 — `can_addressing_by_id`'s `UniqueRespIdTable` lookup is
             no longer the sole signal, it is OR'd with the frame's own RxStatus bit 7; that same
             `UniqueRespIdTable` lookup split by role into `usdt_addressing_by_id`/
             `uudt_addressing_by_id` by ADR-217 (Codex-review fix, PR #132) — a single flat table
             let one role's `Addressing` bleed into a delivery routed via the other role under a
             colliding USDT/UUDT configuration; all other decisions below remain in force)
**Affects:** `j2534-0404-service/src/service.rs` (`ReceivedFrame`),
             `j2534-0404-service/src/service/events.rs`
             (`CllRxEntry`, `build_cll_rx_entries`, `process_frame_for_entry`,
             `poll_rx_inner`, `header_footer_len`, `kwp_header_and_payload_len`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_get_event_item`),
             `j2534-0404-service/tests/grpc_mock.rs`

## Context

Before this change, `poll_rx_inner` delivered every protocol's frames to
both `GetEventItem` and `SubscribeEvent` exactly as
`process_frame_for_entry` produced them — for CAN-family protocols the same
`[4-byte CAN ID][payload]` layout documented in ADR-046's "Consequences" as
keeping RX handling "mode-agnostic" across Hardware/Companion/SoftwareIsoTp
routing. `ResultData.data_bytes` was always the full frame, header
included, and `extra_info` was always `None`.

This was asymmetric with the TX side: ADR-050 already made `CoptSendrecv`'s
`cop_data` payload-only, with the service constructing the ID/header prefix
(CAN ID, KWP format/target/source/length, J1850 format/target/source) from
ComParams/`UniqueRespIdTable` before it reaches `PassThruWriteMsgs`.
`ExpectedResponseData.mask_data`/`pattern_data` also had to account for
that same header's length to match against the diagnostic payload at all,
which is error-prone and inconsistent with the now payload-only TX
contract.

## Decision

`poll_rx_inner` now splits each delivered frame into header, payload, and
footer, per this table, applied uniformly to every protocol this service
supports:

| Protocol | Header | Footer |
|---|---|---|
| CAN | 4 bytes of CAN ID | — |
| ISO15765 | 4 bytes of CAN ID | — |
| ISO15765 (extended addressing) | 4 bytes of CAN ID, 1 Address Extension byte | — |
| ISO9141 / ISO14230 | 1-4 bytes, parsed from the KWP2000 format byte | whatever trails the header's own declared payload length (e.g. a 1-byte checksum) |
| J1850 (PWM/VPW) | 3 bytes (format/priority + target + source) | 1 byte (CRC), whenever a trailing byte remains |
| SCI | — | — |

- `ResultData.data_bytes` (and the `GetEventItem`-polled equivalent) is the
  payload only.
- `ResultData.extra_info` carries the header/footer as
  `ExtraInfo.header_bytes`/`ExtraInfo.footer_bytes`. `extra_info` is `None`
  when both are empty (SCI, or any protocol outside this table).
- `ExpectedResponseData.mask_data`/`pattern_data` (and `CP_RCByteOffset`
  pending-response-code detection) now match against this same
  payload-only slice, not the header or footer.

**ISO9141/ISO14230's header length is parsed per-frame, not assumed
fixed**, and its declared payload length is what makes the footer
self-describing. This service always constructs the 4-byte "physical
addressing, length in a separate byte" variant on TX
(`tx_header::kwp_header_bytes`, ADR-050, format `0x80`), but nothing
requires an ECU's *response* to use that same encoding or to carry a
checksum at all, so `kwp_header_and_payload_len` decodes the actual format
byte per the ISO14230-1 standard: bit 7 set means a 2-byte target+source
address follows the format byte; the low 6 bits (the embedded `LEN` field)
give the payload length directly when nonzero, or — when zero — indicate a
separate length byte immediately follows. Header length is `1 (format
byte) + (2 if addressed) + (1 if a separate length byte follows)` — 1, 2,
3, or 4 bytes. Whatever bytes remain in the frame after `header + declared
payload length` become the footer — 0 bytes when no checksum is present
(e.g. on a checksum-managed J2534 connection, where the vendor DLL already
strips it before this service ever sees the frame), 1 when it is.
ISO9141 shares this exact handling, since it shares the same header
construction as ISO14230 on TX in this codebase.

**J1850's footer has no self-describing length field to lean on**, unlike
KWP — J1850's header carries no explicit length byte at all. Its footer is
therefore a *fixed* 1 byte (the standard's CRC) whenever a byte remains
after the 3-byte header, mirroring how this codebase already treats
J1850's header as unconditionally fixed at 3 bytes on TX
(`tx_header::j1850_header_bytes`) rather than hedging on connect-flag
state the way K-line's checksum is hedged on.

**ISO15765's extended-addressing widening only applies to a genuinely raw
frame.** `process_frame_for_entry`'s return type carries a third element,
`raw: bool`: `true` for Hardware/Companion delivery and software-ISO-TP's
raw non-ISO-TP passthrough (an Address Extension byte, if extended
addressing is configured for that CAN ID, is still embedded at `data[4]`);
`false` for software-ISO-TP's reassembled Single/First+Consecutive-complete
deliveries, where any AE byte was already consumed during reassembly and
must never be re-counted as header (re-slicing it off would silently steal
the payload's first real byte). `raw = false` always yields the plain
4-byte CAN-ID-only header. `CllRxEntry` carries a `can_addressing_by_id:
Vec<(u32, Addressing)>` field (empty except for ISO15765 CLLs), built from
each `UniqueRespIdTable` entry's `CP_CanRespUSDTId`/`CP_CanRespUSDTFormat`/
`CP_CanRespUSDTExtAddr` (and the `UUDT` equivalents), so `header_footer_len`
can look up whether the CAN ID a raw delivery actually carries uses
extended addressing.

**SCI is included in the same mechanism, but is always a no-op** — it has
no header/footer concept in this service (mirroring
`tx_header::build_tx_message`'s "SCI: unchanged" TX-side stance): `header`
and `footer` are always empty, `data_bytes` carries the frame exactly as
received, `extra_info` is absent.

**The J2534 PassThruMessage.Data actually written to and read from the
J2534 library is unaffected** — it still carries the full header/footer on
the wire (CAN ID, KWP format/address/length bytes, checksum/CRC, etc.),
exactly as SAE J2534-1 requires. Only the gRPC-facing `ResultData`
representation changes; nothing about `process_frame_for_entry`'s frame
*contents* or the underlying wire format is touched. This mirrors ADR-050,
which is TX-only and payload-only at the `StartComPrimitive` RPC boundary
while `PassThruWriteMsgs` still sees the full constructed message.

## Consequences

- `CllRxEntry` carries `header_protocol: u32` (from `LogicalLinkState::
  protocol.j2534_protocol_id()`, not `hw_protocol_id` — the two differ in
  software-ISO-TP mode, ADR-046, and the split must apply to a
  software-ISO-TP ISO15765 CLL the same as a hardware one) and
  `can_addressing_by_id`. `ReceivedFrame` carries `header_bytes: Vec<u8>`
  and `footer_bytes: Vec<u8>` fields alongside its existing (now
  payload-only, where the split applies) `data`. Both `poll_rx_inner`'s
  `SubscribeEvent` push path and `rpc_get_event_item`'s `GetEventItem`
  pull path read `ReceivedFrame` the same way, so both surfaces stay
  consistent without duplicating the split logic.
- The header/payload/footer split happens once, in `poll_rx_inner`, after
  `process_frame_for_entry` has already normalized Hardware/Companion/
  SoftwareIsoTp delivery — so the split itself stays mode-agnostic, same as
  the framing it operates on. A dual-channel-mode UUDT frame delivered via
  the companion raw-CAN channel is still split, since it belongs to an
  ISO15765-protocol CLL.
- `grpc_mock.rs`: the full-sequence test
  (`iso15765_standard_grpc_call_sequence_round_trips_through_mock`) asserts
  the ISO15765 split directly. Further tests cover the table's other rows:
  `can_protocol_splits_can_id_header_into_extra_info`,
  `iso15765_hardware_extended_addressing_widens_rx_header_to_five_bytes`,
  `iso14230_protocol_parses_variable_length_kwp_header_on_rx` (all four KWP
  header lengths — 1, 2, 3, 4 bytes — plus a trailing-checksum case),
  `iso9141_protocol_splits_kwp_header_and_checksum_footer_on_rx`,
  `j1850vpw_protocol_splits_header_and_crc_footer_on_rx`, and
  `sci_protocol_is_unaffected_by_the_header_footer_split`. Four
  pre-existing RX tests that read `ResultData` via `wait_for_result_data`
  (software-ISO-TP reassembly, dual-channel/`auto`-mode UUDT delivery,
  extended-addressing reassembly) were updated: the helper now returns the
  full `ResultData` instead of a bare `(data_bytes, unique_resp_identifier)`
  tuple, and a new `assert_result_data(result, header, footer, payload)`
  helper checks all three together.
- A deployment reading `ResultData.data_bytes` and expecting a protocol
  header (or, for K-line, a trailing checksum) as part of it — or building
  `ExpectedResponseData` mask/pattern bytes that skip over either — must
  migrate to reading `extra_info.header_bytes`/`footer_bytes` and
  mask/pattern-matching the payload directly, for every protocol this
  service supports except SCI. This is an intentional, documented breaking
  change to the client-facing message contract for `ResultData`, symmetric
  with ADR-050's `cop_data` change.

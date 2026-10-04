# ADR-199: SAE J2534-2 Clause 14 Repeat Messaging RawMode Support

**Date:** 2026-08-29
**Status:** Accepted (Decision item 3's third reuse of the shared `raw_mode_addressing_flags` fold — the mask/pattern response template's own addressing basis — superseded by [ADR-214](ADR-214-repeat-messaging-rawmode-response-addressing.md); this ADR's Consequences accepted-residual bullet is closed by that ADR)
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_start_repeat_message`),
             `j2534-0404-service/src/service/rpc_primitive.rs`
             (`raw_mode_tx_flag_bit_to_j2534` — `pub(super)`, doc comment correction),
             `j2534-0404-service/tests/grpc_mock/repeat_message.rs`,
             `docs/rpc-api-guide.md`,
             `j2534-0404-service/docs/implementation-notes.md`,
             `docs/adr/INDEX.md`,
             `docs/adr/ADR-165-j2534-2-repeat-messaging-phase12.md` (Status line annotation),
             `docs/adr/ADR-198-cll-create-flag-raw-mode-checksum-mode-phase2.md` (Status line
             annotation)

## Context

ADR-196 (Phase 1) and ADR-198 (Phase 2) added `CllCreateFlag` RawMode/
ChecksumMode support (ISO 22900-2:2022 Annex D.2.3, Table D.6) for base CAN,
hardware ISO15765, and hardware K-line (ISO9141/ISO14230). Both phases left
`PDU_IOCTL_START_REPEAT_MESSAGE` (SAE J2534-2 clause 14,
`rpc_misc.rs::ioctl_start_repeat_message`) unconditionally rejecting
`PDU_ERR_ID_NOT_SUPPORTED` on any RawMode=ON CLL for these four protocols —
this function's own message composition never honored RawMode at all: it
always built a service-derived header (`tx_header::build_tx_message` called
with `raw_mode` hardcoded `false`) and a service-derived response mask/
pattern prefix (`tx_header::response_header_bytes`, unconditional), which
would double up with a RawMode client's own already-complete raw bytes and
transmit/compare a malformed frame.

A `design-advisor` consult (prior to this implementation) resolved the
composition question this residual left open.

ISO 22900-2:2022 §10.1.4.19.5/Table 80 documents RawMode's expected-response
comparison template as header-inclusive — the client's own comparison
pattern carries the CAN-ID/header bytes itself, the same convention Table
D.4 already establishes for a RawMode client's transmitted message (ADR-196
Decision item 3's ordinary expected-response matching already applies this).
Separately, ISO 22900-2:2022 Annex B documents the header-construction
ComParams (`CP_HeaderFormatJ1850`, `CP_HeaderFormatKW`, and the
CAN-ID/UniqueRespIdTable-driven ComParams `tx_header::response_header_bytes`
consults) as inert under RawMode — corroborating that this function's
ComParam-derived response-header construction should not run at all for a
RawMode CLL.

## Decision

**RawMode extends to ALL THREE `RepeatMsgData` spans** — `repeat_msg_data`,
`mask_data`, and `pattern_data` — not just the transmitted message, for
every protocol this IOCTL already reaches under RawMode (CAN, ISO15765,
ISO9141, ISO14230; `resources::base_protocol_id`-normalized). The
unconditional rejection is removed entirely; nothing new needs rejecting.
Every other existing rejection in this function (software-ISO-TP, UART Echo
Byte, Analog Inputs, Ethernet_NDIS, TP2.0-broadcast, J1939 claim gates) is
unaffected — orthogonal to RawMode.

**1. `repeat_msg_data` composition** passes the calling CLL's real
`raw_mode` value to `tx_header::build_tx_message` instead of a hardcoded
`false` — that function's existing unconditional RawMode early return
(`if raw_mode { return Ok(payload.to_vec()); }`, already protocol-agnostic
per ADR-198 Decision item 5) needed no change of its own.

**2. `mask_data`/`pattern_data` composition** skips
`tx_header::response_header_bytes` entirely when `raw_mode` is `true`,
using `setup.mask_data`/`setup.pattern_data` directly, unprefixed — the
client's own bytes are the complete template already. `(Vec::new(),
Vec::new(), raw_mode_addressing_flags)` stands in for
`response_header_bytes`'s `(header, header_mask, tx_flags)` return shape:
an empty header/mask makes the existing prepend step
(`mask_data = header_mask ++ setup.mask_data`) a structural no-op, so no
separate branch is needed there. When `raw_mode` is `false`, behavior is
byte-for-byte unchanged — `response_header_bytes` still runs unconditionally
regardless of `Condition` (ADR-173 Decision 4), still resolved from
ComParams/UniqueRespIdTable exactly as before.

**3. A single shared `raw_mode_addressing_flags` fold** (computed once,
early in the function, from `setup.tx_flag_bits` via
`rpc_primitive::raw_mode_tx_flag_bit_to_j2534` — now `pub(super)`, mirroring
`rpc_primitive::compute_j2534_tx_flags`'s identical RawMode `TxFlagBits`
fold for an ordinary `CoptSendrecv` TX) is reused for three purposes that
were previously three separate, ComParam-derived computations:

   - the periodic-cap check's ISO15765 `4..=11`/`5..=11` extended-addressing
     split (previously derived from `can_addressing`, ComParam-derived and
     meaningless for a RawMode client with no reason to configure
     `CP_Can*Format` — mirrors `rpc_primitive::resolve_send_recv_tx`'s
     identical RawMode `extended_addressing` derivation, there via
     `compute_tx_prefix`/`ISO15765_ADDR_TYPE`);
   - the transmitted message's `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`
     TxFlags bits (previously always `tx_header::can_addressing_tx_flags`'s
     ComParam-derived contribution — now suppressed under RawMode in favor
     of the client-authoritative fold, mirroring
     `rpc_primitive::apply_resolved_tx_flags`'s identical RawMode
     suppression for ordinary `CoptSendrecv` TX); and
   - the mask/pattern response template's own addressing basis
     (`response_tx_flags`, which the template size-range check's
     `response_extended_addressing` derives from) — reusing the identical
     fold rather than a separately-resolved response-side value.

   Reusing one fold for all three closes what would otherwise be three
   independent RawMode-derivation sites to keep in sync, and directly
   explains the accepted residual below: since `repeat_msg_data`'s TxFlags
   and the mask/pattern template's addressing basis both derive from the
   SAME fold, a RawMode client cannot express asymmetric CAN-ID width
   between the two even if it wanted to — the packed wire format gives it
   only one `tx_flag_bits` field to set both from.

**4. ADR-198 Decision item 7's ISO14230 manual-checksum `+1`-byte widening**
(`raw_mode && !checksum_mode` on an ISO14230 link, 259→260) is added to the
mask/pattern template size-range check, mirroring
`rpc_primitive::resolve_send_recv_tx`'s/the `CoptStartcomm` fast-init size
check's identical computation. **Not** added to the periodic-cap check on
`repeat_msg_data` — SAE J2534-1 v04.04 §7.2.7's flat 12-byte periodic cap
already makes any such widening moot there (12 is already below both 259
and 260), so the periodic-cap check needed no change beyond its
`extended_addressing` derivation (item 3 above).

**5. `checksum_mode` is snapshotted into the function's critical-section
tuple** alongside the pre-existing `raw_mode` (which was previously read
directly off `link.raw_mode` only inside the now-removed rejection block,
not otherwise captured) — both are `LogicalLinkState` fields fixed for the
CLL's lifetime, resolved once at `CreateComLogicalLink` (ADR-196/ADR-198).

**6. `raw_mode_tx_flag_bit_to_j2534`'s own doc comment is corrected**: it
previously asserted this call site "must keep dropping these two bits
unconditionally," which this ADR makes false. The function is changed from
private to `pub(super)` so `rpc_misc` can call it directly, matching this
crate's existing convention for other `rpc_primitive` helpers `rpc_misc`
already reuses (e.g. `tx_flag_bit_to_j2534`).

## Consequences

- Non-RawMode (`raw_mode == false`) behavior is byte-for-byte unchanged —
  every pre-existing non-RawMode `repeat_message.rs` test passes unmodified
  (verified directly, not merely by running the full suite:
  `start_then_query_succeeds_on_a_connected_opted_in_can_link` was run in
  isolation as the regression anchor).
- **Accepted residual, functional-correctness-affecting — asymmetric
  addressing between the transmitted message and the response template is
  not expressible under RawMode, and silently mismatches when the client's
  real ECU actually has one**: the packed `REPEAT_MSG_SETUP` wire format
  (this crate's own hand-packed bytearray, not the frozen proto —
  `unpack_repeat_message_setup`/`pack_repeat_message_setup`, `rpc_misc.rs`)
  has exactly one `tx_flag_bits` field shared by all three `RepeatMsgData`
  messages, so `response_tx_flags` (item 3 above) always inherits the
  transmitted message's own addressing-width bit. This is not a narrow,
  unexercised configuration: this crate's own RawMode=OFF regression
  coverage for this exact IOCTL already proves an ECU that transmits on one
  CAN-ID width and responds on another is real and supported today
  (`tests/grpc_mock/repeat_message.rs`'s
  `condition_one_slot_terminates_immediately_on_a_frame_with_the_wrong_response_format`/
  `..._does_not_terminate_on_a_frame_with_the_correct_response_format`, and
  `tx_header.rs`'s
  `response_header_bytes_tx_flags_reflect_response_addressing_not_request`
  — the composition function this diff's RawMode path bypasses has no other
  production call site, so this is that function's own dedicated coverage
  for precisely this asymmetry, not an incidental capability). A RawMode
  client whose real ECU has this asymmetry gets a silently wrong device-side
  match basis: a genuine `Condition == 1` match can be misclassified as a
  non-match and terminate the slot prematurely; a `Condition == 0` slot's
  genuine stop condition can go unrecognized and retransmit indefinitely
  until a manual `PDU_IOCTL_STOP_REPEAT_MESSAGE`; or an unrelated
  coinciding-Data-bytes frame at the wrong width can be misclassified as a
  match. Not fixed here: a real fix needs either a second
  `tx_flag_bits`-equivalent field scoped to the response template (a
  versioned extension of this crate's own packed format — its existing
  strict trailing-bytes rejection makes a v2 field cleanly detectable) or a
  design decision on deriving it some other way. Two derivation
  alternatives were considered and rejected: sniffing the template's own ID
  byte values for 29-bit-ness (an 11-bit CAN ID value is also a legal 29-bit
  ID value, so no byte pattern distinguishes the two, and the client's own
  mask can zero out the ID bytes entirely, leaving nothing to sniff); and
  consulting `CP_CanRespUUDTFormat`/`CP_CanRespUSDTFormat` under RawMode for
  just this one classification bit (splits addressing authority between a
  client-supplied TxFlags channel for the request and a ComParam channel for
  the response, contradicting ADR-196/198's own RawMode contract that
  `TxFlagBits` is the client's sole addressing-metadata channel; silently
  changes behavior depending on whether an unrelated UniqueRespIdTable
  happens to be configured; and still leaves UUDT-vs-USDT/AE-byte asymmetry
  unresolved). Recorded as a P2 backlog bullet in
  `j2534-0404-service/docs/implementation-notes.md` (not a low-priority
  narrow gap), replacing the entry this ADR closes; `docs/rpc-api-guide.md`
  carries an explicit client-facing warning.
- **ADR-165** gains a Status annotation: Decision item 3's response-header/
  mask composition convention (`tx_header::response_header_bytes`, prepended
  to the client's mask/pattern) is now scoped to RawMode=OFF CLLs only —
  RawMode=ON composes no service-derived header at all, per this ADR.
- **ADR-198** gains a Status annotation: its Consequences section's
  "pre-existing residual... widened by this ADR" bullet (SAE J2534-2 clause
  14 Repeat Messaging's unconditional RawMode rejection) is now closed by
  this ADR.
- `docs/rpc-api-guide.md`'s `PDU_IOCTL_START_REPEAT_MESSAGE` description is
  updated to describe the RawMode template shape and the ChecksumMode=OFF
  trailing-checksum-byte don't-care technique (clause 14.2.2.1's own
  don't-care-beyond-`DataSize` mechanism, which a RawMode/ChecksumMode=OFF
  K-line client can use — by masking out the checksum byte's own mask
  position — to compare a response template without pinning an exact
  checksum value).

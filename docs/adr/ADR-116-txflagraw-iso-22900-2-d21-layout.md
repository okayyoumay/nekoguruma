# ADR-116: `TxFlagRaw` Is the ISO 22900-2 D.2.1 Byte-Array Layout, Not a Native J2534 `TxFlags` u32

**Date:** 2026-07-23
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`compute_j2534_tx_flags`),
             `vci-service-interface/src/proto/service.proto` (`ComPrimitiveCtrlData.tx_flag_raw`),
             `vci-service-interface/src/bindings/vci.service.rs` (regenerated doc comments)

## Context

`iso22900-2-conformance-audit.md`'s A2-10 flagged a contradiction: the proto
comment on `ComPrimitiveCtrlData.tx_flag_raw` (`service.proto:485`, "TxFlag
passed through to `PDU_COP_CTRL_DATA.TxFlag`") names the ISO 22900-2 D.2.1
(Table D.4) 4-byte `TxFlag` byte-array layout, but
`j2534-0404-service::compute_j2534_tx_flags` byte-copied the raw bytes as an
already-native little-endian J2534 `TxFlags` u32 (per its own doc comment,
"escape hatch for J2534-specific flags"). The two layouts place the same
named bits at different bit positions -- e.g. `ISO15765_FRAME_PAD` is D.2.1
byte 3 bit 6 (raw byte sequence `[0x00, 0x00, 0x00, 0x40]`, which the old
little-endian-u32 interpretation would have read back as `0x40000000`) but
J2534 `TxFlags` bit 6 (`0x00000040`, SAE J2534-1 Figure 45) -- so a client
following the proto's documented ISO layout would have a bit silently land
in the wrong position or be dropped by range-masking entirely.

This is a load-bearing wire-contract ambiguity (which byte layout callers
must use), not an implementation detail inferable from context, so per
CLAUDE.md's "stop and ask" rule the two interpretations were put to the
user rather than resolved by guessing. The user confirmed the ISO 22900-2
D.2.1 layout, matching the proto's own documented intent and matching how
`iso22900-service::convert.rs`'s `tx_flag_to_iso` already treats the same
shared `tx_flag_raw` field for its own service: raw bytes are passed
straight through, unmodified, to the native D-PDU API's
`PDU_COP_CTRL_DATA.TxFlag`, which *is* the D.2.1 byte array by definition.
`j2534-0404-service`, as a D-PDU-to-J2534 adapter, must instead decode that
D.2.1 layout and translate each bit to its J2534 equivalent.

## Decision

`compute_j2534_tx_flags`'s `TxFlagRaw` arm decodes the D.2.1 (Table D.4)
byte positions directly, one bit at a time, instead of reinterpreting the
byte slice as a u32:

- Byte 2 bit 1 (`WAIT_P3_MIN_ONLY`) -> `j2534_0404::TX_WAIT_P3_MIN_ONLY`
  (`0x200`).
- Byte 3 bit 6 (`ISO15765_FRAME_PAD`) -> `j2534_0404::TX_ISO15765_FRAME_PAD`
  (`0x40`).
- Byte 2 bit 0 (`CAN_29BIT_ID`) and byte 3 bit 7 (`ISO15765_ADDR_TYPE`) are
  **not** decoded, for the same reason the named-bit arms already skip
  `TxFlagCan29bitId`/`TxFlagIso15765AddrType` (ADR-062): every call site
  applies `apply_resolved_tx_flags` immediately afterward, which
  unconditionally overwrites those two J2534 bit positions from the CLL's
  resolved CAN addressing, superseding any client-requested value by either
  representation. Decoding them here would be dead work.
- Byte 0 bits 6/5 (`SUPPRESS_POS_RESP`/`ENABLE_EXTRA_INFO`) and all of byte 1
  (reserved) have no J2534 `TxFlags` equivalent at all (SAE J2534-1 Figure
  45 defines no such bits) and are dropped, same as the named-bit `_ => 0`
  fallback already does for these two `TxFlagBit` variants.
- A byte position past the end of `raw` (fewer than 4 bytes supplied) reads
  as `0`, matching the previous implementation's zero-padding behavior; a
  `raw` longer than 4 bytes has its extra bytes ignored, since D.2.1's
  layout is defined as exactly 4 bytes.

`service.proto`'s `tx_flag_raw` comment is extended to spell out the D.2.1
layout and cite this ADR, so a future reader doesn't have to reconstruct
the contract from this decision record. `compute_j2534_tx_flags`'s own doc
comment is updated to match.

## Consequences

- A client that was relying on the previous (undocumented, contradicted-by-
  the-proto-comment) behavior -- passing an already-native J2534 `TxFlags`
  u32 as little-endian raw bytes -- gets different (per the resolved
  contract, correct) results now. No such caller is known to exist inside
  this repository (no test previously exercised `TxFlagRaw` on
  `j2534-0404-service`); this is a behavior change for any external caller
  that had adapted to the old, buggy mapping.
- `SCI_MODE`/`SCI_TX_VOLTAGE` (J2534 `TxFlags` bits 22/23) have no D.2.1
  raw-byte position and were never reachable via `TxFlagRaw` under the new
  decode (nor, in practice, meaningfully under the old one, since
  `apply_resolved_tx_flags` only ORs `ComParamSet::sci_tx_flags()` in
  rather than clearing a client-supplied bit first) -- consistent with
  ADR-062's decision that these two bits come from ComParams only, never
  from caller-supplied flags of either representation.
- Regression coverage:
  `j2534-0404-service/tests/grpc_mock/comparam_tx.rs::iso15765_frame_pad_and_wait_p3_min_only_decoded_from_raw_tx_flag_iso_d21_layout`
  and `::iso15765_raw_tx_flag_can_addressing_bits_ignored_and_overridden`.

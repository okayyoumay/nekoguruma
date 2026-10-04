# ADR-167: Extend the KWP/ISO14230 RX-Direction Frame Parser for CARB Address Mode

**Date:** 2026-08-10
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`kwp_header_and_payload_len`,
             `header_footer_len`'s doc table), `j2534-0404-service/docs/implementation-notes.md`,
             `docs/adr/ADR-051-...md` (Status line, partial supersession)

## Context

ADR-166 corrected the TX-direction KWP/ISO14230 header composer
(`tx_header::kwp_header_bytes`/`response_header_bytes`) to branch on the format
byte's top two bits (`format & 0xC0`) per ISO 22900-2 Table 76, rather than bit
7 alone — closing a bug where this codebase's own CARB/ISO9141-2-configured
presets (`iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2`,
`comparam_defaults.rs`) composed a malformed request header. That ADR's
Consequences section explicitly left the RX-direction counterpart out of
scope: `events.rs::kwp_header_and_payload_len`, the frame parser that splits a
*received* KWP/ISO14230 frame into `(header_len, payload_len)`, still
recognizes only the ISO14230 addressed/unaddressed distinction (format byte
bit 7) and does not recognize the CARB shape (`format & 0xC0 == 0x40`) at all.

This function's result feeds `header_footer_len`, whose own `(header, footer)`
split directly becomes `ResultData.data_bytes` (payload only) versus
`ResultData.extra_info.header_bytes`/`footer_bytes` (ADR-051) for every frame
this service delivers to a gRPC client — and, per that code's own governing
comment, expected-response mask/pattern matching and RC-byte-offset detection
run against the split-off payload, not the raw frame. Misparsing a CARB
frame's boundary therefore corrupts what a client actually sees as the
response, and can break response-pattern matching, for every preset using
this address mode.

Unlike the two branches this function already handles — both self-describing,
since the frame's own bytes declare a payload length (embedded in the format
byte's low 6 bits, or a separate trailing length byte) — CARB has no length
field anywhere in the frame at all: the wire shape is simply
`[format, target, source, <everything else>]`. This function also has no
access to the connecting CLL's connect-flag/ComParam state (it is a pure
`data: &[u8]` byte parser), so it cannot itself distinguish "the remainder is
all payload" from "the remainder is payload plus a trailing checksum byte" the
way its two existing branches can from the frame's own declared length.

A design-advisor consult evaluated this exact ambiguity against two
candidates: treating CARB's entire remainder as payload (footer always empty),
or mirroring the sibling `header_footer_len` J1850 branch's convention (a
fixed 1-byte footer whenever a byte remains after the header, justified there
by J1850's own fixed-size CRC and total absence of a length field — the same
underlying reason CARB lacks one). The deciding fact: this service's own
connect-time flag composition (`rpc_link::connect_flags`, ADR-065) never sets
the K-line manual-checksum connect flag — every channel this service opens
therefore has the vendor DLL both compute/verify and strip a KWP/ISO9141
checksum before this service ever sees the frame, matching SAE J2534-1's own
delivery model for that flag state (checksum-verified frames are delivered
without a trailing checksum byte; a bad checksum is discarded outright, never
delivered for this service to mis-parse). The 1-byte-footer convention would
therefore misclassify the true last payload byte of every CARB frame this
service can actually receive — the opposite of what J1850's identical-looking
convention gets right for J1850's own delivery model. The remaining case (this
service somehow later exposing the manual-checksum flag) has no
better-defined answer either: SAE J2534-1 treats that mode's whole message as
undifferentiated data with a manufacturer-specific checksum scheme, not
necessarily 1 byte, so no parsing rule can spec-correctly separate payload
from checksum there without information this codebase does not currently
expose. Threading connect-flag context into this function to attempt that
distinction is therefore not justified: the input driving the choice is a
constant today, and even a non-constant input would not resolve the
undefined-format case.

## Decision

`kwp_header_and_payload_len` now recognizes `format & 0xC0 == 0x40` (CARB/
ISO9141-2 exception addressing) as its own case, checked before the existing
bit-7 addressed/unaddressed logic:

- A frame at least 3 bytes long: fixed 3-byte header (format, target,
  source), with the entire remainder treated as payload — `Some((3,
  data.len() - 3))`. No footer is ever reported for this address mode.
- A frame shorter than 3 bytes (cannot even contain the fixed header):
  `None`, mirroring this function's own existing too-short-for-a-separate-
  length-byte behavior. `header_footer_len`'s caller already treats a `None`
  result as "report the whole frame as header, no payload/footer split at
  all" (`_ => (data.len(), 0)`), so no caller-side change is needed.

The existing bit-7-based branches (`0x80`/`0xC0` addressed-with-length,
`0x00` unaddressed) are unchanged.

## Consequences

- **Correct payload delivery for ISO 9141-2 CARB presets on RX:** a
  genuinely-received CARB-format response (e.g. from an ECU replying to this
  codebase's own `iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2`
  presets) now has its header/payload boundary parsed correctly, fixing
  `ResultData.data_bytes`, expected-response pattern matching, and RC-byte-
  offset detection for those presets. This closes ADR-166's own
  Consequences residual.
- **`ADR-051`'s Status line** is annotated (not replaced) to record this
  partial supersession: its RX-direction K-line header/footer parsing
  description is extended by this ADR's CARB-address-mode case; ADR-051's
  other decisions (CAN/ISO15765 header split, J1850 header/footer split, the
  payload-only client contract) remain in force unchanged.
- **Accepted residual, coupled to `rpc_link::connect_flags`'s current
  invariant (ADR-065):** this decision's correctness for the common case
  depends on this service never setting the K-line manual-checksum connect
  flag. If a future change ever exposes that flag as client-settable (e.g.
  mapping a D-PDU checksum-mode ComParam), this function's footer-always-
  empty assumption for CARB frames must be revisited — SAE J2534-1 does not
  define a spec-guaranteed way to delimit payload from checksum in that mode
  regardless, so the correct fix there (if ever needed) is not obviously a
  parsing change, and is deliberately left unresolved here rather than
  speculatively designed against a case this codebase cannot reach today.
- **Spec-verification basis:** this decision is verified against SAE
  J2534-1's own connect-flags table (§7.2.3.2) and its ISO14230 RX message
  delivery figure (§8.3 Figure 42), both available in the sibling
  `vehicle-comm-specs` repository, rather than against ISO 14230-2's own
  text directly (not present in that repository at the time of this ADR) —
  matching ADR-166's identical caveat for the sibling TX-side decision.

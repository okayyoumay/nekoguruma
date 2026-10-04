# ADR-166: Correct KWP/ISO14230 TX Header Composition for CARB/ISO9141-2 Address Mode

**Date:** 2026-08-09
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/tx_header.rs` (`build_tx_message`'s
             `kwp_header_bytes`, `response_header_bytes`'s ISO9141/ISO14230 branch),
             `j2534-0404-service/docs/implementation-notes.md`,
             `docs/adr/ADR-050-tx-message-id-header-construction-from-comparams.md`
             (Status line, partial supersession)

## Context

ADR-050 established a fixed-shape KWP/ISO14230 TX header: format byte, target
address, source address, then an explicit trailing length byte — always four
bytes, regardless of how `CP_PhysReqFormatPriorityType`/`CP_FuncReqFormatPriorityType`
is configured. This model was carried unchanged into `tx_header::build_tx_message`'s
`kwp_header_bytes` helper (shared by ordinary `CoptSendrecv`/tester-present/periodic
TX composition, ADR-050, and — new in ADR-165 — Repeat Messaging's `RepeatMsgData[0]`).

A Codex review round (PR #42, ADR-165's Repeat Messaging PR, round 20) reported that
this fixed shape malforms every KWP/ISO14230 request built from this codebase's own
ISO 9141-2 presets (`iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2`,
`comparam_defaults.rs`). Investigation (a design-advisor consult, since the literal
suggested fix — mirroring `events.rs::kwp_header_and_payload_len`'s RX-side bit-7
address-presence check — would itself have broken these same presets) found the root
cause: the format byte's top two bits jointly select the wire shape, not just bit 7.
ISO 22900-2's ComParam table (Table 76, both the 2009(E) and 2022 editions) documents
`CP_PhysReqFormatPriorityType`'s default differently per protocol family — ISO 9141-2's
default is a literal, addressed, no-length-byte header shape (this codebase's presets
configure it as `0x6C`/`0x68`, the CARB/ISO9141-2 exception-addressing convention, bit 6
set and bit 7 clear), while ISO 14230's default (`0x80`) is addressed with a
stack-generated length carried in a separate trailing byte. A fixed-shape composer that
always appends a trailing length byte therefore corrupts every CARB-format frame by
inserting a byte the wire format doesn't have.

`events.rs`'s own RX-direction parser (`kwp_header_and_payload_len`) has the identical
blind spot — it was written against the ISO14230 addressed/unaddressed distinction alone
and does not recognize the CARB shape either — but is out of scope for this ADR (see
Consequences).

## Decision

KWP/ISO14230 header composition now branches on the format byte's two address-mode bits
(`format & 0xC0`), matching ISO 22900-2 Table 76's per-protocol default semantics:

- **`0x40`** (CARB/ISO9141-2 exception addressing, e.g. this codebase's `0x6C`/`0x68`
  presets): a 3-byte header — format, target, source — verbatim, with **no** trailing
  length byte. The format byte's low 6 bits are not a length field in this mode.
- **`0x80`/`0xC0`** (ISO14230 physical/functional addressing): format, target, source,
  then either an explicit trailing length byte (when the format's configured low 6 bits
  are `0`, ADR-050's original shape) or the length is carried directly in the format
  byte's own low 6 bits (recomposed here when the configured value is nonzero, since
  ISO 22900-2 documents this ComParam's low-6-bit field as stack-generated from the
  actual payload length, not a client-supplied literal — capped at 63, the field's
  6-bit capacity; a longer payload is rejected as unencodable in this mode rather than
  silently truncated).
- **`0x00`** (unaddressed): a 2-byte header — format, then a trailing length byte — no
  target/source bytes at all.

Applied identically in two places that must agree with each other:

1. `tx_header::build_tx_message`'s `kwp_header_bytes` (the TX/request-direction
   composer, shared by ordinary TX composition and Repeat Messaging's transmitted
   `RepeatMsgData[0]`) — the primary fix this round's finding was about.
2. `tx_header::response_header_bytes`'s ISO9141/ISO14230 branch (the RX-direction
   stop-condition mask/pattern template composer, ADR-165, corrected in an earlier
   round for a narrower bit-7-only distinction that shared the identical CARB blind
   spot) — corrected in the same change so a repeat slot's transmitted frame and its
   own stop-condition template describe the same wire shape.

The fix is applied directly to the shared `kwp_header_bytes`/`build_tx_message`
functions rather than forked into a Repeat-Messaging-only wrapper: unlike a prior,
broader shared-helper gap in this same PR (`can_addressing_tx_flags`, deliberately
left as a documented backlog item because its blast radius spans an unenumerable set
of ordinary TX call sites across the workspace), this function has exactly four
call sites, all already enumerated and `Result`-propagating, and forking it would make
a repeat-transmitted frame diverge on the wire from an otherwise-identical ordinary
`CoptSendrecv` frame — itself incorrect, since SAE J2534-2 clause 14 defines a repeat
message as an ordinary message retransmitted autonomously, not a distinct message
shape.

## Consequences

- **Behavior change for ISO 9141-2 presets:** every KWP request built against this
  codebase's own `iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2` presets (and any
  other CARB-format configuration) now composes a 3-byte header instead of the
  previous, malformed 4-byte one. This is a bug fix, not a behavior contract this
  codebase previously guaranteed correct — those presets' TX frames were unusable
  against a conforming ECU before this change.
- **No change for ISO14230 presets:** the `0x80`/`0xC0` addressed-with-separate-length
  case (the pre-existing common path, including `iso_obd_on_k_line` and similar
  presets) composes byte-identical output to before.
- **Two residuals recorded as backlog items** (`j2534-0404-service/docs/implementation-notes.md`,
  Prioritized Backlog), not fixed in this change — both since resolved:
  - `events.rs::kwp_header_and_payload_len` (the RX-direction parser) still doesn't
    recognize the CARB address-mode shape, so it mis-splits a genuinely-received
    CARB-format frame's header/payload boundary — a pre-existing gap this ADR's
    investigation surfaced but did not fix, affecting ADR-051's header/payload split
    and expected-response matching for ISO 9141-2 presets. Device-side repeat-slot
    matching (ADR-165 Decision 6) does not depend on this parser, so Repeat Messaging
    itself is unaffected by this residual. **Fixed by ADR-167** (extends this same
    `format & 0xC0` gate to the RX-direction parser).
  - `response_header_bytes`'s embedded-length branch cannot predict what value a real
    ECU's *response* format byte's low 6 bits will actually carry on the wire (unlike
    the *request* side, which this ADR fixes by having the service itself compute and
    emit the correct value) — the response-side mask should arguably wildcard that
    position the same way the existing separate-length-byte case already does. Not
    verified/fixed here; flagged as a sibling gap to the round-16 fix that first
    introduced the wildcard-mask technique for the separate-length-byte case. **Fixed**
    (no ADR needed — a straightforward extension of the existing wildcard-mask
    technique to a partial byte position, not a new design decision; see
    `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog for the
    resolution detail).
    - **Accepted residual (Codex review, PR #46; design-advisor consult):** the
      embedded-length template fix cannot cover the case where the same ECU emits a
      response whose payload exceeds the embedded-length field's capacity (a
      response 64 bytes or longer) — that response necessarily takes the
      separate-length-byte wire shape (a 4-byte header) even on a link whose
      `CP_PhysRespFormatPriorityType` is configured with a nonzero embedded length.
      A repeat slot's stop condition is a single fixed-offset mask/pattern template
      (SAE J2534-2 clause 14), which cannot express the "format byte's low 6 bits
      are nonzero" predicate that discriminates the two wire shapes, nor match two
      different header lengths at once. Every alternative considered — always
      composing a 4-byte template, rejecting a nonzero-low-6-bits configuration
      outright, or templating the separate-length-byte shape instead — breaks the
      common 1-to-63-byte embedded case in order to guard this rarer one, so none is
      an improvement. Bounded impact: target/source stay exact-matched, so a
      misaligned comparison only ever runs against a genuine frame from the targeted
      ECU to this tester; worst case is a spurious early stop, or (symmetrically, on
      a link configured the other way) a missed stop bounded by the slot's own
      `TimeInterval` — no misrouting, no data corruption, no cross-ECU effect.
      Deployer guidance: configure `CP_PhysRespFormatPriorityType`'s embedded-length
      bits to match the wire shape of the specific response the stop condition
      targets; prefer a zero-embedded-length configuration when the polled ECU can
      interleave 64-byte-or-longer responses on the same link and a fail-safe missed
      stop is preferable to a shifted match.
- **Spec-verification caveat:** this decision is verified against ISO 22900-2's own
  ComParam table (Table 76, both available editions) rather than against ISO 14230-2's
  own text directly, since the latter is not present in the sibling `vehicle-comm-specs`
  repository at the time of this ADR.
- **ADR-050's Status line** is annotated (not replaced) to record this partial
  supersession: its KWP/ISO14230 header-composition description is superseded by this
  ADR's `0xC0`-address-mode model; ADR-050's other decisions (CAN/ISO15765 addressing
  resolution, J1850 header composition, SCI pass-through, the payload-only client
  contract) remain in force unchanged.

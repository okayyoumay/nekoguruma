# ADR-172: Withhold SW-CAN Speed-Transition Confirmation Frames from Content Processing

**Date:** 2026-08-12
**Status:** Accepted (Decision item 3's bit-16 `HV_RX` untagged-delivery half tagged by ADR-191;
             the bit-17/18 withhold itself unchanged)
**Affects:** `j2534-0404-service/src/service/events.rs` (`poll_rx_inner`, `RX_STATUS_FLAGS_MASK`
             area), `j2534-0404-mock/src/lib.rs` (RX injection), ADR-098's content-frame gate
             (extended, not superseded), ADR-164 (its deferred "RX-side bits" residual, partially
             resolved)

## Context

A SAE J2534-2 (DEC2020) already-implemented-phases conformance audit
(`j2534-0404-service/docs/implementation-notes.md`'s "SAE J2534-2 (DEC2020) already-implemented-
phases conformance audit" section) found a live P1 bug: SW-CAN speed-transition confirmation
frames (clause 9.4.1.1 Table 12 — RxStatus bit 17 and bit 18, arriving for both commanded and
automatic speed changes per clause 9.3.2.3) carry explicitly undefined data content, but this
codebase's content-vs-indication gate (`is_content_frame`, `poll_rx_inner`) has no way to
recognize them. `RX_STATUS_FLAGS_MASK` (ADR-098) covers only `RxStatus`'s 5 low bits; a frame
whose only set bit is 17 or 18 evaluates to `rx_status_flags == 0` under that gate and is
processed as ordinary content — its undefined first 4 bytes get parsed as a CAN ID and become
eligible for `UniqueRespIdTable` matching, `ExpectedResponseData` mask/pattern matching, and
pending-RC attribution, any of which could misattribute or corrupt a real diagnostic exchange.

Fixing this is not a bare mask edit. Bit 17 carries a **different meaning on Fault-Tolerant CAN**
(clause 20, Table 86 — `LINK_FAULT`, which tags a correctly-received real message that must
remain content-eligible), and bit 16 (`SW_CAN_HV_RX`) is itself genuine content on SW-CAN links.
Only SW-CAN-family bits 17/18 specifically need excluding. This is also this mechanism's first
protocol-family-conditional case — every bit `RX_STATUS_FLAGS_MASK` already excludes applies
uniformly regardless of protocol — so it revises how the gate works, not just what it covers, and
gets its own ADR per this repo's normal rule for a non-obvious design choice.

ADR-164 (SWCAN, Phase 4) already anticipated this gap and explicitly deferred it: RxStatus bits
16-18 need a new `RxFlag` byte-range extension to *forward* as a positive indication to the
D-PDU client (ADR-098's forwarding path only carries the 5 low bits into `RxFlag` byte 3), a
genuinely separate, harder design question, tracked as its own P2 backlog item
(`implementation-notes.md`'s Prioritized Backlog). This ADR resolves only the *exclusion* half of
that gap (stopping active mishandling as content) — the *forwarding* half (surfacing a positive
indication to the client) remains deferred, unaffected by this decision.

## Decision

1. **A SW-CAN-family frame carrying RxStatus bit 17 or 18 is withheld entirely** (not delivered,
   not matched against anything), rather than delivered as an untagged indication. This check is
   inserted early in `poll_rx_inner`'s per-message loop, keyed on the frame's own reported native
   protocol ID (`resources::is_sw_protocol_id`, already used elsewhere in this file) against the
   frame's raw, unmasked `RxStatus` value — not folded into `RX_STATUS_FLAGS_MASK`/
   `is_content_frame`'s existing `u8`-truncated computation, since bits 17/18 don't fit in that
   byte and that computation is dual-purpose (it also feeds ADR-098's RxFlag forwarding). This
   mirrors this codebase's existing precedent of checking a wider raw `RxStatus` value ahead of
   the 5-bit-scoped gate for a different purpose (the FlowControl-capture check).

2. **Withhold, not deliver-untagged, because the data is genuinely undefined and this codebase
   has no representable tag for it yet.** A delivered-but-empty-`RxFlag` frame would reproduce
   the same class of bug this ADR closes, just one step downstream (garbage bytes silently
   entering `data_bytes` as an apparently-Normal Message, indistinguishable from real content by
   any client-visible signal). Once the ADR-164-deferred RxFlag-byte-extension work lands, this
   withhold can be replaced with a properly tagged delivery — this ADR does not do that work now.

3. **Only SW-CAN family, only bits 17/18.** Bit 16 stays content-eligible on SW-CAN links; every
   bit on every non-SW-CAN protocol (including FT-CAN's own bit 17, `LINK_FAULT`) is untouched —
   the check is scoped by `is_sw_protocol_id` and nothing else changes.

4. **A nonconforming device that misreports a received message's native protocol ID** (e.g.
   reports plain `CAN` on what is physically an SW-CAN wire) would bypass this withhold — accepted
   as consistent with this codebase's existing per-message protocol-ID trust model (ADR-160's
   `native_mixed` flag already relies on the same per-message `ProtocolID` field being honest).

## Consequences

- Closes the live-mishandling P1: SW-CAN speed-transition frames no longer corrupt
  `UniqueRespIdTable` matching, `ExpectedResponseData` matching, or pending-RC attribution.
- **Accepted residual, cross-referencing ADR-164's own deferred item:** these frames remain
  entirely invisible to the D-PDU client (no positive indication) until the RxFlag-byte-extension
  work resolves ADR-164's other deferred half. `SW_CAN_HS`/`SW_CAN_NS`'s own success/failure is
  still separately observable via the triggering RPC's own return status, so this residual affects
  only the passive, transition-*confirmation* RX signal, not command outcome visibility.
- First protocol-family-conditional case in this content/indication gate — a future family with
  its own RxStatus bit reuse should follow this same "check the raw value against a family
  predicate before the shared low-bit gate" shape rather than growing `RX_STATUS_FLAGS_MASK`
  itself, which cannot represent bits above 7 without breaking its existing `u8` role in ADR-098's
  forwarding path.
- **Accepted residual (`edge-case-hunter` follow-up on this PR):** this withhold trusts each
  frame's own reported native `ProtocolID` (`resources::is_sw_protocol_id(frame_protocol_id)`) to
  correctly identify an SW-CAN link, the same trust model point 4 above already accepts for a
  nonconforming device. A narrower, currently-unverified question sits inside that same trust
  boundary: `can_channel_mode = "native-mixed"` (ADR-160) combined with an SW-CAN or FT-CAN
  resource is an interaction no test in this codebase exercises (SW/FT links are always
  pin-selected, so they already take a different, qualified-link code path than native-mixed's own
  per-CLL routing flag targets — `j2534-0404-service/docs/implementation-notes.md`'s existing
  qualified-link/native-mixed interaction note covers the general shape) — whether a genuinely
  conforming device could ever report an SW-CAN frame's `ProtocolID` as something other than
  `SW_CAN_PS`/`SW_ISO15765_PS` under that specific combination, in a way that would let a
  speed-transition frame slip past this withhold, is a SAE J2534-2 clause 8-vs-clause 9
  interaction this ADR does not resolve. Pre-existing (not introduced by this ADR); flagged for a
  `design-advisor` consult if `native-mixed` + SW/FT ever becomes a combination this codebase
  actually needs to support, rather than investigated speculatively here.

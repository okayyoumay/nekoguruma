# ADR-171: J1850 RX Footer Derived from Native ExtraDataIndex, Not a Fixed CRC Byte

**Date:** 2026-08-11
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`header_footer_len`, `poll_rx_inner`),
             `j2534-0404-service/src/service/events_rx_routing.rs` (`build_cll_rx_entries`),
             `j2534-0404-mock/src/lib.rs` (`StoredMessage`, `write_to_passthru`, J1850 RX fixtures),
             `j2534-0404-service/tests/grpc_mock/rx_header_split.rs`,
             [ADR-051](ADR-051-iso15765-resultdata-header-in-extra-info.md) (J1850 Decision row,
             partial supersession — see its amended Status line)

## Context

A SAE J2534-1 v04.04 conformance audit (recorded in
`j2534-0404-service/docs/implementation-notes.md`'s "SAE J2534-1 v04.04 conformance audit
(2026-08-11)" section) found that `header_footer_len`'s J1850 branch
(`j2534-0404-service/src/service/events.rs:1125-1128`) unconditionally treats the last byte of
every received J1850 PWM/VPW frame as a 1-byte CRC footer and strips it out of
`ResultData.data_bytes`. This was a deliberate decision, codified in
[ADR-051](ADR-051-iso15765-resultdata-header-in-extra-info.md)'s Decision table (J1850 row): 3
header bytes, then "1 byte (CRC), whenever a trailing byte remains."

That model is wrong. SAE J2534-1's own description of the J1850 wire `Data` field (§8.3/§8.5,
Figure 42) accounts for a fixed 3-byte header plus up to 7 data bytes, with no CRC byte anywhere
in that arithmetic — J1850's on-wire checksum is verified and stripped by the interface itself,
before delivery, the same way this codebase already correctly models K-line's managed checksum
(no fabricated footer byte there). The spec instead defines a separate, general mechanism for
marking trailing bytes on a received message: the native `PASSTHRU_MSG.ExtraDataIndex` field
(§8.2/§8.5) reports the offset within `Data` at which any In-Frame Response (IFR) bytes begin;
everything from that offset to `DataSize` is IFR, not core protocol data. For every received
message except J1850 PWM carrying IFR bytes, a conforming device reports `ExtraDataIndex ==
DataSize` (i.e. no trailing bytes) — J1850 VPW included, since VPW has no IFR mechanism at all.

`ExtraDataIndex` already has a safe accessor (`j2534-0404/src/message.rs`'s
`extra_data_index()`), but it was never read anywhere in `j2534-0404-service` — the value is
silently dropped in `poll_rx_inner` when the native message is reduced to a bare byte slice
before reaching `header_footer_len`. The mock has no representation of it either:
`j2534-0404-mock`'s `StoredMessage` carries no such field, and `write_to_passthru` unconditionally
reports `ExtraDataIndex = DataSize` regardless of what bytes were actually injected — silently
papering over the very gap this ADR closes, and contradicting the synthetic trailing byte its own
J1850 RX fixtures inject to exercise the (buggy) footer-stripping code.

Concretely, this bug drops one real payload byte from **every** J1850 RX message today (a
conforming VPW response like a 3-header + 5-data-byte frame loses its last data byte into
`footer_bytes`), and corrupts downstream `ExpectedResponseData` matching and `CP_RCByteOffset`
detection, which both operate on the (wrongly truncated) split. This is a correctness fix, not
merely a bug fix with no design alternative — it revises a prior ADR's protocol interpretation and
introduces new state (an `ExtraDataIndex`-shaped field) into both the service's RX pipeline and
the mock's message model, so it gets its own ADR per this repo's normal rule for revising a prior
Decision.

## Decision

1. **`header_footer_len` gains a new `Option<usize>` parameter** (native `ExtraDataIndex`, `None`
   when unavailable). **Only the J1850 PWM arm derives the footer length from it**, defensively
   clamped, instead of assuming a fixed 1-byte CRC: header is `min(3, len)`; footer is
   `len - extra_data_index` when `Some(extra_data_index)` falls within `header..=len`, otherwise
   `0` (see point 3 for why this degrades safely rather than trusting the value blindly). **The
   J1850 VPW arm ignores `ExtraDataIndex` unconditionally** and always reports an empty footer —
   VPW has no IFR mechanism at all, so even an in-range value from a non-conforming or stale
   adapter must not be trusted to mean "real IFR bytes" for a protocol that structurally has none
   (PWM and VPW were originally one shared match arm; split into two after a Codex review round 1
   finding caught this — an in-range-but-wrong `ExtraDataIndex` on VPW would otherwise have
   silently reintroduced the exact class of payload-truncation bug this ADR exists to fix, just
   sourced from a different field). No other protocol branch's logic changes — CAN/ISO15765/KWP
   already compute their own footer length independently of this field and don't consume the new
   parameter.

2. **Plumbing:** in `poll_rx_inner`, `ExtraDataIndex` is read off the native `PassThruMessage`
   alongside the existing `frame_protocol_id` capture and passed straight into the
   `header_footer_len` call at the RX-loop split site — no change to `process_frame_for_entry`'s
   own signature, since the value is captured and consumed within the same loop scope. The second
   `header_footer_len` call site, in the fast-init/5-baud-init response path, passes `None`: that
   path is only reachable for K-line links (ISO9141/ISO14230), which never populate or read
   `ExtraDataIndex` in the first place, so this is behavior-neutral there — not a J1850 gap.

3. **`ExtraDataIndex` is treated as an untrusted, FFI-crossing value, not an invariant.** It is
   reported by an arbitrary vendor DLL (or, in tests, injected directly by the mock), not
   validated at any earlier layer. §8.5/§8.6 both establish that a conforming device keeps it
   within `0..=DataSize`, but conformance is a requirement on the far side of the FFI boundary, not
   a guarantee this service can rely on — and a legitimate `ExtraDataIndex == 0` on an indication
   message would, unclamped, misclassify an entire frame as footer. `header_footer_len` therefore
   clamps: any `ExtraDataIndex` outside `header..=len` is treated as absent (full frame delivered
   as payload, empty footer) rather than propagated into a nonsensical split.

4. **Mock (`j2534-0404-mock`) gains a real `ExtraDataIndex` representation.** `StoredMessage`
   gains `extra_data_index: Option<u32>` (`None` = "no extra bytes," the correct default for every
   existing call site converted via `from_passthru`, since `ExtraDataIndex` is don't-care on
   client-written messages per §8.2). `write_to_passthru` reports
   `extra_data_index.unwrap_or(DataSize)` — deliberately unclamped in the mock itself, so a test
   can inject an out-of-range value and exercise the service-side clamp from point 3 as the actual
   trust boundary. The two existing J1850 RX fixture constants
   (`MOCK_J1850_VPW_RESPONSE`/`MOCK_J1850_PWM_RESPONSE`) drop their synthetic trailing byte
   outright rather than repurposing it as IFR data — a delivered CRC byte isn't a real thing under
   this corrected model, and VPW has no IFR mechanism to repurpose it into. A new
   `__mock_inject_rx_msg_with_edi` mock export (and matching test-harness wrapper) lets a test
   inject a genuine IFR-bearing PWM response without widening the existing five-argument
   `__mock_inject_rx_msg` signature.

5. **`CllRxEntry::header_protocol` (`events_rx_routing.rs::build_cll_rx_entries`) is derived
   per `RxEntryKind` instead of a single blanket `l.protocol.j2534_protocol_id()` read** — a
   pre-existing latent bug exposed by point 1 above making `header_footer_len`'s protocol key
   flavor-sensitive for the first time. `l.protocol.j2534_protocol_id()` is only the fixed VPW
   *initial probe candidate* for the `SAE_J1850` bus-agnostic resources (ADR-070); it never
   reflects a PWM auto-detect result recorded separately in `l.hw_protocol_id`. Before this
   split mattered only for a size-range/header-construction check and a ComParam allowlist gate
   (`docs/implementation-notes.md`'s "PR-review fix on ADR-070" note, the same bug class's first
   two instances); it now also determines whether a received frame is split through the PWM or
   VPW arm, and only the PWM arm consumes `ExtraDataIndex` — so a PWM-detected bus-agnostic link
   was silently losing IFR footer bytes to the VPW arm's unconditional empty footer. Fixed by
   deriving `header_protocol` per `RxEntryKind`: a `SoftwareIsoTp` entry keeps
   `l.protocol.j2534_protocol_id()` (the logical ISO15765 identity must govern this split, not
   the raw CAN `hw_protocol_id` underneath — a software-ISO-TP link's `hw_protocol_id` is plain
   `CAN`); a `Hardware`/`Companion` entry instead uses `resources::base_protocol_id(l.hw_protocol_id)`,
   which also supplies the `_PS`/`_CHx` normalization `header_footer_len`'s raw numeric match
   needs (the old `j2534_protocol_id()` path incidentally already provided this for
   ISO15765-family qualified links, but never provided it for J1850's auto-detect case). This
   makes the `can_addressing_by_id` gate (`header_protocol == j2534_0404::ISO15765`) provably
   invariant across the change: every ISO15765-family case — qualified or not, software-ISO-TP
   or not — still collapses to plain `ISO15765` either way, so only J1850/SCI-family cases can
   differ in value, and those never satisfied that gate before or after.

6. **`ADR-051`'s Status line is annotated at the granularity of its J1850 Decision row** (this
   repo's established partial-supersession convention — see ADR-167's precedent for the K-line
   CARB-mode extension), not rewritten wholesale: the fixed-1-byte-CRC footer and its "J1850 has no
   self-describing length field" rationale are superseded by this ADR; every other row/decision in
   ADR-051 remains in force unchanged.

## Consequences

- **Client-visible behavior changes**, correcting silent data loss: a VPW RX frame now delivers
  its true last data byte in `data_bytes` instead of losing it to a fabricated footer; a PWM RX
  frame's `footer_bytes` now reports 0 or more genuine IFR bytes (as reported by the device),
  rather than always exactly 1. `ExpectedResponseData` mask/pattern matching and
  `CP_RCByteOffset`/pending-RC detection, both of which operate on this split, now see the correct
  payload for every J1850 exchange.
- `header_footer_len` gains a fifth parameter; both call sites were audited and updated (RX loop
  passes the real value, fast-init path passes `None` since it is structurally unreachable for
  J1850).
- The mock's `StoredMessage`/`write_to_passthru` gain general `ExtraDataIndex` support, available
  for any future protocol that needs it — not J1850-specific machinery bolted on sideways.
- **`CllRxEntry::header_protocol` now follows the detected J1850 flavor for a bus-agnostic link**,
  correcting silent IFR-footer data loss on a PWM-detected `SAE_J1850` resource. This is the third
  instance of the `protocol`-vs-`hw_protocol_id` bug class in this codebase — the first two
  (`resolve_send_recv_tx`'s TX size-range check, and the `CP_NetworkLine`-class ComParam allowlist
  gate) are catalogued in `j2534-0404-service/docs/implementation-notes.md`'s "PR-review fix on
  ADR-070" note. `bind_frame`'s shared J1850 KWP-source-id arm (`events.rs`, matches both
  `J1850PWM | J1850VPW` in one arm) now also keys off the detected flavor via `header_protocol` —
  behavior-identical today, since both flavors share that one arm, but now correct-by-construction
  if the two ever need to diverge. The `can_addressing_by_id` gate is proven invariant across the
  change (see Decision item 5): every ISO15765-family case collapses to plain `ISO15765` under
  both the old and new derivation, so only J1850/SCI-family values can differ, and neither ever
  satisfied that gate.
- **Accepted residual:** this fix only threads `ExtraDataIndex` into the one branch that needs it
  today (J1850). Nothing else in this codebase's RX pipeline currently has a legitimate use for a
  general-purpose `ExtraDataIndex`/IFR concept (CAN/ISO15765 footers are always empty; KWP's
  footer comes from an embedded length byte per ADR-166/167), so no other branch was touched or
  needs to be — a future protocol with its own genuine IFR semantics can extend the same plumbing
  rather than re-deriving it.

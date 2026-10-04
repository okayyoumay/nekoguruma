# ADR-198: `CllCreateFlag` RawMode/ChecksumMode Support, Phase 2 (Hardware K-Line)

**Date:** 2026-08-29
**Status:** Accepted (the Consequences section's "pre-existing residual... widened by
             this ADR" bullet — SAE J2534-2 clause 14 Repeat Messaging's unconditional
             rejection on any RawMode=ON CAN/ISO15765/ISO9141/ISO14230 CLL — is closed by
             ADR-199, which gives that function's own message composition a RawMode-aware
             path. The Consequences section's "Documented Phase 3 residual" bullet is
             closed by ADR-200 Phase 3: SAE J1850/SAE J1939 are now RawMode-admitted
             (J1850 via the existing generic passthrough plus a new ChecksumMode=ON
             requirement; J1939 via a dedicated destination-address TX/RX shim), and
             TP2.0 is resolved as a permanent RawMode exclusion, not left unscoped.
             Every other Decision item remains in force)
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`resolve_cll_create_flag`,
             `rpc_create_com_logical_link`'s protocol allowlist, `connect_flags`,
             the shared-channel join-compatibility check),
             `j2534-0404-service/src/service.rs` (`LogicalLinkState`/`LinkView`
             `checksum_mode`, `TimingChangeConfig::with_request`,
             `AccessTimingConfig::with_request`),
             `j2534-0404-service/src/service/tx_header.rs` (`build_tx_message`,
             doc comment only — no behavior change),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`compute_tx_prefix`,
             `resolve_send_recv_tx`, the `CoptStartcomm` fast-init size check),
             `j2534-0404-service/src/service/events.rs`
             (`kwp_header_and_payload_len` visibility, `observe_timing_response`,
             `observe_registrant_timing_change`), `j2534-0404-service/src/service/protocol.rs`
             (`tx_message_size_range` doc comment only),
             `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_start_repeat_message`'s
             RawMode rejection widened to K-line), `docs/rpc-api-guide.md`,
             `j2534-0404-service/docs/comparam-protocol-support.md`,
             `j2534-0404-service/docs/implementation-notes.md`, `docs/adr/INDEX.md`,
             `docs/adr/ADR-065-passthruconnect-flag-derivation.md` (Status line),
             `docs/adr/ADR-196-cll-create-flag-raw-mode-checksum-mode-phase1.md` (Status line)

## Context

ADR-196 (Phase 1) implemented `CllCreateFlag` RawMode/ChecksumMode
(ISO 22900-2:2022 Annex D.2.3, Table D.6) for base CAN and hardware
ISO15765 only, and left K-line/J1850/J1939/TP2.0 RawMode, and ChecksumMode's
real (non-no-op) semantics, as a documented Phase 2 residual
(`j2534-0404-service/docs/implementation-notes.md`'s backlog).

A `design-advisor` consult (prior to this implementation) scoped Phase 2 to
**hardware K-line (ISO9141 + ISO14230) only**. Software-ISO-TP has no
K-line analog (ADR-046 layers ISO-TP entirely over CAN), so there is
nothing to exclude there the way Phase 1 excludes a software-ISO-TP
ISO15765 CLL. J1850, SAE J1939, and TP2.0 remain out of scope: J1850 has no
native CRC-suppression connect flag in v04.04 (confirmed by direct grep of
`j2534-0404-sys/src/bindings/j2534_v0404.h` — no such flag exists for
J1850), so it cannot get the same clean two-state treatment K-line's
`ISO9141_NO_CHECKSUM` bit gives; J1939/TP2.0 are CAN-based and structurally
unrelated. This residual is recorded as a new Phase 3 backlog bullet
(`implementation-notes.md`), replacing the entry this ADR closes.

ISO 22900-2:2022 Annex D.2.3 Table D.6 (byte 0 bits 7/6), paraphrased —
four states across RawMode/ChecksumMode:

| RawMode | ChecksumMode | Header bytes | Checksum |
|---|---|---|---|
| OFF | (ignored) | service-built | vendor DLL/interface manages it |
| ON | ON | client-built, passed through unchanged | vendor DLL/interface still manages it |
| ON | OFF | client-built, passed through unchanged | client-built, passed through unchanged |

SAE J2534-1 v04.04 §7.2.3.3 Figure 7 (`PassThruConnect`'s `Flags` parameter) names
`ISO9141_NO_CHECKSUM` (bit 9, `CONNECT_FLAG_ISO9141_NO_CHECKSUM` in
`j2534-0404-sys/src/bindings/j2534_v0404.h`) as the native connect-time knob
for this: `0` (default) means the interface generates/verifies the
checksum; `1` means the interface treats the whole message as data,
leaving checksum handling entirely to the application. This maps directly
onto Table D.6's third row: the native bit is set iff `raw_mode &&
!checksum_mode`.

SAE J2534-1 §8.3 Figure 42 (message-size table) gives ISO14230 a distinct
"Manual Checksum" row (Min/Max Tx 1..=260, one byte wider than the ordinary
1..=259 row), whose accompanying note ties that row to a connection made
with the `ISO9141_NO_CHECKSUM` connect flag active. ISO9141's own row
(1..=4128) has no separate Manual Checksum variant in the table — its
bound is already far larger than any checksum byte could affect, so no
widening is needed there.

This service never computes, verifies, or strips a K-line checksum itself
in any mode — `tx_header::kwp_header_bytes`'s own doc comment already
states this for RawMode=OFF ("relies on the vendor DLL computing/verifying
it"). Phase 2 does not change who computes checksums, only whether the
interface is told to touch them at all.

## Decision

**1. Protocol allowlist extended to hardware K-line**
(`rpc_link::rpc_create_com_logical_link`): RawMode=ON is now accepted when
`hw_protocol_id` is exactly `ISO9141`/`ISO14230` (not a `_PS`-qualified
variant — same "raw hardware id, not `base_protocol_id`-normalized"
exclusion Phase 1 already applies to CAN/ISO15765), in addition to Phase
1's base CAN/hardware ISO15765/Analog Inputs/SCI. No `software_isotp`
guard is needed for K-line (no such mode exists for it). Every other
protocol (UART-family protocols other than K-line, J1850, SAE J1708, SAE
J1939, TP2.0) still rejects RawMode=ON `PDU_ERR_ID_NOT_SUPPORTED`.

**2. `resolve_cll_create_flag` now returns and stores ChecksumMode**
(`(raw_mode, checksum_mode): (bool, bool)`, was `raw_mode: bool` alone with
ChecksumMode read-and-discarded). `LogicalLinkState`/`LinkView` gain a
`checksum_mode: bool` field mirroring `raw_mode`'s existing pattern exactly
(resolved once at `CreateComLogicalLink`, fixed for the CLL's lifetime,
meaningful only for a RawMode=ON K-line CLL — inert everywhere else, per
Table D.6's own "ignored" rule).

**3. `connect_flags`'s ISO9141/ISO14230 arm sets `ISO9141_NO_CHECKSUM` iff
`raw_mode && !checksum_mode`**, ORed with the existing `ISO9141_K_LINE_ONLY`
derivation (unchanged, still keyed on `CP_K_L_LineInit`) — the two bits are
independent, both tested. This supersedes the function's prior doc comment
("`ISO9141_NO_CHECKSUM` is deliberately never set"), which ADR-065's Status
line now annotates. `connect_flags` gained two new parameters
(`raw_mode: bool, checksum_mode: bool`), threaded from the connecting
CLL's own `LogicalLinkState` at its one call site
(`rpc_connect_com_logical_link`).

**4. Shared-channel join-compatibility check**, mirroring the SAE J2534-2
clause 24 Ethernet_NDIS pin-option join check's exact shape (`rpc_link.rs`,
`CONNECT_FLAG_NDIS_PINS_OPTION1`/`_OPTION2` masked comparison against
`SharedChannel::connect_flags`): for an ISO9141/ISO14230 join, the joining
CLL's own resolved `connect_flags` masked to `ISO9141_NO_CHECKSUM` must
equal the physical channel's already-recorded value (stamped verbatim at
creation, ADR-044 creator-decides), or the join is rejected
`PDU_ERR_FCT_FAILED`-shaped. A RawMode=OFF CLL's own `ISO9141_NO_CHECKSUM`
bit is always `0` regardless of its `ChecksumMode` value (Table D.6:
ChecksumMode is ignored when RawMode is OFF), so this naturally bars a
RawMode=OFF CLL from ever joining a RawMode=ON/ChecksumMode=OFF channel,
and vice versa.

**5. TX header skip needed NO code change**: `tx_header::build_tx_message`'s
existing `if raw_mode { return Ok(payload.to_vec()); }` early return already
runs before, and instead of, every protocol match arm (not just CAN/
ISO15765's) — it is unconditional and protocol-agnostic. Phase 1's own doc
comment claiming this "is only ever reachable for a base CAN or hardware
ISO15765 link" was describing the ALLOWLIST's scope, not a mechanism
specific to those protocols; extending the allowlist alone makes this
function correctly handle K-line RawMode with no change to the function
itself, beyond updating its doc comment to stop citing the now-stale Phase
1 scope claim.

**6. `compute_tx_prefix` gains a K-line arm** (`rpc_primitive.rs`, plus a
new `cop_data: &[u8]` parameter threaded through all 5 real call sites):
unlike CAN/ISO15765's fixed 4/5-byte prefix, KWP's RawMode header width is
data-dependent (`tx_header::kwp_header_bytes`'s TX composition varies 2-4
bytes by format byte). This arm reuses `events::kwp_header_and_payload_len`
(the identical RX-side parser, changed from private to `pub(super)` for
this call) against the client's own `cop_data`, rather than re-deriving a
second, possibly-diverging notion of "how wide is this frame's header." A
frame that does not parse as well-formed KWP (`kwp_header_and_payload_len`
returns `None`, or its declared header+payload length exceeds the buffer)
falls back to treating the WHOLE buffer as header (`cop_data.len()`) —
mirroring `header_footer_len`'s own degenerate-frame fallback exactly, so
downstream anchors (`request_sid`, `RcHandlingConfig`) land past the
buffer and decline to classify rather than misreading arbitrary bytes as a
SID (ADR-147's "decline rather than risk a false positive" philosophy,
reused not reinvented). Direct unit coverage in `compute_tx_prefix_tests`
covers an unaddressed embedded-length header, a CARB fixed-3-byte header,
an addressed explicit-length-byte header, a malformed/truncated frame, and
an empty buffer.

**7. `resolve_send_recv_tx` gains a `checksum_mode: bool` parameter** and
widens the SAE J2534-1 TX size range by 1 byte
(`kline_manual_checksum_iso14230`) when `raw_mode && !checksum_mode` on an
ISO14230 link specifically — NOT ISO9141, per the Context section's
Figure 42 reading. This is applied as a call-site adjustment on top of
`ChannelProtocol::tx_message_size_range`'s existing return value, rather
than adding a third boolean parameter to that function: `extended_
addressing` and the K-line manual-checksum bit are independent axes that
never combine (ISO14230 has no addressing-mode split at all), so a
dedicated per-protocol special case at the two call sites that need it
(`resolve_send_recv_tx`, the `CoptStartcomm` fast-init size check) is
simpler than growing the shared table function's signature for one
protocol's one row. `tx_message_size_range`'s own doc comment is updated
to describe this division of responsibility.

**8. Fast-init (`CoptStartcomm`'s `WithRequest` path,
`rpc_primitive.rs`) now genuinely exercises RawMode**: its own doc comment
previously asserted `link.raw_mode` was "provably always `false` here"
under Phase 1's K-line-excluding allowlist. That is no longer true; the
comment is corrected, and the fast-init size-range check gets the
identical `+1`-byte ISO14230 manual-checksum widening `resolve_send_recv_tx`
applies, computed the same way (`link.raw_mode && !link.checksum_mode &&
base_protocol == ChannelProtocol::ISO14230`). `build_tx_message`'s own
unconditional RawMode early return (Decision item 5) means no other change
was needed here — the client's fast-init frame passes through unchanged
already.

**9. RX side needed NO code change beyond doc-comment correction**:
`events::header_footer_len`'s KWP arm already calls
`kwp_header_and_payload_len` per frame and is checksum-tolerant by
construction (its footer computation already handles "with or without a
trailing checksum byte," confirmed by reading it, not assumed). Under
RawMode the actual split is already unconditionally skipped
(`entry.raw_mode`-gated, ADR-196 Decision item 3), with `header_footer_len`
still called only to produce a classification-only `raw_prefix` — this
mechanism is protocol-generic already and required no change once the
allowlist admits K-line.

**10. A gap the original plan did not anticipate, found while
implementing: two more internal anchors needed the same `raw_prefix`
re-basing ADR-196 Decision item 3b already gave `RcHandlingConfig`/
`classify_queue_error`/`observe_session_timing_response`.**
`observe_registrant_timing_change`'s own `0xC3`/TPI-echo match guard
(`payload.first()`/`payload.get(1)`) and `observe_timing_response`'s TPI=2
arm (`payload.get(2..7)`, the 5 timing bytes) were both still hardcoded at
absolute positions 0/1/2..7 — the ORIGINAL request-side capture
(`AccessTimingConfig::with_request`) was the only anchor the design-advisor
consult's concrete plan named, but the RESPONSE-side pairing/derivation
logic has its own, separate set of fixed-offset reads that are equally
reachable once a K-line RawMode CLL can receive a genuine `0xC3` response.
Both were unreachable in Phase 1 (K-line RawMode was rejected at create
time) the same way `AccessTimingConfig::with_request` was, and both are
fixed the same way: `observe_timing_response` and
`observe_registrant_timing_change` both gain the `raw_prefix: usize`
parameter `observe_session_timing_response` (the UDS/ISO15765 sibling)
already had, with every fixed offset re-based to `raw_prefix`/
`raw_prefix + N`. `0` for RawMode=OFF leaves both byte-for-byte the
pre-ADR-198 behavior. Direct unit coverage added in
`events_bind_frame_tests.rs` (`observe_timing_response_tpi2_derives_
correctly_at_{zero,nonzero}_raw_prefix`, plus an end-to-end `bind_frame`
test proving a header byte that happens to equal `0xC3` at position 0
cannot spoof the match guard).

**11. `AccessTimingConfig::with_request` gains a `tx_prefix: usize`
parameter** (was previously called with none at all — `TimingChangeConfig::
with_request`'s `KwpAccess` arm did not forward the value it already had),
re-basing `cop_data.first()`/`cop_data.get(1)`/`cop_data.get(2..7)` to
`tx_prefix`/`tx_prefix + 1`/`tx_prefix + 2..tx_prefix + 7`, mirroring
`SessionTimingConfig::with_request`'s existing shape exactly. The real
`tx_prefix` value (already computed generically by `compute_tx_prefix` at
every call site, per ADR-196 Decision item 3b) now reaches both variants.

**12. ChecksumMode's own semantics need no TX/RX mechanism beyond the
connect flag and size-range widening**: this service never synthesizes or
strips a K-line checksum in any RawMode/ChecksumMode combination — the
client's bytes (header only under ChecksumMode=ON, header plus its own
checksum under ChecksumMode=OFF) pass through unchanged on TX
(`build_tx_message`'s unconditional early return, Decision item 5) and a
received frame is delivered whole and unsplit on RX regardless of
ChecksumMode (Decision item 9) — ChecksumMode's only observable effects
are the native connect flag (Decision item 3) and the message-size bound
(Decision item 7).

### CARB/ISO9141-2 residual (ADR-167) verified dormant, not fixed

`events::kwp_header_and_payload_len`'s CARB (`format & 0xC0 == 0x40`) arm
is not self-describing about a trailing checksum byte's presence
(ADR-167's own accepted residual). Under K-line RawMode this stays
dormant, verified rather than merely asserted:

- The RX split is unconditionally skipped for a RawMode CLL (Decision item
  9) — a genuinely-present trailing checksum byte is delivered whole in
  `data_bytes` either way, never actually mis-split off as payload the
  client didn't ask for.
- `raw_prefix` for a CARB frame is fixed at 3 (the parser's own fixed
  3-byte CARB header), unaffected by the checksum-byte ambiguity — the
  ambiguity is specifically about where the PAYLOAD ends, not where the
  HEADER ends.
- A RawMode=OFF CLL (the only mode where the ambiguity could actually
  mis-split a real frame) can never share a physical K-line channel with a
  RawMode=ON/ChecksumMode=OFF one (Decision item 4's join check), so the
  ambiguous ChecksumMode=OFF configuration and the split-sensitive
  RawMode=OFF configuration never coexist on the same channel.

`events.rs`'s own doc comment for `kwp_header_and_payload_len` is updated
to record this dormancy analysis directly (it previously stated the
manual-checksum connect flag "does not" exist in this service, which
ADR-198 makes false); `kline_raw_mode.rs`'s
`carb_addressed_raw_mode_frame_is_unaffected_by_the_checksum_ambiguity_residual`
test exercises a CARB-addressed RawMode CLL end to end (TX and RX) to
confirm this, rather than leaving it as a comment-only claim.

### Alternatives rejected

- **A third boolean parameter on `ChannelProtocol::tx_message_size_range`**
  for the manual-checksum widening (rejected in favor of a call-site
  adjustment, Decision item 7) — that function is a pure SAE J2534-1
  per-protocol constant table with its own lockstep test; growing its
  signature for one protocol's one alternate row, when the two call sites
  that need it already compute the gating condition themselves, adds
  surface area with no reuse benefit.
- **Widening ISO9141's own range too**, for symmetry with ISO14230
  (rejected) — SAE J2534-1's own Figure 42 gives ISO14230 alone a distinct
  Manual Checksum row; ISO9141's existing 1..=4128 bound already
  comfortably fits a manually-included checksum byte, and inventing a
  widened row the spec does not define would be an unfounded
  interpretation, not a conservative extension.
- **Extending RawMode to J1850/J1939/TP2.0 in this same PR** (rejected,
  Phase 3 residual) — J1850 has no native connect-flag analog to
  `ISO9141_NO_CHECKSUM` at all (checked directly against the header), so it
  cannot receive the same two-state treatment without inventing a
  different mechanism entirely; J1939/TP2.0 are unrelated CAN-based
  protocols. Disproportionate blast radius and a genuinely different
  design question, phased separately per this crate's established
  per-feature-area phasing convention.

## Consequences

- **ADR-065** gains a Status annotation: its "`ISO9141_NO_CHECKSUM` is
  deliberately never set" rule is superseded, scoped to a RawMode=ON
  K-line CLL — every other CLL (including every RawMode=OFF K-line CLL)
  keeps ADR-065's original bit-9-stays-0 behavior unchanged.
- **ADR-196** gains a Status annotation: Decision item 1's protocol
  allowlist is extended to hardware K-line, and Decision item 3b's claim
  that `AccessTimingConfig::with_request` is unreachable/needs no
  `tx_prefix` under RawMode is superseded — exactly as that Decision
  item's own "revisit only if Phase 2 ever extends RawMode to K-line" note
  anticipated. Every other ADR-196 Decision item is unaffected.
- **Documented Phase 3 residual, closed by ADR-200**: RawMode for
  J1850/J1939/TP2.0 remained unimplemented and unscoped after this ADR —
  ADR-200 resolves all three: J1850 and J1939 are now RawMode-admitted
  (J1850 confirmed to need no `ISO9141_NO_CHECKSUM`-style mechanism at all,
  only a new ChecksumMode=ON requirement at create time; J1939 via a new
  protocol-specific destination-address shim, since its D-PDU raw wire shape
  and native J2534-2 wire shape genuinely differ), and TP2.0 is resolved as
  a permanent exclusion with documented rationale, not a residual.
- **A pre-existing residual split out during this ADR's backlog sweep,
  widened by this ADR (Codex review, PR #108 round 1 — this ADR's own
  first draft incorrectly claimed it was "unrelated to K-line")**: SAE
  J2534-2 clause 14 Repeat Messaging still unconditionally rejects
  `PDU_ERR_ID_NOT_SUPPORTED` on any RawMode=ON CAN/ISO15765/ISO9141/
  ISO14230 CLL (`rpc_misc.rs::ioctl_start_repeat_message` — the
  CAN/ISO15765 rejection is ADR-196 Phase 1's own finding; this ADR
  widens the SAME rejection to K-line, since Decision item 1's allowlist
  extension makes the identical double-header risk reachable there too)
  rather than composing a correct frame — this function's message
  composition has no RawMode-aware path of its own, for any protocol. Not
  given a real fix by this ADR (only the existing rejection's protocol
  list is widened); now tracked as its own self-contained backlog bullet
  rather than folded into the general RawMode/ChecksumMode entry this
  ADR's own backlog cleanup removed.
- `docs/rpc-api-guide.md`'s `CllCreateFlag` bullet list and
  `j2534-0404-service/docs/comparam-protocol-support.md`'s RawMode-bypass/
  `CP_RCByteOffset` notes are updated to describe the now-implemented Phase
  2 K-line behavior and the Phase 3 residual.
- `cll_tag` was removed outright by ADR-204 (gRPC event-correlation tags:
  `cop_tag` added, `cll_tag` deleted), which is now the durable record for
  it — no backlog entry tracks it any longer.

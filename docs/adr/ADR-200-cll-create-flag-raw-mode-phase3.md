# ADR-200: `CllCreateFlag` RawMode Support, Phase 3 (SAE J1850/J1939, TP2.0 Closed Out)

**Date:** 2026-08-29
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_create_com_logical_link`'s
             protocol allowlist), `j2534-0404-service/src/service/tx_header.rs`
             (`build_tx_message`, new `raw_j1939_tx_message`), `j2534-0404-service/src/service/events.rs`
             (`CllRxEntry::raw_mode` doc comment, `poll_rx_inner`'s delivery loop, new
             `raw_j1939_rx_drop_destination_address`), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`compute_tx_prefix`), `j2534-0404-service/src/service/rpc_misc.rs`
             (`ioctl_start_repeat_message`'s RawMode `mask_data`/`pattern_data`
             composition), `j2534-0404-service/tests/grpc_mock/raw_mode.rs`,
             `j2534-0404-service/tests/grpc_mock/j1850_raw_mode.rs` (new),
             `j2534-0404-service/tests/grpc_mock/j1939_raw_mode.rs` (new),
             `docs/rpc-api-guide.md`, `j2534-0404-service/docs/implementation-notes.md`,
             `j2534-0404-service/docs/comparam-protocol-support.md`,
             `docs/adr/INDEX.md`, `docs/adr/ADR-196-cll-create-flag-raw-mode-checksum-mode-phase1.md`
             (Status line), `docs/adr/ADR-198-cll-create-flag-raw-mode-checksum-mode-phase2.md`
             (Status line, Consequences)

## Context

ADR-196 (Phase 1: base CAN/hardware ISO15765) and ADR-198 (Phase 2: hardware
K-line/ISO9141/ISO14230) implemented `CllCreateFlag` RawMode/ChecksumMode
(ISO 22900-2:2022 Annex D.2.3, Table D.6) for four protocols, and ADR-199
extended RawMode support to SAE J2534-2 clause 14 Repeat Messaging for those
same four. Both ADR-196 and ADR-198 left SAE J1850, SAE J1939, and TP2.0 as
an unscoped Phase 3 residual (`j2534-0404-service/docs/implementation-notes.md`'s
backlog) — J1850 has no native CRC-suppression connect flag in v04.04 to give
it the same clean two-state treatment K-line's `ISO9141_NO_CHECKSUM` bit
gets; J1939/TP2.0 are CAN-based and structurally unrelated to K-line's
mechanism.

A `design-advisor` consult (prior to this implementation, independently
verified against the actual spec text before this work began) produced a
concrete, per-protocol decision for all three, resolving the residual
definitively rather than deferring it again.

**J1850 (VPW/PWM).** SAE J2534-1 v04.04 §7.2.3.3 Figure 7 (the
`PassThruConnect` `Flags` parameter table) has exactly one checksum-control
bit, `ISO9141_NO_CHECKSUM`, scoped explicitly to ISO9141/ISO14230 — no
analogous bit exists anywhere in that table for J1850. This service's own
`tx_header.rs` (`j1850_header_bytes`'s doc comment) and `events.rs`
(`header_footer_len`'s J1850 arms) already establish that the interface
computes/verifies/strips J1850's CRC unconditionally, with no service
involvement, in either RawMode or not — confirmed by reading both, not
assumed. J1850's IFR (In-Frame Response, PWM only) bytes are legitimately
part of the raw frame under RawMode, not a checksum concern.

**SAE J1939.** ISO 22900-2:2022 line 774/§10.1.4.19.5/Table 80 define the
D-PDU RawMode wire contract for J1939 as 4 bytes (the 29-bit CAN identifier
only) plus payload — the identical shape CAN/ISO15765 already use. SAE
J2534-2 §16.4.3/Table 62 defines the NATIVE J2534 wire format for J1939 as 5
bytes: the same 4-byte CAN ID plus a 5th byte, the destination address (DA),
required on every native `PassThruWriteMsgs`/`PassThruReadMsgs` call on a
J1939 channel (SAE J1939-21's BAM-vs-RTS/CTS multi-packet transport
selection is keyed on this byte). These two contracts genuinely differ for
this one protocol — every other RawMode-admitted protocol's D-PDU raw shape
and native J2534 wire shape coincide.

**TP2.0.** ISO 22900-2:2022 has no TP2.0 protocol entry at all — no Table 80
row, no RawMode wire-shape definition anywhere. Architecturally, TP2.0's
non-broadcast header carries a live, service-negotiated TX-ID this service's
own dispatch logic rewrites at every send (`events.rs`, ADR-188's
established-connection-ID handling); a client-owned raw header would either
be silently overwritten (defeating RawMode's own "client controls the wire
bytes" premise) or go stale, and the client has no D-PDU-visible way to
learn the negotiated ID to supply it correctly in the first place. TP2.0's
broadcast path (clause 19.3.2.2) offers no clean partial admission either:
its own `TP2_0_BROADCAST_MSG` TxFlag is itself derived from a ComParam
(`tp20_broadcast_address()`) — a "raw" CLL that still needs a
ComParam-driven flag to send correctly is not actually raw.

## Decision

**1. Protocol allowlist** (`rpc_link::rpc_create_com_logical_link`) admits
`J1850VPW`/`J1850PWM` and `PROTOCOL_J1939_PS` for RawMode=ON, alongside
Phase 1/2's existing CAN/ISO15765/K-line/Analog-Inputs-SCI set. TP2.0
(`PROTOCOL_TP2_0_PS`) and every other still-excluded protocol continue to
reject `PDU_ERR_ID_NOT_SUPPORTED` exactly as before — TP2.0's rejection text
now states the permanent-exclusion rationale from the Context section above,
replacing the earlier "not yet implemented" framing.

**2. J1850: admitted via the EXISTING generic RawMode passthrough, no
mechanism change, plus a new create-time constraint.**
`tx_header::build_tx_message`'s RawMode early return
(`if raw_mode { return Ok(payload.to_vec()); }` before ADR-200, now branched
by protocol for J1939 only — see Decision item 3) already ran before every
protocol match arm, including J1850's, so its TX side needed no code change.
`events::header_footer_len`'s existing J1850 arms (`data.len().min(3)`, no
footer for VPW, `ExtraDataIndex`-derived IFR footer for PWM) and the
existing RawMode RX split-skip (ADR-196 Decision item 3, `(0, 0)`
unconditionally) already produce correct RX behavior once the allowlist
admits J1850.

**RawMode=ON on a J1850 CLL now additionally REQUIRES ChecksumMode=ON**,
checked at `CreateComLogicalLink` (`rpc_link.rs`), rejected with a distinct
message from the general protocol-allowlist rejection: ChecksumMode=ON's
Table D.6 semantics (interface still appends/validates/strips the checksum)
is exactly what the interface already unconditionally does for J1850 in
v04.04 — honest and achievable. ChecksumMode=OFF's semantics (client owns
the checksum entirely) is NOT achievable for J1850 in v04.04 — there is no
connect flag to make the device stop touching J1850's CRC. Silently
downgrading a ChecksumMode=OFF request to ON's actual behavior would
recreate exactly the "flag silently ignored" defect ADR-196 exists to close,
so RawMode=ON + ChecksumMode=OFF is rejected outright on a J1850 CLL,
`PDU_ERR_ID_NOT_SUPPORTED`-shaped, distinguishable from the general
"protocol doesn't support RawMode at all" message by naming ChecksumMode
specifically.

One correction from this brief's own plan, found while implementing:
`rpc_primitive::compute_tx_prefix` (the shared helper that re-bases internal
`0x7F`/SID-echo anchors — `request_sid`, `RcHandlingConfig` — by the
per-frame raw prefix width, ADR-196 Decision item 3b/ADR-198 Decision item
6) had no J1850 arm and would have fallen through to the CAN family's fixed
4-byte default. J1850's native header is a fixed 3 bytes
(`tx_header::j1850_header_bytes`), not 4 — left uncorrected, this would have
desynced `request_sid`'s SID-echo anchor by one byte for every RawMode
J1850 CLL, exactly the class of bug ADR-196 Decision item 3b/ADR-198
Decision item 6 exist to prevent for other protocols. Fixed by adding a
dedicated J1850 arm (`cop_data.len().min(3)`, mirroring
`header_footer_len`'s own degenerate-frame handling); direct unit coverage
added alongside the pre-existing K-line arms' own tests
(`compute_tx_prefix_tests`).

**3. SAE J1939: admitted via a NEW protocol-specific shim, not the generic
passthrough**, since the D-PDU raw wire shape and the native J2534-2 wire
shape genuinely differ for this one protocol (Context section above).

- **TX** (`tx_header::build_tx_message`'s RawMode branch, now protocol-aware
  instead of an unconditional early return): a new `raw_j1939_tx_message`
  function derives the native destination-address (DA) byte mechanically
  from the client's own raw CAN-ID bytes alone — no ComParam is consulted,
  preserving RawMode's "client owns addressing" premise. Given the client's
  `cop_data` (>= 4 bytes: the same byte layout `tx_header::j1939_header_bytes`
  already composes — byte 0 priority/data-page, byte 1 PDU Format (PF), byte
  2 PDU Specific (PS), byte 3 source address — confirmed against that
  function's own established convention rather than re-derived from
  scratch): PF < 240 (PDU1, peer-to-peer) sets DA = PS (byte 2), the SAME
  value `j1939_header_bytes`'s own PDU1 arm uses; PF >= 240 (PDU2,
  group-broadcast) sets DA = `0xFF` (BAM/broadcast segmentation, clause
  16.4.4). The DA byte is inserted at wire position 4, producing the native
  5-byte-header J2534-2 shape. A `cop_data` shorter than 4 bytes (no CAN-ID
  prefix to derive a DA from) is rejected with a clear error, not a
  panicking out-of-bounds slice.
- **RX** (`events.rs`'s RawMode delivery loop, `poll_rx_inner`): a new
  `raw_j1939_rx_drop_destination_address` function performs the inverse —
  an interior byte removal (5 bytes -> 4 bytes: CAN ID + payload), NOT a
  prefix/suffix split like `header_footer_len` produces for every other
  protocol, so it needed its own small transform rather than reusing that
  mechanism. This runs BEFORE `payload`/`raw_prefix` computation and before
  expected-response/RC-byte-offset matching (which run against
  `ResultData.data_bytes`, ADR-051), so the DA byte never reaches any
  downstream consumer. A received frame shorter than 5 bytes (malformed or
  untrusted FFI input, crossing the boundary from a vendor DLL or the mock)
  is returned unchanged rather than panicking — mirroring
  `header_footer_len`'s own "treat as unsplittable rather than risk an
  out-of-bounds slice" fallback philosophy. `raw_prefix` for a RawMode
  J1939 CLL is derived directly from the DA-dropped buffer's own length
  (capped at 4), not from `header_footer_len`'s `PROTOCOL_J1939_PS` arm
  (which still assumes the native 5-byte shape this transform has already
  removed, and stays unchanged for RawMode=OFF).
- **Repeat Messaging** (`rpc_misc.rs::ioctl_start_repeat_message`, ADR-199's
  RawMode composition): `repeat_msg_data`'s composition already calls
  `build_tx_message` with the CLL's real `raw_mode` value, so the TX shim
  above is inherited automatically with no separate call site needing the
  DA-derivation logic duplicated. `mask_data`/`pattern_data` (composed
  separately, unprefixed under RawMode per ADR-199) are compared by the
  DEVICE against native 5-byte-DA-included frames, so a J1939 RawMode
  client's raw 4-byte template gets a zeroed (`0x00`, don't-care) byte
  inserted at position 4 in both, mirroring clause 14.2.2.1's own
  don't-care-beyond-`DataSize` mask mechanism (the same technique
  `docs/rpc-api-guide.md` already documents for K-line's ChecksumMode=OFF
  trailing-checksum-byte case, ADR-199) — a DERIVED DA is deliberately NOT
  used here, unlike the TX side: the device's own incoming DA on a
  RECEIVED frame is not predictable the way a transmitted DA is.
- **ChecksumMode is inert (accepted-but-no-op) for J1939**, mirroring how
  Phase 1 left it for CAN/ISO15765: J1939 is CAN-based with no
  message-level (application-layer) checksum concept — only the CAN
  controller's own hardware CRC applies, untouched by any ComParam or D-PDU
  mechanism.

**Correction (`edge-case-hunter` finding, this PR's own close-out pass): the
Repeat Messaging `mask_data`/`pattern_data` template above never carried
`TX_EXTENDED_ID`, unconditionally misclassifying every genuine match.**
J1939 is unconditionally 29-bit (clause 16.4.3) — unlike CAN/ISO15765,
there is no legitimate 11-bit J1939 frame. But `response_tx_flags` under
RawMode is set entirely from `raw_mode_addressing_flags` (the client's own
`tx_flag_bits` fold, gated to CAN/ISO15765 only per that fold's own
comment — J1939 is a distinct `base_protocol_id`), so the response template
NEVER carried `TX_EXTENDED_ID`, regardless of what the client requested.
The device-side matcher (`RepeatSlot::response_format_rx_bits`,
`j2534-0404-mock`) keys a match on this bit agreeing with each incoming
frame's own `CAN_29BIT_ID_STATUS` — the identical mechanism ADR-199's own
close-out review found broken for CAN/ISO15765's asymmetric-addressing
residual — so every genuinely-matching, honestly-29-bit-flagged real J1939
response was rejected as a non-match on every RawMode J1939 repeat slot,
unconditionally. Unlike ADR-199's own CAN/ISO15765 residual (a narrow,
client-avoidable asymmetric-TX/RX-width corner case), this had no
"narrow" framing available: J1939 has no 11-bit form to be asymmetric
with, so the gap fired on every genuine response, not a corner case.
`rpc_misc.rs`'s existing unconditional `tx_flags |=
j2534_0404::TX_EXTENDED_ID` force for J1939's TRANSMITTED message (a
pre-existing fix, PR #72 round 11, whose own comment incorrectly asserted
the response side "was unaffected" — true only for the non-RawMode
`response_header_bytes` path this ADR's RawMode branch bypasses) is now
mirrored onto `response_tx_flags`, forced unconditionally regardless of
the client's own `tx_flag_bits` — RawMode's client-authoritative-addressing
premise does not extend to a bit no J1939 frame can ever legitimately
clear. The new J1939 Repeat Messaging match-proof test
(`j1939_raw_mode.rs`) was corrected to inject its proof frame via
`inject_rx_with_status(..., CAN_29BIT_ID_STATUS)` rather than plain
`inject_rx` (`RxStatus = 0`) — the original draft's use of the always-`0`
default coincidentally "matched" even with the bug present, masking it the
same way `repeat_message.rs`'s own
`condition_one_slot_terminates_immediately_on_a_frame_with_the_wrong_response_format`
doc comment warns a byte-content-only proof can. Direct unit coverage for
`events::raw_j1939_rx_drop_destination_address`'s own short-buffer/
boundary cases was also added in the same pass (it previously had none,
unlike its TX-side sibling `raw_j1939_tx_message`).

**4. J1939 claim-machinery interaction: confirmed unaffected, no new gating
needed.** `rpc_primitive.rs`'s existing `j1939_tx_source`/`tester_addr`
resolution (the source-address byte used by the NON-RawMode
`j1939_header_bytes` composition, and independently by `events.rs`'s
transmit-time claim-drift check, ADR-180 Decisions 14/18) is orthogonal to
the RawMode DA shim — the shim derives DA purely from the client's own
CAN-ID bytes and never touches the claimed-address ComParam, while the
claim-drift check compares the LIVE claimed address against
`NODE_ADDRESS` (which the claim loop itself writes back on success) with no
dependency on what bytes the client's raw payload actually carries. The
existing claim-state gates (the per-cycle transmit gate checking
Engaged/claimed status, ADR-180 Decisions 14/18; `CoptSendrecv`'s
`comm_started` precondition; Repeat Messaging's own claimed-address gate,
ADR-180 Decision 13) are all COP-level and RawMode-independent — they keep
running exactly as before. The client's own raw CAN-ID source-address byte
is trusted/unvalidated under RawMode, mirroring how CAN/ISO15765 RawMode
(ADR-196) already trusts the client's own CAN-ID bytes entirely.

**5. TP2.0 is now a documented, PERMANENT RawMode exclusion, not a
residual.** The rejection message and `rpc_link.rs`'s own doc comment are
updated to state the Context section's rationale (no ISO 22900-2:2022
RawMode wire shape exists for TP2.0 at all; this service's own TX-ID
rewriting at every send is fundamentally incompatible with a client-owned
raw header, broadcast included) rather than the earlier "deferred pending
its own design pass" framing. `raw_mode.rs`'s former J1850 rejection test
(J1850 is now allowlisted) is repurposed as the TP2.0 permanent-exclusion
test — proven against a real opted-in-module, correctly-pinned TP2.0
resource, so the rejection is shown to come from the RawMode allowlist
check itself, not an earlier pin/opt-in failure.

### Alternatives rejected

- **Emulating J1850 ChecksumMode=OFF by having this service compute/strip
  its own CRC on top of the interface's own management.** Would either
  double-process the checksum or misrepresent to the client which layer
  actually manages it; the interface's own unconditional CRC handling in
  v04.04 cannot be disabled, so there is nothing for a service-side
  emulation to layer correctly onto.
- **Deriving SAE J1939's native DA byte from a ComParam
  (`CP_J1939TargetAddress`) instead of the client's own raw CAN-ID bytes.**
  Rejected the same way ADR-199's own alternatives-rejected section rejects
  splitting RawMode addressing authority between a client TxFlags channel
  and a ComParam channel — it would contradict RawMode's own "client is the
  sole addressing authority" contract and silently reintroduce a
  ComParam-configuration dependency RawMode exists to remove.
- **A generic prefix/suffix `header_footer_len`-style split for J1939's
  RX-side DA removal**, instead of the dedicated
  `raw_j1939_rx_drop_destination_address` transform. The DA byte sits in
  the MIDDLE of the native frame (after the 4-byte CAN ID, before the
  payload), not at a leading/trailing edge — the generic split mechanism
  has no way to express an interior removal, so a dedicated small function
  was the correct-shaped tool, not a forced generalization of the existing
  one.
- **Extending RawMode to TP2.0 with a synthesized/frozen TX-ID.** Considered
  and rejected: freezing the TX-ID at CLL-create time would desync from
  whatever connection this service's own dispatch logic actually
  establishes at `CoptStartcomm` time, and there is still no D-PDU-visible
  mechanism for the client to learn or influence that value — the same
  "client controls the wire bytes" premise violation the Context section
  already describes, just deferred rather than avoided.

## Consequences

- **ADR-196**/**ADR-198** each gain a Status-line annotation: Decision item
  1's protocol allowlist (ADR-196) is further extended by this ADR to J1850
  and J1939; ADR-198's own Phase 3 residual bullet is closed by this ADR.
- **Accepted residual, vendor-hardware-only, not testable in this repo**:
  what CAN ID a reassembled multi-frame SAE J1939 RX (a real ECU's
  BAM/RTS-CTS multi-packet response, clause 16.4.5) reports on a RawMode
  CLL is unspecified by that clause and is passed through untouched by this
  service — this service performs no J1939 transport-layer reassembly of
  its own (the native adapter/vendor DLL does, opaquely, the same way it
  already does for classic ISO15765 multi-frame responses this service
  also does not reassemble). `j2534-0404-mock` itself implements no J1939
  multi-frame reassembly logic at all (confirmed by direct code reading —
  it exposes only a raw `inject_rx` backdoor, delivering exactly the bytes
  a test supplies), so there is no reassembly-specific CAN-ID convention
  for this repo to pin beyond what `j1939_raw_mode.rs`'s own RX test
  already fixes: a single injected frame's CAN-ID bytes pass through the
  DA-drop transform unchanged, byte for byte. A real vendor DLL's
  reassembled-response CAN ID (e.g. whether it reports the first segment's
  ID, the last, or some fixed convention) is genuinely unverifiable without
  real J1939 hardware and is not assumed here.
- `docs/rpc-api-guide.md`'s `CllCreateFlag` bullet list is updated to
  describe J1850's ChecksumMode=ON requirement and J1939's client-visible
  4-byte/service-derived-DA shape, and to restate TP2.0's exclusion as
  permanent.
- `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog:
  the Phase 3 residual entry (naming J1850/J1939/TP2.0 as unscoped) this ADR
  resolves is removed outright; the vendor-DLL reassembled-RX CAN-ID
  residual above is recorded as its own new, self-contained backlog bullet
  citing this ADR.
- `cll_tag` was removed outright by ADR-204 (gRPC event-correlation tags:
  `cop_tag` added, `cll_tag` deleted), which is now the durable record for
  it — no backlog entry tracks it any longer.

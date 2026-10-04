# ADR-151: `CP_StartMsgIndEnable`/`CP_TransmitIndEnable` Gate SOM/TxDone Indication Delivery

**Date:** 2026-07-30
**Status:** Accepted (amends ADR-098, ADR-100)
**Affects:** `j2534-0404-service/src/service.rs` (`ComParamSet` accessors), `j2534-0404-service/src/service/events.rs` (`CllRxEntry`, `build_cll_rx_entries`, `poll_rx_inner`), `j2534-0404-service/docs/comparam-protocol-support.md`, `j2534-0404-service/docs/implementation-notes.md`, `docs/j2534-0404-architecture.md`, `docs/glossary.md`

## Context

ISO 22900-2:2022 Annex B.4.1/B.5.1 defines `CP_StartMsgIndEnable` and
`CP_TransmitIndEnable` as opt-in, per-logical-link gates on two J2534
`RxStatus` indication types this service already forwards: an RxStart herald
(first frame of a multi-frame ISO 15765 message, or first byte of a UART
message) and a TxDone/transmit-completion notice. Both ComParams default to
disabled (0) for every protocol; when disabled, the D-PDU server must not
generate the corresponding indication `ResultData` item at all.

Before this ADR both ComParams were fully inert plumbing:
`PARAM_START_MSG_IND_ENABLE`/`PARAM_TRANSMIT_IND_ENABLE`
(`service_params.rs:165,168`) existed as registered names, defaults, and
allow-listed validation entries, but nothing read their value.
Independently, the underlying `RX_START_OF_MESSAGE`/`RX_TX_INDICATION`
`RxStatus` bits were (and remain) forwarded into `rx_status_flags`
unconditionally per ADR-098, and any frame carrying one of the four
indication-type bits is unconditionally delivered to the client via
`poll_rx_inner`'s `FrameBinding::Unbound` fallthrough per ADR-100 Decision §3
step 1 ("indication frames bypass binding entirely ... keep their existing
`ResultData` delivery path"). Net effect: this service always delivered SOM
and TxDone indications to every client, on every CLL — the opposite of the
spec's disabled-by-default behavior, since nothing ever actually withheld
them.

J2534 v04.04 (SAE J2534-1 §8.7.2, Valid RxStatus Bit Combinations) has no
IOCTL that suppresses these bits at the hardware layer — a PassThru device
that supports them emits them unconditionally. Client-visibility of the
indication is therefore necessarily an adapter-level policy this service
must apply itself, per CLL, without disturbing the frame-global hardware
classification (`rx_status_flags`/`is_content_frame`/`is_tx_side`) that
ADR-098/ADR-099/ADR-100's whole attribution precedence chain depends on —
that classification recognizes what a frame *is* on the wire and must stay
correct regardless of what any one CLL's client has opted into seeing.

## Decision

**1. Two new per-CLL `ComParamSet` accessors**, `service.rs`, following the
existing `suspend_queue_on_error()` pattern: `start_msg_ind_enable(&self) ->
bool` and `transmit_ind_enable(&self) -> bool`, both
`self.unum32.get(&PARAM_X).copied().unwrap_or(0) != 0`.

**2. `CllRxEntry` gains two `bool` fields**, `start_msg_ind_enable`/
`transmit_ind_enable`, stamped once per poll pass in `build_cll_rx_entries`
from that entry's own `l.active.*` accessor — the same per-pass-snapshot
granularity every other Active-derived `CllRxEntry` field already uses (e.g.
the software-ISO-TP `block_size`/`st_min`/`framing`/`n_cr` fields). A change
takes effect on the CLL's next poll pass, not mid-pass; this matches the
existing snapshot semantics for every sibling field and avoids adding lock
traffic to the per-frame hot loop. Populated for every entry kind, including
`Companion`.

**3. Gating happens at the per-entry delivery decision, after `bind_frame`
has already run — not by altering `rx_status_flags`/`is_content_frame`
computation.** Those remain frame-global hardware truth, computed once
before the per-CLL-entry loop, and stay untouched: `bind_frame`'s whole
attribution chain (tier-1/tier-2 registrant scans, ADR-099's tester-present
SOM-herald and TX-echo discard, `queue_error_class` folding) runs exactly as
before, on every frame, regardless of these two ComParams' values. This
preserves ADR-099's discard precedence: a tester-present response's SOM
herald is invisible to the client whether or not `CP_StartMsgIndEnable`
happens to be 1, and with it 0 the two mechanisms simply agree on the same
outcome by different paths.

The suppression check is inserted as a new arm in `poll_rx_inner`'s
`FrameBinding` match, between the existing `Unbound if is_content_frame =>
continue` and the final `Unbound => (0, None, false, None, false)`
fallthrough (`events.rs:5564-5565`):

```rust
FrameBinding::Unbound if indication_suppressed(entry, rx_status_flags) => continue,
```

`indication_suppressed` applies **per-frame precedence, `RX_BREAK`-first,
then TX_INDICATION-dominant**:

- `rx_status_flags & RX_BREAK != 0` → never suppressed, regardless of any
  other bit also set. `RX_BREAK` is not a governed indication type under
  either ComParam and always wins, mirroring the same-file precedent at
  `bind_frame`'s tester-present signature check, which unconditionally
  excludes `RX_BREAK` from its own discard the same way (`events.rs:4509`,
  "RX_BREAK is never a tester-present artifact ... even combined with
  another bit"). (Edge-case-hunter review finding: an earlier version of
  this rule checked `RX_TX_INDICATION`/`RX_START_OF_MESSAGE` first, which
  meant an off-spec `SOM | RX_BREAK` frame fell through to the
  `start_msg_ind_enable` branch and had its RX_BREAK signal silently
  withheld whenever that ComParam was disabled — the spec default. Fixed by
  checking `RX_BREAK` first.)
- else `rx_status_flags & RX_TX_INDICATION != 0` → governed by
  `transmit_ind_enable`. The accompanying `RX_TX_MSG_TYPE` bit (SAE J2534-1
  §8.7.2's TxDone row always sets both) is not treated as an independent
  loopback-delivery justification — it denotes "this is TX-side," mirroring
  this module's own existing `is_tx_side` bit grouping (`events.rs:5330`),
  not a second reason to deliver.
- else `rx_status_flags & RX_TX_MSG_TYPE != 0` (and `RX_TX_INDICATION`
  absent, by the arm above) → never suppressed. This is a CONFIG_LOOPBACK
  echo, which ADR-098 explicitly decided is never filtered from delivery —
  including the documented `RX_TX_MSG_TYPE | RX_START_OF_MESSAGE`
  combination (a loopback echo of our own SOM-tagged transmit, ADR-098's
  Context). This arm must be checked BEFORE the `RX_START_OF_MESSAGE` arm
  below. (Codex review finding on this PR: an earlier version of this rule
  checked `RX_START_OF_MESSAGE` first, so this exact combination fell
  through to the `start_msg_ind_enable` branch and silently dropped a valid
  loopback echo whenever that ComParam was at its spec default of
  disabled. Fixed by checking bare `RX_TX_MSG_TYPE` before
  `RX_START_OF_MESSAGE`.)
- else `rx_status_flags & RX_START_OF_MESSAGE != 0` → governed by
  `start_msg_ind_enable`.
- else → never suppressed by these two ComParams.

This is a whole-frame decision, not a per-bit mask edit: every reachable
delivery site for these two bits (the `Unbound` fallthrough) only ever
carries a pure, non-content indication frame — `is_content_frame == false`
is what routed it there in the first place, and no code path constructs a
delivered frame mixing a real payload with a gated indication bit. `rx_flag`
byte construction (`rx_flag_bytes`, ADR-098), `start_msg_timestamp`/
`tx_msg_done_timestamp` population, and the `GetEventItem` pull path all stay
unmodified: suppression means "never enqueue this indication item" for a
disabled CLL, not "zero a bit out of an item that ships anyway" — since
nothing ships when the whole frame is suppressed.

**4. Amends ADR-098** ("`rx_status_flags` forwards unconditionally") to add:
that statement describes the internal `rx_status_flags`/`RxFlag`-byte
computation only; it no longer implies unconditional *delivery* of a pure
SOM/TxDone indication item — delivery is additionally gated per CLL by this
ADR. **Amends ADR-100** Decision §3 step 1 ("indication frames ... keep
their existing `ResultData` delivery path") the same way: the path is kept,
but is now conditional on `start_msg_ind_enable`/`transmit_ind_enable` for
the SOM/TxDone subset specifically — RxBreak and loopback are unaffected and
still deliver unconditionally as before.

## Consequences

- Corrects a real conformance gap: this service previously always delivered
  SOM/TxDone indications regardless of ComParam value, which is backwards
  from the spec's disabled-by-default behavior. A client that has not
  explicitly set `CP_StartMsgIndEnable`/`CP_TransmitIndEnable = 1` will now
  stop receiving these indications — a client relying on the old always-on
  behavior must opt in explicitly going forward.
- Accepted residual: `RxEntryKind::SoftwareIsoTp` links (ADR-046) never
  produce SOM/TxDone indications even when enabled — raw-CAN hardware never
  emits these bits for a software-reassembled/segmented link, and
  `process_frame_for_entry`'s deliveries inherit the outer frame's
  `rx_status_flags` verbatim. Synthesizing these indications for the
  software path is out of scope here; tracked as a backlog item in
  `j2534-0404-service/docs/implementation-notes.md`.
- Accepted residual: `RX_BREAK` has no governing ComParam in this ADR's
  scope and continues to deliver unconditionally, unchanged from ADR-098 —
  including when combined with `RX_START_OF_MESSAGE`/`RX_TX_INDICATION`,
  per the `RX_BREAK`-first precedence rule above.
- Accepted residual: an off-spec frame combining `RX_START_OF_MESSAGE` with
  `RX_TX_INDICATION` (SAE J2534-1 §8.7.2's table does not enumerate such a
  combination) resolves deterministically via the TX_INDICATION-dominant
  precedence rule above, not by inspecting every bit independently —
  documented, not treated as a hardware-conformance violation to reject.
- The synthetic fast-init response frame in `handle_start_comm`
  (`rx_status_flags: 0`, unconditionally) is unaffected — it never carries
  either gated bit.
- `j2534-0404-service/docs/comparam-protocol-support.md`'s existing "S"
  (Supported) marking for both ComParams across all protocol columns becomes
  accurate as of this ADR, rather than aspirational.

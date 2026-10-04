# ADR-055: Reject Functionally Addressed ISO15765 Requests That Don't Fit a Single Frame

**Date:** 2026-07-04
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/tx_header.rs`,
             `j2534-0404-service/src/service/rpc_primitive.rs`

## Context

ADR-054 wired `CP_RequestAddrMode` into `tx_header::build_tx_message` so a
value of `2` (functional/broadcast) builds the outgoing CAN ID from
`CP_CanFuncReqId`/`Format`/`ExtAddr` instead of the per-ECU
`UniqueRespIdTable`. That work only changed *which* CAN ID is used to build
the header — it left the size/framing decision untouched.

ISO 15765-2 requires a functionally addressed (broadcast) request to fit in
a single frame: with no specific target ECU, there is no way to negotiate
FlowControl (block size, separation time) for a multi-frame exchange, since
any number of ECUs might attempt to answer. Nothing in this service enforced
that constraint. In software-ISO-TP mode (`can_channel_mode =
"software-isotp"`), `events.rs::isotp_send` decides SingleFrame vs.
FirstFrame+ConsecutiveFrame purely by payload length
(`payload.len() <= tx_addressing.max_sf_payload()`), with no awareness of
`CP_RequestAddrMode`. A functional request whose payload didn't fit in one
frame would silently be segmented as multi-frame and would wait (up to the
configured `N_Bs` timeout) for a FlowControl frame from *any* CAN ID —
`resolve_can_addressing`'s functional branch sets `fc_can_id: None`, and
`FcCapture` treats that as "accept a FlowControl frame from any CAN ID."
This is not a theoretical gap: this service's own `iso15765_4_common` default
ComParam preset (used by the standard OBD-II-on-ISO15765-4 presets) sets
`CP_RequestAddrMode = 0x02` (functional) by default.

The hardware (non-software-ISO-TP) ISO15765 channel path has no equivalent
guard either — this service hands the full constructed message straight to
`PassThruWriteMsgs` and the vendor DLL decides SF/MF framing on its own,
with no visibility into whether the request is functionally addressed.

(Separately: a lone Single Frame exchange does not, by itself, require a
hardware `FLOW_CONTROL_FILTER` to be configured for *sending* — the
per-protocol addressing requirement `tx_header.rs` already enforces
(`CP_CanPhysReqId`/`CP_CanFuncReqId`) is a header-construction requirement,
not a flow-control one, and is unrelated to whether a filter object exists
on the channel. Filter installation for RX is a separate, already-decoupled
concern — ADR-039/040/041/048 already extend point-to-point
`FLOW_CONTROL_FILTER` coverage to Single-Frame-only UUDT traffic for exactly
this reason, and `ConnectComLogicalLink`'s pass-all fallback
(`sync_channel_fc_pass_all_filter`) already covers the case where no
per-ECU filter applies. No change was needed on that side.)

## Decision

`CoptSendrecv` now rejects, before the primitive is queued, a functionally
addressed ISO15765 request whose `cop_data` exceeds the addressing's Single
Frame payload capacity (`isotp::Addressing::max_sf_payload()` — 7 bytes
under Normal addressing, 6 under Extended) with `Status::invalid_argument`.
This mirrors ADR-049's existing "reject, never silently adjust" pattern and
runs right next to that check in `rpc_primitive.rs`, using the same already
computed `tx_addressing`.

To know whether the resolved addressing is functional without re-deriving
it from `CP_RequestAddrMode`, `tx_header::CanAddressing` gained a
`functional: bool` field, set by `resolve_can_addressing`'s two branches.

The check applies uniformly to both the hardware ISO15765 channel path and
software-ISO-TP mode — it runs in `rpc_primitive.rs` before either path
sees the message, so the vendor DLL never receives an oversized functional
request to (mis)handle on its own, and software-ISO-TP never reaches the
FlowControl-wait branch under functional addressing at all.

Raw `CAN` protocol is not affected: its own SAE J2534-1 TX size range
(`4..=12`, ADR-049) already caps every message at 8 payload bytes, so it can
never exceed a Single Frame's worth of data regardless of addressing mode —
there is no ISO-TP segmentation concept for raw CAN to begin with.

## Consequences

- A client that sends a functionally addressed request larger than one
  Single Frame now gets an immediate, specific `invalid_argument` error
  instead of the service either wedging on a FlowControl wait that will
  never resolve correctly (software-ISO-TP) or silently handing an
  ambiguous multi-frame functional write to the vendor DLL (hardware
  channel).
- This service's own OBD-II ComParam defaults (`CP_RequestAddrMode = 2`)
  are unaffected for their common case (short diagnostic requests), but a
  caller attempting a large functional request — which was already
  protocol-invalid — now fails fast.
- `CanAddressing::functional` is available for any future code that needs
  the physical/functional distinction without re-reading
  `CP_RequestAddrMode` from the Active set itself.
- No change to filter installation (`rpc_link.rs`) or to the SAE J2534-1 TX
  size-range table (`protocol.rs::tx_message_size_range`, ADR-049) — this
  is an additional, addressing-mode-aware pre-flight check layered on top
  of the existing size-range check, not a replacement for it.
- **Extended by ADR-169:** this ADR's `max_sf_payload()`-based limit is
  Classic-CAN-sized (7/6 bytes) and was reused unchanged on a native
  `FD_ISO15765_PS` link (ADR-159), which has real CAN FD frame capacity —
  ADR-169 widens the limit for that one link family specifically, without
  changing this ADR's own Classic/software-ISO-TP behavior at all.

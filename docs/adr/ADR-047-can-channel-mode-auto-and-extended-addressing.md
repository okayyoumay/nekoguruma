# ADR-047: `can_channel_mode = "auto"` and Extended Addressing for Software ISO-TP

**Date:** 2026-07-03
**Status:** Accepted
**Affects:** `j2534-0404-service` can_mode.rs, isotp.rs, service.rs, events.rs, rpc_link.rs, rpc_misc.rs, rpc_primitive.rs, `j2534-0404-mock` lib.rs

## Context

ADR-046 introduced `can_channel_mode` with three modes (`dual-channel`,
`single-channel`, `software-isotp`) but left two gaps, both flagged as
follow-up work at the time:

1. Choosing `dual-channel` vs. `single-channel` requires knowing in advance
   whether the target J2534 device can hold an `ISO15765` channel and a `CAN`
   channel open simultaneously — information a deployer may not have, and
   which can vary by device model even within one vendor's product line.
2. The software ISO-TP engine (`service/isotp.rs`) only understood normal
   addressing (PCI byte first). ECUs configured for ISO 15765-2 extended
   addressing (D-PDU API `CP_Can*Format` Table B.13 bit 3, with the target
   address in `CP_Can*ExtAddr`) could not be reached in `software-isotp`
   mode, forcing those deployments onto a hardware ISO15765 channel even when
   `software-isotp` was otherwise the better fit for the device.

## Decision

### `can_channel_mode = "auto"`

Add a fourth mode, `Auto`, that probes hardware capability instead of
requiring the deployer to know it up front:

- The first time an ISO15765-family CLL establishes a *new* physical
  `ISO15765` channel, the service additionally attempts to open a companion
  raw `CAN` channel at the same baud rate, reusing the existing
  `ensure_uudt_companion_channel` / `release_uudt_companion_channel` helpers
  from ADR-046's dual-channel mode. The probe channel is closed immediately
  regardless of outcome — the probe only decides policy, it never leaves a
  channel open on the probing CLL's behalf.
- If the companion opens, the device is treated as `DualChannel` from then
  on; if it fails (e.g. `ERR_EXCEEDED_LIMIT`, `ERR_DEVICE_IN_USE`), it is
  treated as `SingleChannel`. The decision is cached for the lifetime of the
  service instance (`J2534Service::resolved_can_channel_mode`,
  `probe_can_channel_mode` in `rpc_link.rs`) so only the first qualifying
  connect pays the probe cost; every later `DualChannel`-vs-`SingleChannel`
  check goes through `effective_can_channel_mode()` instead of comparing
  `can_channel_mode` directly.
- `Auto` never resolves to `SoftwareIsoTp`: a working `ISO15765` channel is
  easy to tell apart from a failed second-channel open, but a
  *present-but-unreliable* `ISO15765` channel is not distinguishable from a
  healthy one by a connect-time probe. That choice is left to explicit
  configuration, same as ADR-046 left it.
- `hw_protocol_id()`/`is_software_isotp()` do not need the resolved decision:
  `Auto`'s `DualChannel`-vs-`SingleChannel` sub-decision only affects whether
  a companion channel is opened, not which hardware protocol the CLL's
  *primary* channel connects with, so `CreateComLogicalLink` can call them
  directly on the configured (possibly still-unresolved) `CanChannelMode`.
- `j2534-0404-mock` gained `__mock_set_max_channels(limit)` (and the
  underlying `MockState.max_channels` cap, returning `ERR_EXCEEDED_LIMIT`
  from `PassThruConnect` past the limit) so both probe outcomes — capable and
  incapable — are covered by `grpc_mock.rs` integration tests without needing
  real hardware.

### Extended addressing in `software-isotp` mode

`service/isotp.rs` gained an `Addressing` enum (`Normal` /
`Extended(ae_byte)`) consulted by every frame builder/parser (`single_frame`,
`first_frame`, `consecutive_frame`, `flow_control_frame`, `parse_frame`) and
by the payload-capacity/chunking helpers (`max_sf_payload`,
`ff_payload_len`, `cf_payload_len`, `consecutive_chunks`). Extended addressing
prepends a one-byte Address Extension before the PCI byte on every ISO-TP
frame type, reducing each frame's payload capacity by 1 byte;
`Addressing::from_format` decodes it from the same `CP_Can*Format` bit 3 /
`CP_Can*ExtAddr` pair the hardware `FLOW_CONTROL_FILTER` path
(`rpc_link.rs::CanIdFormat`) already reads for the same purpose. `parse_frame`
rejects a frame whose AE byte does not match the expected value, rather than
misreading it as part of the PCI.

Addressing is resolved **per UniqueRespIdTable entry**, not once per CLL,
mirroring how `CP_Can*Format`/`CP_Can*ExtAddr` are already per-entry ComParams:

- **TX** (`rpc_primitive.rs`, `CoptSendrecv`/`CoptStartcomm`): the entry whose
  `CP_CanPhysReqId` matches the outgoing CAN ID supplies `tx_addressing`
  (from `CP_CanPhysReqFormat`/`CP_CanPhysReqExtAddr`, for the frames this
  service builds) and `fc_rx_addressing` (from
  `CP_CanRespUSDTFormat`/`CP_CanRespUSDTExtAddr`, for parsing the ECU's
  FlowControl reply, carried on `SoftIsoTpTx`/`SoftIsoTpFraming`).
- **RX** (`events.rs`, `IsoTpRxContext`/`FcPair`): each table entry that
  names both a `CP_CanRespUSDTId` and a `CP_CanPhysReqId` carries its own
  `rx_addressing` (for parsing SF/FF/CF from that response ID) and
  `fc_tx_addressing` (for building the FlowControl frame this service sends
  back to the matching physical request ID).
- An entry with no `CP_Can*Format` set defaults to `Addressing::Normal`,
  preserving the pre-extended-addressing behaviour for entries that only
  configure a CAN ID.

## Consequences

- `single-channel`, `dual-channel`, and `software-isotp` are unaffected —
  `grpc_mock.rs`'s existing suite (pre-ADR-047) passes unchanged.
- `Auto` costs one extra `PassThruConnect`/`PassThruDisconnect` round-trip
  the first time a device's capability is needed; every subsequent CLL reuses
  the cached decision. A device whose channel capability changes at runtime
  (e.g. a USB VCI that only *sometimes* refuses a second channel) is not
  re-probed — the first result sticks for the service's lifetime.
- Extended addressing is still classic-CAN-only (`isotp.rs`'s CAN FD /
  4095-byte-payload scope from ADR-046 is unchanged) and still assumes one
  consistent addressing scheme per UniqueRespIdTable entry — mixed or
  functional addressing (where the same CAN ID answers to more than one AE)
  remains out of scope.

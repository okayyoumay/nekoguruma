# ADR-049: Enforce SAE J2534-1 Per-Protocol TX Message Size Range on `CoptSendrecv`

**Date:** 2026-07-03
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/protocol.rs` (`ChannelProtocol::tx_message_size_range`),
             `j2534-0404-service/src/service/rpc_primitive.rs`
             (`J2534Service::resolve_can_addressing`, `rpc_start_com_primitive`),
             `j2534-0404-service/tests/grpc_mock.rs`

## Context

SAE J2534-1 defines a per-protocol length range for the `PassThruMessage.Data`
buffer exchanged with the J2534 library:

| Protocol | Min Tx | Max Tx | Notes |
|---|---|---|---|
| CAN | 4 | 12 | 4-byte CAN ID + up to 8 data bytes |
| ISO15765 | 4 | 4099 | 4-byte CAN ID + up to 4095 data bytes |
| ISO15765 (extended addressing) | 5 | 4100 | 4-byte CAN ID + 1 AE byte + up to 4095 data bytes |
| J1850PWM | 3 | 10 | 3 header bytes + up to 7 data bytes |
| J1850VPW | 1 | 4128 | |
| ISO9141 | 1 | 4128 | |
| ISO14230 | 1 | 259 | 1-4 header bytes + up to 255 data bytes |
| ISO14230 (Manual Checksum) | 1 | 260 | Format not defined by the spec |
| SCI | 1 | 4128 | |

Before this change, the only length check anywhere on the TX path was
`j2534-0404`'s `PassThruMessage::new` → `validate_data_size`, a single
protocol-agnostic ceiling of `MAX_MESSAGE_DATA` (4128, the C struct's array
capacity). No minimum, and no protocol-specific maximum, was enforced — a
client could send a 1-byte CAN "message" (with no CAN ID at all) or a
4096-byte normal-addressing ISO15765 payload (1 byte too long) and the
service would forward it to the vendor DLL unchanged, surfacing whatever
error (or worse, undefined behaviour) the DLL produced. The only exception
was `software-isotp` mode's ad hoc `cop_data.len() < 5` / payload `> 4095`
check in `rpc_start_com_primitive`, specific to that one code path.

## Decision

Add `ChannelProtocol::tx_message_size_range(self, extended_addressing: bool)
-> RangeInclusive<usize>`, encoding the table above (`j2534_protocol_id()`
already maps every extended service-level protocol ID down to one of the 10
native J2534 protocol IDs, so this one table covers every protocol the
service supports). `rpc_start_com_primitive`'s `CoptSendrecv` handler
resolves the addressing `cop_data`'s CAN ID uses (ISO15765-family only, via
a new `J2534Service::resolve_can_addressing` helper that looks up the
matching `UniqueRespIdTable` entry's `CP_CanPhysReqFormat` /
`CP_CanPhysReqExtAddr`; defaults to Normal for every other protocol and when
`cop_data` is too short to carry a CAN ID at all), then rejects `cop_data`
outside the resulting range with `Status::invalid_argument` **before** the
primitive is queued — so the caller gets an immediate, specific gRPC error
instead of a hardware-level failure surfacing asynchronously from the poll
task.

`resolve_can_addressing` also returns the paired flow-control response CAN
ID and its addressing, replacing the inline closures `CoptSendrecv`'s
software-ISO-TP branch and `CoptStartcomm`'s tester-present-addressing logic
each had — both now call the same helper (a straightforward de-duplication:
the software-ISO-TP branch and `CoptStartcomm` already needed the same
UniqueRespIdTable lookup this check needs).

The `software-isotp` branch keeps its existing `cop_data.len() < 5` check
*in addition to* the table-driven range: the table's normal-addressing
minimum (4 total bytes) technically permits a CAN-ID-only message with zero
ISO-TP payload, but this service's own `isotp::single_frame` builder
`debug_assert!`s a non-empty payload (compiled out in release builds) and
would otherwise emit a PCI byte encoding an ISO 15765-2 SingleFrame with
`SF_DL = 0`, which is not a well-formed diagnostic request. This is a
deliberately stricter business rule layered on top of the spec's bare
minimum, not a contradiction of it.

**Scope, matching explicit decisions made when this was scoped:**

- **TX only.** RX (`PassThruReadMsgs` → `poll_rx_inner` /
  `process_frame_for_entry`) is unchanged; whatever the hardware returns is
  still forwarded to the client as-is. Enforcing the table's Min/Max Rx
  columns as well was considered and explicitly deferred.
- **Reject, never adjust.** Out-of-range `cop_data` is always rejected with
  `Status::invalid_argument`; the service never truncates or pads a
  request to fit.
- **`CoptSendrecv` only.** This is the primitive that carries client-supplied
  diagnostic payloads for every protocol and is the direct, universal
  analogue of "a message sent to the J2534 library." `CoptStartcomm`'s
  `init_data` (the ISO14230/ISO9141 fast-init wakeup frame, when non-empty
  and non-single-byte) and the periodic tester-present message are also
  eventually wrapped in a `PassThruMessage`, but are not validated by this
  change — they are service-managed control data rather than a per-call
  message the client actively composes, and were judged out of scope to
  keep this change focused on the primary, everyday data-exchange path.
- **ISO14230 "Manual Checksum" mode (260-byte variant) is not implemented.**
  This service has no connect flag, ComParam, or CLL state representing that
  mode; `tx_message_size_range` only enforces the base 1-259 ISO14230 range.
  Adding manual-checksum support is a separate, unscoped feature.

## Consequences

- Three existing `grpc_mock.rs` tests sent hardware-channel CAN/ISO15765
  `cop_data` without the leading 4-byte CAN ID
  (`can_protocol_forwards_bit_timing_and_extended_id_frame`,
  `iso15765_protocol_forwards_flow_control_params_and_plain_frame`,
  `iso15765_extended_id_and_frame_pad_tx_flags_are_forwarded`) — a shape the
  spec table (and this service's own `events.rs` doc comments) never
  actually allowed, but that nothing previously rejected. All three were
  updated to prepend a CAN ID, matching the convention every other
  CAN-family test and the software-ISO-TP code already use.
- Five new tests cover the boundary behaviour this ADR adds:
  `can_protocol_rejects_cop_data_outside_size_range`,
  `iso15765_protocol_rejects_cop_data_outside_size_range`,
  `iso15765_extended_addressing_widens_tx_size_range`,
  `j1850pwm_protocol_rejects_cop_data_outside_size_range`, and
  `iso14230_protocol_rejects_cop_data_outside_size_range`.
- A deployment that was previously sending undersized or oversized
  `cop_data` for a given protocol (relying on the vendor DLL to reject it,
  silently truncate it, or otherwise handle it out of spec) now gets a clear
  gRPC `INVALID_ARGUMENT` from the service itself instead. This is the
  intended behaviour change.

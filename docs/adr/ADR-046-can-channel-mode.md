# ADR-046: Selectable CAN Channel Operating Mode (`can_channel_mode`)

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `vci-service-config` lib.rs, `j2534-0404-service` service.rs, can_mode.rs (new), isotp.rs (new), events.rs, rpc_link.rs, rpc_misc.rs, rpc_primitive.rs

## Context

A D-PDU API CAN ComLogicalLink semantically covers three kinds of traffic at
once: segmented **USDT** request/response (ISO 15765-2), unsegmented **UUDT**
responses, and raw CAN frames. J2534 v04.04 splits these across two channel
types — `ISO15765` (USDT only) and `CAN` (raw frames) — and vendor libraries
differ widely in what they actually support:

- Some devices deliver UUDT frames on an `ISO15765` channel only through the
  non-conformant UUDT `FLOW_CONTROL_FILTER` workaround (ADR-041), or not at
  all.
- Some devices cannot open an `ISO15765` and a `CAN` channel simultaneously;
  others can.
- Some libraries have absent or unreliable `ISO15765` channel support but a
  solid raw `CAN` channel.

A single fixed mapping therefore cannot serve every deployment. The mapping
from CAN-family CLLs onto J2534 physical channels must be selectable per
J2534 library, without changing the gRPC interface.

## Decision

Add a `can_channel_mode` key to `config.toml`, read through
`vci-service-config::find_can_channel_mode()` with the priority
`arch+lib → api+lib → api` (mirroring `library_path`), parsed at service
startup into `CanChannelMode` (`service/can_mode.rs`). An invalid value fails
startup (fail fast on typos). Three modes:

1. **`"dual-channel"`** — one ISO15765-family CLL may use two physical
   channels: its primary `ISO15765` channel (USDT) plus a companion raw `CAN`
   channel for UUDT reception, opened on demand when the CLL's
   UniqueRespIdTable configures a `CP_CanRespUUDTId` (at
   `SetUniqueRespIdTable`, or at `ConnectComLogicalLink` when the table was
   set first). The companion is a normal `SharedChannel` keyed
   `(CAN, baud_rate)`, so raw-CAN CLLs and other CLLs' companions share it.
   Companion frames are routed to the CLL only when their CAN ID matches a
   `CP_CanRespUUDTId` (`RxEntryKind::Companion`) — everything else, including
   the raw view of USDT conversations, is dropped for that CLL to avoid
   duplicate delivery. The ADR-041 UUDT `FLOW_CONTROL_FILTER` workaround is
   *not* installed in this mode. Loss of either channel is a loss of comms
   for the CLL. Requires a library that supports two simultaneous channels.

2. **`"single-channel"`** (default) — the pre-ADR-046 behaviour: exactly one
   physical channel per CLL, chosen by its protocol (`ISO15765` for
   ISO15765-family, `CAN` for raw CAN). The other channel type's
   functionality is not reachable from that CLL; clients must open a second
   CLL with the other protocol (whether both can be connected at once depends
   on the library). UUDT reception on an ISO15765 CLL keeps relying on the
   ADR-041 filter workaround, i.e. on the device honouring it.

3. **`"software-isotp"`** — ISO15765-family CLLs always connect a raw `CAN`
   channel (`LogicalLinkState::hw_protocol_id = CAN`), and this service
   performs USDT itself (`service/isotp.rs` + `events.rs`): TX segmentation
   (SF/FF/CF) honouring the ECU's FlowControl (BS / STmin / Wait / Overflow,
   N_Bs timeout), RX reassembly with service-built FlowControl replies
   (BS/STmin from `CP_BlockSize`/`CP_StMin`, N_Cr expiry), and frame padding
   (`TX_ISO15765_FRAME_PAD` or `CP_CANFillerByteHandling`/`CP_CANFillerByte`).
   FlowControl destinations come from UniqueRespIdTable
   `CP_CanRespUSDTId`↔`CP_CanPhysReqId` pairs. UUDT-ID matches and
   non-ISO-TP frames on the channel are delivered raw, so UUDT and raw
   monitoring work without extra channels. RC21/RC23 re-requests are
   re-segmented through the same driver.

Supporting decisions:

- `LogicalLinkState` gains `hw_protocol_id` (the J2534 protocol actually
  passed to `PassThruConnect` and used for `ChannelKey`, message building,
  filter-family selection, and ADR-028 SET_CONFIG gating) as a distinct value
  from the service protocol's `j2534_protocol_id()`. All physical-resource
  comparisons (`find_physical_lock_holder`, filter installation, IOCTL
  filter re-install) now use `hw_protocol_id`, so in software-ISO-TP mode an
  ISO15765-family CLL and a raw-CAN CLL correctly count as one physical
  resource. A useful consequence of the ADR-028 gating: on a raw CAN channel
  the ISO15765-only CONFIG IDs (`ISO15765_BS`, `ISO15765_STMIN`, …) are never
  forwarded to hardware and are instead consumed by the software engine.
- RX fan-out was consolidated into one `poll_rx_inner` pass parameterised by
  an optional expected-response `MatchProbe` and an optional `FcCapture`
  (used by the TX driver to wait for FlowControl while other CLLs' traffic
  keeps flowing). Reassembled messages are delivered in the same
  `[4-byte CAN ID][payload]` layout as hardware ISO15765 frames, so
  expected-response matching, UniqueRespIdTable routing, and event encoding
  are mode-agnostic.

## Consequences

- Default deployments are unaffected (`single-channel` is bit-for-bit the
  old behaviour; the existing `grpc_mock.rs` suite passes unchanged).
- Dual-channel mode consumes an extra J2534 channel per baud rate; on 2-
  channel devices this leaves no channel for a separate K-line CLL.
- The software ISO-TP engine supports classic CAN only: CAN FD frames and
  >4095-byte payloads are out of scope (documented in `isotp.rs`). Normal
  *and* extended addressing are both supported — extended-addressing support
  was added after this ADR by ADR-047, which also adds a fourth mode,
  `"auto"`, that probes dual-channel capability instead of requiring it to be
  known up front.
- The companion channel is receive-only from the service's perspective and
  the CLL's Working params are not applied to it beyond the baud rate.
- `CLEAR_MSG_FILTERS` via `IoCtl` targets the CLL's primary channel only; the
  companion channel's PASS_FILTER is not rebuilt by that path.

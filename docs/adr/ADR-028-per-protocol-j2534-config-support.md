# ADR-028: Per-Protocol J2534 CONFIG Support Table

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/events.rs`

## Context

`ComParamId::to_j2534_config_id()` (ADR-027) translated a ComParam ID to its
J2534 native config parameter ID (for forwarding via `PassThruIoctl
SET_CONFIG`/`GET_CONFIG`) without regard to which J2534 protocol the
channel was connected with. In reality, each native J2534 CONFIG ID is only
valid for a specific subset of J2534 protocols — e.g. `BIT_SAMPLE_POINT` and
`SYNC_JUMP_WIDTH` are CAN-only (not valid on an `ISO15765` channel, even
though ISO15765 runs over CAN hardware and shares the physical channel via
`SharedChannel`); `NODE_ADDRESS` and `NETWORK_LINE` are J1850PWM-only;
`W0` is ISO9141-only while `W5` is ISO14230-only (both share `W1`-`W4`).
Forwarding a CONFIG ID to a protocol that doesn't support it risks an
adapter-rejected `SET_CONFIG` call or, worse, an adapter silently accepting
a parameter it doesn't actually apply.

## Decision

`ComParamId::to_j2534_config_id()` now takes a `j2534_protocol_id: u32`
parameter (the channel's `ChannelProtocol::j2534_protocol_id()`) and checks
it against an explicit per-CONFIG-ID support table (see
`docs/j2534-0404-architecture.md` §7 for the full table) before returning
`Some`. `P1_MIN`, `P2_MIN`, `P2_MAX`, `P3_MAX`, `P4_MAX` are not in the
table at all and now always translate to `None` for every protocol — they
have no `SET_CONFIG`/`GET_CONFIG` support regardless of protocol.

Call sites updated to pass the relevant protocol ID:
- `rpc_link.rs::apply_j2534_params` (called from `ConnectComLogicalLink`)
  gained a `protocol: ChannelProtocol` parameter — available at its call
  site from the already-fetched `LogicalLinkState`.
- `rpc_link.rs::rpc_set_com_param`'s physical-ComParam-lock check
  (`is_physical`) now fetches the link's protocol before calling
  `to_j2534_config_id`, since "is this a physical/hardware-forwarded
  param" is itself protocol-dependent.
- `events.rs::apply_params_to_hardware` (called from `CoptUpdateparam` and
  from `temp_param_update` handling in `CoptSendrecv`) gained a
  `j2534_protocol_id: u32` parameter. Two of its three call sites already
  had this value on hand (`TxItem::SendRecv::protocol_id`, populated from
  `link.protocol.j2534_protocol_id()` at enqueue time in
  `rpc_primitive.rs`); the third (`handle_update_param`) now fetches
  `l.protocol.j2534_protocol_id()` alongside the Working-set snapshot it
  already took.

## Consequences

- A ComParam value that numerically maps to a native J2534 CONFIG ID is
  silently **not** forwarded to hardware when the channel's protocol
  doesn't support that CONFIG ID (consistent with how service-level and
  unrecognized IDs were already silently skipped per ADR-027) — this is
  intentional: forwarding is opt-in per protocol, not opt-out.
- `comparam_support.rs`'s `is_can_param`/`is_kwp_param`/etc. (the
  `SetComParam`/`GetComParam` allowlist, governing which ComParam IDs a
  D-PDU client may reference at all for a protocol) is **not** changed by
  this ADR and still allows a coarser set (e.g. `NODE_ADDRESS` for both CAN
  and KWP families, interpreted there as J1939 tester source address /
  K-Line node address respectively) — that allowlist answers a different
  question ("can this ComParam be set/read at the D-PDU level") than
  `to_j2534_config_id` ("does this reach `PassThruIoctl`"), and a
  service-level value is a legitimate thing to store even when it has no
  hardware effect on that protocol.

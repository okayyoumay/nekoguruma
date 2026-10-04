# ADR-023: Service-Level Channel Protocol Abstraction (`ChannelProtocol`)

**Date:** 2026-06-30
**Status:** Accepted (the "Physical channel sharing" bullet's `ChannelKey` shape superseded by ADR-156, which widens it from `(j2534_protocol_id, baud_rate)` to a 3-tuple including a `pin_select` discriminant for J2534-2 Pin Selection; every other part of this ADR remains in force)
**Affects:**
- `j2534-0404-service/src/service/protocol.rs` (new)
- `j2534-0404-service/src/service.rs`
- `j2534-0404-service/src/service/names.rs`
- `j2534-0404-service/src/service/rpc_link.rs`
- `j2534-0404-service/src/service/rpc_primitive.rs`
- `j2534-0404-service/src/service/comparam_support.rs`

## Context

The J2534 v04.04 API identifies physical channel types using a small set of protocol IDs (J1850VPW=0x01 through SCI_B_TRANS=0x0A). The D-PDU compatibility layer, however, must distinguish between multiple application-layer protocol stacks that share the same J2534 channel type. For example:

- `ISO_15765_3_on_ISO_15765_2` and `ISO_14229_3_on_ISO_15765_2` both use J2534 `ISO15765` (0x06) at the hardware level, but represent different transport-layer / application-layer stacks with distinct initialisation and framing requirements.
- `ISO_11783_12_on_ISO_11783_5` uses J2534 `CAN` (0x05) but is not a generic CAN link.
- `SAE_J2610_on_SAE_J2610_SCI` maps to the existing `TX_FLAG_SCI_MODE` quirk, not a native J2534 SCI protocol ID.

Before this change, `names.rs:map_protocol_name()` mapped all ISO-15765-based stacks to the same raw `u32` value (`j2534_0404::ISO15765 = 0x06`). After `CreateComLogicalLink`, all semantic information about the application-layer protocol was lost; the service could not distinguish which stack the logical link was created for.

## Decision

Introduce `ChannelProtocol`, a newtype wrapping `u32`, in `service/protocol.rs`.

**Encoding:**
- Values 1–10: native J2534 protocol IDs. `value()` equals the J2534 hardware ID returned by `j2534_protocol_id()`.
- Values 0x0100+: service-level extended protocols. `j2534_protocol_id()` maps them to the underlying J2534 channel type (e.g. `ISO_14229_3_ON_ISO_15765_2` → ISO15765 = 0x06).

**Physical channel sharing is preserved:** `ChannelKey = (j2534_protocol_id, baud_rate)` continues to use the J2534 hardware protocol ID, so ISO_14229_3 and ISO_15765_3 logical links with the same baud rate still share a single physical J2534 channel.

**Call-site discipline:**
- `parse_protocol_id_from_resource()` returns `ChannelProtocol` (not `u32`).
- `LogicalLinkState.protocol: ChannelProtocol` stores the service-level identity.
- Hardware calls (`PassThruConnect`, `PassThruWriteMsgs`, filter installation) use `protocol.j2534_protocol_id()`.
- Service comparisons (resource status, conflict detection, lock scope) use `protocol.value()` (service-level ID) or `protocol.j2534_protocol_id()` (physical-resource scope), as appropriate.
- `check_param_allowed` accepts `ChannelProtocol` and uses the `is_*_family()` helpers.

## Consequences

- Future code can branch on `link.protocol` to apply stack-specific framing, timing, or addressing rules for protocols that share a J2534 channel type.
- The service-level protocol ID exposed to gRPC callers (e.g. in `GetConflictingResources.resource_id`) now reflects the precise application-layer stack (0x0101 for ISO_14229_3_ON_ISO_15765_2) rather than the raw J2534 hardware ID (0x06). Callers that previously matched on raw J2534 IDs must be updated if they expected the hardware value for extended protocols.
- The existing SCI-Mode quirk (`TX_FLAG_SCI_MODE` as a pseudo protocol ID) is preserved: `SAE_J2610_ON_SAE_J2610_SCI.j2534_protocol_id() == j2534_0404::SCI_MODE`.

See ADR-069, which layers an ISO 22900-2 resource-ID/bus-type-ID table on top of this `ChannelProtocol` abstraction (reusing it unchanged as each table row's connect protocol) and adds 6 further extended `ChannelProtocol` variants.

**2026-07-31 (Phase 2, ADR-156):** the "Physical channel sharing" bullet's
`ChannelKey = (j2534_protocol_id, baud_rate)` shape is superseded — J2534-2
Pin Selection (`_PS` protocol variants) requires two links with the same
hardware protocol and baud rate but *different* caller-selected pins to use
distinct physical channels, which a 2-tuple key cannot express. ADR-156
widens it to `(hw_protocol_id, baud_rate, pin_select)`, with `pin_select = 0`
for every non-`_PS` link — reproducing today's sharing behavior exactly for
every protocol that existed before Phase 2.

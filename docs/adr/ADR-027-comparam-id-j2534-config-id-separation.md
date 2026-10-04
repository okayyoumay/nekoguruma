# ADR-027: Type-Level Separation of ComParam IDs from J2534 Native Config IDs

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs`, `j2534-0404-service/src/service/comparam_id.rs`, `j2534-0404-service/src/service/comparam_defaults.rs`, `j2534-0404-service/src/service/comparam_support.rs`, `j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/events.rs`

## Context

`j2534-0404-service` stores D-PDU-style ComParam values (`SetComParam`/
`GetComParam`, `ComParamSet.unum32/bytes/structfield`) and, at
`ConnectComLogicalLink`/`CoptUpdateparam`, forwards a subset of them to the
J2534 adapter via `j2534_0404::J2534Api0404::set_config_u32`/`get_config_u32`
(`PassThruIoctl SET_CONFIG`/`GET_CONFIG`, which expect a native J2534 config
parameter ID such as `CONFIG_DATA_RATE = 0x01`).

Both ID spaces were the same Rust type (`u32`) and, for parameters that map
1:1 onto a J2534 native config parameter (e.g. `CP_Baudrate`), the same
numeric value — `names.rs` mapped the D-PDU name `"CP_Baudrate"` directly to
the constant `j2534_0404::DATA_RATE`, and that raw value was used as the
`ComParamSet` key everywhere (150+ sites in `comparam_defaults.rs` alone).

The only thing preventing an arbitrary ComParam ID from reaching
`set_config_u32` was `is_service_param()` — an exhaustive **blacklist** of
~130 known service-level IDs, used at the two hardware-forwarding call
sites (`apply_j2534_params` in `rpc_link.rs`, `apply_params_to_hardware` in
`events.rs`). Both looped over `ComParamSet.unum32` and forwarded every key
**not** in the blacklist, unchecked. Any ComParam ID accidentally missing
from the blacklist — or any ID a client sent that was never anticipated —
would have been silently forwarded to real hardware as a native config
parameter ID.

## Decision

Introduced `ComParamId(pub(super) u32)` (`comparam_id.rs`), a newtype
distinct from the plain `u32` `set_config_u32`/`get_config_u32` accept.
`ComParamId::to_j2534_config_id(self) -> Option<u32>` is an explicit
**whitelist**: `Some(self.0)` for the ~35 parameters with a real J2534
native config equivalent, `None` for everything else (all service-level
IDs, and — critically — any future or unrecognized ID by default).

Numeric values are **unchanged** — `ComParamId` wrapping
`j2534_0404::DATA_RATE` is still `0x01`. A full renumbering into a disjoint
ID range was considered and rejected: it would ripple through ~160 existing
test assertions' *expected values* (not just their types) for no additional
safety benefit, since the newtype alone already makes it a compile error to
pass a `ComParamId` where `set_config_u32`'s `u32` parameter is expected.

Mechanically:
- `ComParamSet`'s three `HashMap` fields are now keyed by `ComParamId`.
- All ~130 service-level `PARAM_*` constants in `service.rs` are now
  `ComParamId`-typed at their single definition site, so their ~500
  downstream usage sites (inserts, match arms, `RcHandlingConfig::from_params`)
  needed no changes.
- Bare `j2534_0404::<NATIVE_CONST>` used as a ComParam-space value (in
  `comparam_defaults.rs`, `comparam_support.rs`, `names.rs`, `rpc_link.rs`,
  `events.rs`) is wrapped in `ComParamId(...)` at each site — the main
  mechanical work, since these constants keep their original `u32` type in
  `j2534_0404` itself (unrelated crate, not touched).
- `apply_j2534_params`/`apply_params_to_hardware` now forward a param only
  when `to_j2534_config_id()` returns `Some` — replacing the blacklist with
  the whitelist, closing the "unrecognized ID silently forwarded" gap.
- `is_service_param` is removed entirely, including its third use as the
  `SetComParam` physical-ComParam-lock check in `rpc_link.rs`
  (`is_physical = param_id.to_j2534_config_id().is_some()`).
- The gRPC wire boundary (`rpc_set_com_param`/`rpc_get_com_param`,
  `rpc_set_unique_resp_id_table`) wraps the raw `u32` `com_param_id` field
  into `ComParamId` immediately on receipt, and unwraps via `.0` when
  writing a response — the only places a raw `u32` and a `ComParamId`
  legitimately meet.

## Consequences

- Forwarding an unrecognized or future ComParam ID to
  `PassThruIoctl SET_CONFIG`/`GET_CONFIG` is now impossible by construction:
  it requires an explicit `Some` arm in `to_j2534_config_id()`, not the
  absence of an entry in a separate blacklist.
- `Error`/`Status` messages built from `ComParamId` need `.0` to format as a
  number (`ComParamId` has no `Display`/`LowerHex` impl by design — this
  keeps the type from being casually used as if it were the raw ID).
- `j2534-0500` has a structurally identical `ComParamSet`-less design
  question (it does not yet have its own comparam-adapter service), so this
  ADR does not apply there; revisit if a `j2534-0500-service` is built with
  similar D-PDU compatibility logic.

**Update (ADR-028):** `to_j2534_config_id()` gained a `j2534_protocol_id: u32`
parameter — the translation now also depends on which J2534 protocol the
channel uses, not just the ComParam ID. This ADR's core decision (the
`ComParamId` newtype and the whitelist-not-blacklist approach) is
unaffected; see ADR-028 for the per-protocol support table.

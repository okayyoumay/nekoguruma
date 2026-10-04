# ADR-043: `SetUniqueRespIdTable` Respects `LOCK_PHYSICAL_COM_PARAMS` on ISO15765

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs` (`rpc_set_unique_resp_id_table`)

## Context

`LOCK_PHYSICAL_COM_PARAMS` (`LockResource` mask bit 0) exists so one CLL can
get exclusive privilege to modify the physical ComParams of a shared J2534
channel while other CLLs sharing it are blocked. `rpc_set_com_param`
(`rpc_link.rs`) enforces this: before writing a `unum32` value, it checks
whether the param is "physical" (`ComParamId::to_j2534_config_id(...)
.is_some()` — i.e. has a native J2534 `SET_CONFIG` mapping) and, if so,
rejects the call with `Status::resource_exhausted` when another CLL sharing
the same physical resource (`channel_key` once connected, else matching
`j2534_protocol_id`) holds the lock.

Since ADR-039, `SetUniqueRespIdTable` (`rpc_set_unique_resp_id_table`,
`rpc_misc.rs`) also performs real hardware I/O on ISO15765 channels: it
(re)installs point-to-point `FLOW_CONTROL_FILTER`s via
`PassThruStartMsgFilter` / `PassThruStopMsgFilter`, replacing the connect-time
pass-all fallback once a CLL's addressing is configured. This is a physical
mutation of the shared channel just like a `SET_CONFIG` write is, but nothing
checked `LOCK_PHYSICAL_COM_PARAMS` before doing it — a CLL holding the lock to
protect a shared ISO15765 channel's physical configuration could still be
undercut by another CLL calling `SetUniqueRespIdTable`, which would silently
succeed and change the channel's filter set out from under the lock holder.

This gap exists because `LOCK_PHYSICAL_COM_PARAMS`'s enforcement point
(`is_physical = param_id.to_j2534_config_id(...).is_some()`, introduced by
ADR-027) is inherently `SetComParam`-shaped: it answers "does *this specific
param* reach `PassThruIoctl SET_CONFIG`". `PDU_PC_UNIQUE_ID` class params
(`CP_CanPhysReqId`, `CP_CanRespUSDTId`, `CP_CanRespUUDTId`, ...) never have a
`to_j2534_config_id()` mapping (by design — see ADR-042), so they were never
"physical" by that test, and `SetUniqueRespIdTable` — a wholly separate RPC
that writes a wholly separate `LogicalLinkState` field
(`unique_resp_id_table`, not `working`/`active`) — never consulted the lock at
all. ADR-027 predates ADR-039's introduction of hardware I/O into
`SetUniqueRespIdTable` by one day, so the narrowing was not a deliberate
decision about `SetUniqueRespIdTable`; it simply never had lock enforcement to
begin with.

## Decision

`rpc_set_unique_resp_id_table` now performs the same lock-conflict check as
`rpc_set_com_param`, gated on protocol rather than per-param: if the CLL's
protocol is ISO15765 (the only case in which this RPC touches hardware — see
`adapter-design.md`/ADR-039), and any *other* CLL sharing the same physical
resource holds `LOCK_PHYSICAL_COM_PARAMS`, the call is rejected with
`Status::resource_exhausted` before any table validation or hardware I/O
happens. Non-ISO15765 CLLs are exempt — `SetUniqueRespIdTable` never mutates
hardware for them (the table is stored in-memory only, picked up if/when the
CLL later connects on an ISO15765 channel), so there is nothing for the lock
to protect there, mirroring how `SetComParam` exempts service-level params
that never reach `SET_CONFIG`.

The "same physical resource" comparison reuses the exact match used by
`SetComParam`: `channel_key` equality once both CLLs are connected, falling
back to `j2534_protocol_id()` equality for CLLs not yet connected (so the
lock also blocks pre-connect table writes that would apply once the CLL
connects and shares an already-locked channel).

The snapshot of `(protocol, channel_id, channel_key)` that the function
already took later on (to decide whether/how to rebuild the filter set) is
reused for the lock check instead of being fetched a second time, removing a
redundant `logical_links` lock acquisition as a side effect.

## Alternatives Considered

1. **Check the lock per-entry, only for entries that would actually produce a
   filter** (i.e., have both `CP_CanPhysReqId` and a response ID) — Rejected
   as unnecessary complexity: the whole `SetUniqueRespIdTable` call removes
   and reinstalls the *entire* filter set for the CLL in one operation
   (`remove_point_to_point_fc_filters` then `install_point_to_point_fc_filters`),
   so partial-entry granularity would not reflect what actually happens to
   the shared channel's filter state.

2. **Extend `ComParamId::to_j2534_config_id` (or add a parallel predicate) so
   `PDU_PC_UNIQUE_ID` params report as "physical"** — Rejected: that function
   answers a `SET_CONFIG`-specific question by construction (ADR-027), and
   `PDU_PC_UNIQUE_ID` params are never forwarded to `SET_CONFIG` — reusing it
   for a different hardware call (`PassThruStartMsgFilter`) would conflate two
   distinct meanings of "physical" for no benefit, since `SetUniqueRespIdTable`
   needs a call-level (not per-param) lock check anyway (see Alternative 1).

## Consequences

- A CLL holding `LOCK_PHYSICAL_COM_PARAMS` on an ISO15765 channel now also
  blocks other CLLs sharing that channel from calling `SetUniqueRespIdTable`,
  closing the gap where the lock's hardware-mutation guarantee did not
  actually cover every hardware-mutating RPC on the shared resource.
- Non-ISO15765 `SetUniqueRespIdTable` calls, and calls by the lock holder
  itself, are unaffected.
- The error message ("physical ComParam lock is held by another
  ComLogicalLink on this resource") is shared verbatim with `SetComParam`'s,
  since both RPCs are now enforcing the same lock for the same reason.

> **Amended by ADR-068**: `SetUniqueRespIdTable` now only stages the Working
> UniqueRespIdTable (the table gets a Working/Active split); the actual
> `FLOW_CONTROL_FILTER` hardware I/O this ADR's rationale refers to happens
> later, at promotion time. The lock check described above stays at
> Set/stage time regardless — the earliest possible rejection point,
> consistent with how `SetComParam` checks the lock before writing Working
> rather than waiting for a promotion that may never come — while the
> protected hardware I/O itself is now separately gated at promotion by
> ADR-044's `CoptUpdateparam` lock check (and by `ConnectComLogicalLink`'s
> existing connect-time check, ADR-045).

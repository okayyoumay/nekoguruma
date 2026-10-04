# ADR-045: `ConnectComLogicalLink` Respects `LOCK_PHYSICAL_COM_PARAMS` for New Channels

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_link.rs` (`rpc_connect_com_logical_link`)

## Context

ADR-043 and ADR-044 closed two gaps where a hardware-mutating operation on a
shared physical channel bypassed `LOCK_PHYSICAL_COM_PARAMS` even though
`SetComParam` checks it. Auditing the remaining `set_config` call sites (there
are exactly two in `j2534-0404-service`: the one inside
`apply_params_to_hardware` in `events.rs`, now fully covered by ADR-044, and
`apply_j2534_params` in `rpc_link.rs`) turned up a third instance in
`rpc_connect_com_logical_link`.

When the first CLL connects on a physical channel that does not yet exist
(`is_new_channel`), `rpc_connect_com_logical_link` calls `PassThruConnect`
then `apply_j2534_params`, which writes the CLL's Working ComParam set to
hardware via `PassThruIoctl SET_CONFIG` — a real physical mutation, exactly
the kind `LOCK_PHYSICAL_COM_PARAMS` exists to guard. This call had no lock
check at all.

Unlike ADR-043's channel-key-based check, this call site runs *before* the
CLL has a `channel_key` (it is in the process of being assigned one). But
`LockResource` already supports acquiring `LOCK_PHYSICAL_COM_PARAMS`
*before* a CLL connects — the "pre-connect reservation" case documented in
`rpc_lock_resource`, which matches resource conflicts by `j2534_protocol_id()`
alone when either side lacks a `channel_key`. This makes the gap concretely
exploitable, not just theoretical:

1. CLL A is created (protocol `CAN`) but not connected, and calls
   `LockResource(LOCK_PHYSICAL_COM_PARAMS)` — a legitimate pre-connect
   reservation, granted because no other CLL on the `CAN` protocol holds it
   yet.
2. `SetComParam` on any *other* CLL sharing that protocol is now correctly
   blocked by the existing `is_physical` check in `rpc_set_com_param` (a
   physical param like `DATA_RATE` always has a `to_j2534_config_id()`
   mapping).
3. But CLL B does not need `SetComParam` at all to get a valid `DATA_RATE`:
   `CreateComLogicalLink` pre-populates the Working set from
   `comparam_defaults::bustype_default_params(bustype_name)` — e.g. naming
   bustype `ISO_11898_2_DWCAN` gives `DATA_RATE = 500_000` — writing directly
   into `LogicalLinkState::working` without going through the lock-checked
   `SetComParam` RPC at all.
4. CLL B then calls `ConnectComLogicalLink`. Since its `(protocol_id,
   baud_rate)` pair has never been used, this creates a brand-new physical
   channel and unconditionally calls `apply_j2534_params` → `SET_CONFIG`,
   completely bypassing CLL A's held lock.

## Decision

The `is_new_channel` branch of `rpc_connect_com_logical_link` (the `else` arm
of `if let Some(sc) = chans.get(&channel_key) { .. } else { .. }`, which is
the only branch that performs a hardware write — the branch above it, joining
an already-connected channel, applies nothing at connect time; see ADR-044)
now checks for a lock conflict *before* calling `PassThruConnect`: if any
*other* CLL holds `LOCK_PHYSICAL_COM_PARAMS` for the same `j2534_protocol_id`,
the call is rejected with `Status::resource_exhausted`.

The match uses `j2534_protocol_id()` equality only, not a `channel_key`
comparison — this reuses exactly the granularity `LockResource`'s own
pre-connect reservation already uses (`rpc_lock_resource`'s `_ =>
l.protocol.j2534_protocol_id() == protocol.j2534_protocol_id()` fallback),
since by construction no existing CLL can already share this not-yet-created
`channel_key` (if one did, `chans.get(&channel_key)` would have found it and
this branch would not run at all).

## Alternatives Considered

1. **Match on `channel_key` where possible, like ADR-043/ADR-044's checks** —
   Not applicable here: in the `is_new_channel` branch, no other CLL can
   already hold this exact `channel_key` (proven by the branch condition
   itself), so a `channel_key`-based comparison would always fall through to
   the protocol fallback anyway. Using the fallback directly is simpler and
   equivalent.

2. **Leave this uncovered, since it requires a caller to deliberately acquire
   a pre-connect reservation** — Rejected: pre-connect reservation is a
   documented, intended `LockResource` use case (`rpc_lock_resource`'s own
   comment), not an edge case to dismiss. A lock that can be silently
   defeated by a second CLL relying on bustype defaults instead of
   `SetComParam` does not provide the exclusivity guarantee its name promises.

3. **Reject `CreateComLogicalLink` from pre-populating `DATA_RATE` (or any
   physical param) from bustype defaults, forcing every physical value
   through the lock-checked `SetComParam` path** — Rejected: bustype-default
   population is unrelated to locking (ADR predates any lock-check design)
   and used throughout the service (see `comparam_defaults.rs`); changing it
   would affect every CLL, not just the narrow locking scenario this ADR
   addresses, and the lock check at `ConnectComLogicalLink` closes the gap
   just as effectively without touching default population.

## Consequences

- `ConnectComLogicalLink` now returns `Status::resource_exhausted` when
  establishing a brand-new physical channel if another CLL already holds
  `LOCK_PHYSICAL_COM_PARAMS` for the same J2534 protocol — whether that CLL
  acquired the lock pre-connect or is already connected on a different
  channel of the same protocol.
- Joining an already-connected channel (the `!is_new_channel` branch) is
  unaffected, since it performs no hardware write at connect time.
- Together with ADR-043 and ADR-044, every `PassThruIoctl SET_CONFIG` call
  site in `j2534-0404-service` now respects `LOCK_PHYSICAL_COM_PARAMS`:
  `SetComParam` (writes the Working set only, but gated defensively),
  `ConnectComLogicalLink` (this ADR), and `CoptUpdateparam` /
  `CoptSendrecv`+`temp_param_update` (ADR-044). `SetUniqueRespIdTable`'s
  separate `PassThruStartMsgFilter`/`PassThruStopMsgFilter` hardware calls are
  covered by ADR-043.

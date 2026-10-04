# ADR-011: DATA_RATE Excluded from CoptUpdateparam Hardware Application

**Date:** 2026-06-28  
**Status:** Accepted  
**Affects:**
- `j2534-0404-service/src/service/events.rs` (`apply_params_to_hardware`)

## Context

`CoptUpdateparam` applies the Working ComParam set to the J2534 hardware via
`PassThruIoctl SET_CONFIG`.  This is handled by `apply_params_to_hardware` in the
poll task, which iterates `ComParamSet::unum32` and calls `set_config_u32` for
every param that is not service-specific.

`DATA_RATE` (J2534 param 0x0004) is a unum32 param stored in the Working set when
the caller issues `SetComParam(DATA_RATE, baud_rate)`.  It is used as the baud rate
argument to `PassThruConnect` at `ConnectComLogicalLink` time.

**Bug (M-AUDIT-3):**  
After the channel is established, `apply_params_to_hardware` (called by
`CoptUpdateparam` and by `temp_param_update` in `CoptSendrecv`) also attempted to
`SET_CONFIG(DATA_RATE, value)` on the open channel.  Most J2534 adapters reject this
with an error because the baud rate is a property of the physical channel and cannot
be changed after `PassThruConnect`.

As a result, calling `CoptUpdateparam` after changing any timing param — even one
the adapter would happily accept — would fail with `PduErrEvtProtErr` if `DATA_RATE`
appeared anywhere in the Working set.  The Working → Active promotion was blocked
by the DATA_RATE rejection.

## Decision

`apply_params_to_hardware` now skips `DATA_RATE` in addition to service-specific
params:

```rust
if !is_service_param(param_id) && param_id != j2534_0404::DATA_RATE {
    api.set_config_u32(channel_id, param_id, value)?;
}
```

This mirrors the approach in `apply_j2534_params` (used in `rpc_connect_com_logical_link`),
which already accepts a `skip: Option<u32>` argument and skips `DATA_RATE` explicitly
(`Some(j2534_0404::DATA_RATE)`) because it was already passed to `PassThruConnect`.

The baud rate of an established J2534 channel is immutable.  To change baud rate, the
caller must:
1. `CoptStopcomm` (if comm is active)
2. `DisconnectComLogicalLink`
3. `SetComParam(DATA_RATE, new_value)`
4. `ConnectComLogicalLink` (opens a new physical channel at the new baud rate)

## Alternatives Considered

1. **Pass `skip: Some(DATA_RATE)` to a shared helper** — Would require refactoring
   `apply_params_to_hardware` to accept a skip parameter (matching `apply_j2534_params`).
   Adds API surface with no benefit; the simpler in-line guard is equivalent.

2. **Strip DATA_RATE from Working when it is already in Active** — More targeted but
   adds complexity and would silently discard the value from a round-trip through
   `GetComParam`.  The Working set should faithfully store what the caller set.

3. **Return an error if DATA_RATE changes on a connected CLL** — Shifts the burden to
   callers; service-level enforcement is cleaner.

## Consequences

- `CoptUpdateparam` and `CoptSendrecv` (with `temp_param_update=true`) no longer
  attempt to SET_CONFIG the baud rate on an established channel.
- Callers may store any `DATA_RATE` value in the Working set; it will be applied only
  at the next `ConnectComLogicalLink` call (when a new physical channel is opened).
- The Active set's `DATA_RATE` value after `ConnectComLogicalLink` reflects the
  rate actually used for `PassThruConnect`; subsequent `SetComParam(DATA_RATE, x)`
  updates only the Working set, not the Active set, until reconnection.

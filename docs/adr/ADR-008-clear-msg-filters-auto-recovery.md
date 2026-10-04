# ADR-008: Auto-Recovery of Pass-All Filter After CLEAR_MSG_FILTERS

**Date:** 2026-06-28  
**Status:** Superseded by ADR-038  
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs` (`rpc_io_ctl`, CLEAR_MSG_FILTERS branch)

## Context

ADR-005 established that the service installs a pass-all `PASS_FILTER` (and a
`FLOW_CONTROL_FILTER` for ISO15765) on every newly opened J2534 channel.  Without
these filters, J2534 adapters block all inbound frames by default.

The `CLEAR_MSG_FILTERS` IoCtl handler called `PassThruIoctl CLEAR_MSG_FILTERS`,
which removes **all** hardware-level message filters including the pass-all filter
installed at connect time.  After this call the adapter receives no further frames,
and because `ConnectComLogicalLink` short-circuits on already-connected channels,
there was no RPC path to restore the filter without a full disconnect/reconnect
cycle.  The service effectively went deaf to all incoming frames.

## Decision

> **Superseded by ADR-038:** step 1 below (`PASS_FILTER`) is no longer
> installed on ISO15765 channels — the J2534 v04.04 spec permits only
> `FLOW_CONTROL_FILTER` there. See ADR-038 for the corrected per-protocol
> filter type selection. The rest of this ADR (re-install after
> `CLEAR_MSG_FILTERS`, same lock scope, best-effort failure handling) still
> stands.

After a successful `CLEAR_MSG_FILTERS` call, the service immediately re-installs
the pass-all filter within the **same `api` lock scope**:

1. `PassThruStartMsgFilter(PASS_FILTER, mask=0x00000000, pattern=0x00000000)` —
   restores the unconditional pass filter for all protocols.
2. For ISO15765 channels only:
   `PassThruStartMsgFilter(FLOW_CONTROL_FILTER, mask=0, pattern=0, flowcontrol=0)` —
   restores the zero-ID flow-control filter so ISO15765 multi-frame sessions are
   not broken.

If re-installation fails (e.g., the adapter reached its filter table limit), a
`warn!` log is emitted but the IoCtl still returns success — the clear completed
as requested; the failure to re-install is a best-effort recovery.

The re-install uses the same parameters as ADR-005 (`install_pass_all_filter` in
`rpc_link.rs`), inlined here to avoid cross-sibling module visibility overhead.
The `link.protocol_id` obtained from `get_link_state` at the top of `rpc_io_ctl`
provides the protocol without an additional lock acquisition.

## Alternatives Considered

1. **Return `Status::unimplemented` for CLEAR_MSG_FILTERS** — Avoids the
   complexity but breaks callers that legitimately want to rotate filter sets
   (e.g., switching from functional to physical addressing).

2. **Require callers to re-install filters via a follow-up RPC** — Maintains
   explicit control but violates the J2534 spec's implied contract that the
   adapter remains in a usable state after standard IoCtl calls.

3. **Track per-channel filter state and re-install on next read_messages** — More
   robust but adds state management overhead and a latency gap where frames are
   dropped between the clear and the next poll.

## Consequences

- `CLEAR_MSG_FILTERS` is now safe to call without disrupting the receive path.
  Callers that want to narrow the filter set (e.g., install specific CAN ID
  filters after clearing) must call `StartMsgFilter` explicitly after the IoCtl.
- The pass-all filter IDs change on each re-install; any stale filter handles
  from before the clear are invalidated by the adapter, which is the expected
  J2534 behaviour.
- Filter re-installation happens under the `api` lock, so it is atomic with
  respect to the poll task's `PassThruReadMsgs` calls.

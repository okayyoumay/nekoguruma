# ADR-044: `CoptUpdateparam` / `CoptSendrecv`+`temp_param_update` Respect `LOCK_PHYSICAL_COM_PARAMS`

**Date:** 2026-07-02
**Status:** Superseded by ADR-110
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`)

## Context

`SetComParam` (`rpc_link.rs`) checks `LOCK_PHYSICAL_COM_PARAMS` before writing
a physical-layer param into the Working set — but, per
`docs/rpc-api-guide.md` / `j2534-0404-architecture.md`, `SetComParam` only
ever touches the in-memory Working set; it never calls `PassThruIoctl
SET_CONFIG` itself. The actual hardware write happens later, at
`ConnectComLogicalLink` (Offline → Online, for the first CLL on a new
channel) or at `CoptUpdateparam` (`COPT_UPDATEPARAM`, Working → Active,
`handle_update_param` in `events.rs` → `apply_params_to_hardware` →
`api.set_config(...)`).

`rpc_start_com_primitive` (`rpc_primitive.rs`) already checks
`LOCK_PHYSICAL_TX_QUEUE` for `CoptSendrecv` / `CoptStartcomm` (which
transmit), but had **no** `LOCK_PHYSICAL_COM_PARAMS` check for
`CoptUpdateparam` at all. `CoptUpdateparam` was enqueued (`TxItem::UpdateParam`)
unconditionally, regardless of any lock held by another CLL. This is the same
shape of gap fixed for `SetUniqueRespIdTable` in ADR-043: a hardware-mutating
operation that bypassed the lock meant to protect exactly that mutation. In
fact this gap is more direct than ADR-043's: `SetComParam` itself checks the
lock, creating the false impression that physical ComParam writes are
protected, when the RPC that actually performs the write
(`CoptUpdateparam`) was not.

A second, related path does the same kind of write: `CoptSendrecv` with
`ComPrimitiveCtrlData.temp_param_update` set (ISO 22900-2 §9.4.3) applies the
Working param set to hardware via `SET_CONFIG` immediately before
transmitting, then restores the Active set afterward
(`TxItem::SendRecv` handling in `events.rs`, both calls going through the same
`apply_params_to_hardware` helper `CoptUpdateparam` uses). This path *is*
already gated by `LOCK_PHYSICAL_TX_QUEUE` (since it is dispatched under
`CoptSendrecv`), which incidentally protects it whenever the two locks are
held together by the same CLL. But a CLL that holds only
`LOCK_PHYSICAL_COM_PARAMS` (e.g. because it specifically wants to protect its
physical configuration but has no need to reserve the TX queue) gets no
protection against another CLL's `temp_param_update` write, since that write
was never checked against `LOCK_PHYSICAL_COM_PARAMS` — only against
`LOCK_PHYSICAL_TX_QUEUE`.

## Decision

`rpc_start_com_primitive` now runs a second lock-conflict check — identical in
shape to the existing `LOCK_PHYSICAL_TX_QUEUE` check and to `SetComParam`'s —
gated on `writes_physical_com_params`, true when:

- `cop_type == CoptUpdateparam` (always writes hardware via `SET_CONFIG`), or
- `cop_type == CoptSendrecv` and `cop_ctrl_data.temp_param_update != 0`.

If any *other* CLL sharing the same physical resource (`channel_key` once
connected, else `j2534_protocol_id()` match, matching the existing pattern)
holds `LOCK_PHYSICAL_COM_PARAMS`, the call is rejected with
`Status::resource_exhausted` before the COP is enqueued — using the same
error message `SetComParam` already returns for a physical-ComParam-lock
conflict, since both RPCs now enforce the same lock for the same reason.

This check runs independently of (and in addition to) the existing
`LOCK_PHYSICAL_TX_QUEUE` check for `CoptSendrecv`: a `temp_param_update`
`CoptSendrecv` call can be rejected for either lock conflict, whichever
applies.

`CoptRestoreParam` is explicitly out of scope: it copies Active → Working
in-memory only (`handle_restore_param`, `events.rs`) and never calls
`set_config`, so there is no hardware mutation for the lock to protect.
`CoptStartcomm`, `CoptStopcomm`, and `CoptDelay` also do not call
`set_config`.

> **Extended by ADR-066**: `CoptStartcomm` did not support `temp_param_update`
> at all at the time of this decision. ADR-066 adds `temp_param_update`
> support to `CoptStartcomm`'s transient init transaction, which does call
> `set_config` (bracketing the init step) — so this lock check is extended to
> cover `CoptStartcomm` with the flag set, the same way it already covers
> `CoptSendrecv`. `CoptStopcomm` remains out of scope even with the flag set:
> it still calls no `set_config` regardless (ADR-066 accepts the flag there
> as a no-op).

> **Amended (not superseded) by ADR-067**: ADR-067 adds a separate,
> synchronous `PDU_PC_BUSTYPE` guard (`PDU_ERR_TEMPPARAM_NOT_ALLOWED`) that
> runs for every `temp_param_update=1` call on `CoptSendrecv`/`CoptStartcomm`/
> `CoptStopcomm`, checked *before* and *independently of* this ADR's
> `LOCK_PHYSICAL_COM_PARAMS` check. The two checks are unrelated: this ADR's
> lock check protects against a *different CLL* concurrently writing physical
> ComParams; ADR-067's guard rejects a *single CLL's own* attempt to stage a
> bus-physical ComParam change through `temp_param_update` at all, regardless
> of any lock. ADR-067 also moves ComParam resolution itself back to
> `StartComPrimitive` call time (a snapshot bound then, not read live by the
> poll task), which does not change this ADR's lock-check scope or timing.

> **Cross-referenced by ADR-085**: ADR-085 gives `CoptStopcomm` a non-empty
> `cop_data` bus transmit (a final fire-and-forget message). That transmit is
> not a config write — it calls no `set_config` / `PassThruIoctl
> SET_CONFIG` — so it does not affect this ADR's scope: `CoptStopcomm` still
> writes no hardware ComParam config and is still never blocked by
> `LOCK_PHYSICAL_COM_PARAMS`, with or without `cop_data`. (ADR-085 instead
> makes non-empty-`cop_data` `CoptStopcomm` join the *separate*
> `LOCK_PHYSICAL_TX_QUEUE` check, for the same reason `CoptSendrecv`/
> `CoptStartcomm` are already checked against it.) This ADR remains Accepted
> and unmodified; nothing here is superseded.

## Alternatives Considered

1. **Only fix `CoptUpdateparam`, leave `temp_param_update` alone** — Rejected:
   `temp_param_update` performs the identical hardware call
   (`apply_params_to_hardware`) for the identical reason (pushing physical
   ComParam values live); leaving it unguarded would repeat the exact same
   category of bug this ADR (and ADR-043) exists to close, just discovered
   later. The fact that it happens to be *partially* covered by the TX queue
   lock (when a CLL holds both locks) does not mean it needs no
   `LOCK_PHYSICAL_COM_PARAMS` check of its own.

2. **Fold `temp_param_update` detection into the existing TX-queue-lock `if`
   block, requiring both locks whenever `temp_param_update` is set** —
   Rejected: `LOCK_PHYSICAL_TX_QUEUE` and `LOCK_PHYSICAL_COM_PARAMS` are
   independently acquirable and semantically distinct (transmit privilege vs.
   ComParam-modification privilege); a caller should not be forced to hold
   `LOCK_PHYSICAL_TX_QUEUE` just to get `LOCK_PHYSICAL_COM_PARAMS`'s
   protection. Two independent checks (this ADR's approach) keep the two
   locks orthogonal, matching how `LockResource`/`UnlockResource` already
   treat them as separate mask bits.

3. **Also check the lock at `ConnectComLogicalLink` time**, since the first
   CLL on a new physical channel also calls `apply_j2534_params` →
   `set_config` — Considered but left out of this ADR's scope: that call only
   ever happens for the *first* CLL establishing a genuinely new
   `channel_key` (joining CLLs never re-push their Working set at connect
   time — see `adapter-design.md`, "Physical Channel Sharing"), so a
   same-`channel_key` conflict is structurally impossible at that call site.
   The only theoretical collision is the pre-connect, protocol-only fallback
   match (`j2534_protocol_id()` equality) colliding across two *different*
   not-yet-created channels (e.g. two different baud rates on the same J2534
   protocol) — a materially different, narrower question about whether that
   fallback-match granularity is itself too coarse, which is unrelated to the
   "physical write bypasses the lock" bug pattern this ADR fixes. Flagged for
   a future ADR if it proves to matter in practice, rather than folded in here
   speculatively.

## Consequences

- `StartComPrimitive(COPT_UPDATEPARAM)` now returns
  `Status::resource_exhausted` if another CLL sharing the physical resource
  holds `LOCK_PHYSICAL_COM_PARAMS`, closing the main gap: `SetComParam`'s lock
  check was previously decorative for any client relying on
  `LOCK_PHYSICAL_COM_PARAMS` to actually prevent a competing CLL's *hardware*
  ComParam changes, since the write itself happened at `CoptUpdateparam`,
  unchecked.
- `StartComPrimitive(COPT_SENDRECV)` with `temp_param_update` set is now
  blocked by either lock conflict, not just the TX queue one.
- The lock holder's own `CoptUpdateparam` / `temp_param_update` calls are
  unaffected, matching `SetComParam`'s existing self-exemption (`h != handle`
  in the conflict check).

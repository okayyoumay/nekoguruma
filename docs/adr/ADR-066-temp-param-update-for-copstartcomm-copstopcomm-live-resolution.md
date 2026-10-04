# ADR-066: temp_param_update for CoptStartcomm/CoptStopcomm: Live Resolution Bounded to the Transient Init Transaction

**Date:** 2026-07-07
**Status:** Superseded by ADR-067
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`,
             `CoptStartcomm`/`CoptStopcomm` branches, `resolve_tester_present`,
             `resolve_init_tx_flags`),
             `j2534-0404-service/src/service/events.rs` (`handle_start_comm`),
             `j2534-0404-service/src/service.rs` (`TxItem::StartComm`, `ComParamSource`)

## Context

ADR-063/ADR-064 fixed a race for `CoptSendrecv`: resolving ComParam-dependent
TX state (addressing, message construction, TxFlags, ISO-TP framing) at
`StartComPrimitive` call time could observe a stale Active/Working set if a
`CoptUpdateparam`/`CoptRestoreParam` was already queued ahead of it on the
same CLL — `StartComPrimitive` returns as soon as its `TxItem` is enqueued,
not after the poll task actually processes it. The fix deferred that
resolution to the poll task's execution of the COP itself, reading whichever
ComParam set is live at that moment.

`CoptStartcomm` had the exact same shape of bug and was explicitly left out
of scope by both ADRs: ADR-063 states "`CoptStartcomm` remains entirely out
of scope (ADR-044): `temp_param_update` is only defined for
`PDU_COPT_SENDRECV`", and `rpc_start_com_primitive`'s `CoptStartcomm` branch
built the full tester-present message, its TxFlags/addressing, ISO-TP framing,
and the fast-init wakeup frame's TxFlags all synchronously from a single
`link.active` snapshot taken at `StartComPrimitive` call time, before the COP
was even enqueued. A `CoptUpdateparam` queued immediately ahead of a
`CoptStartcomm` on the same CLL had no guarantee of being reflected in it,
for the same structural reason ADR-064 fixed for `CoptSendrecv`.

Separately, ISO 22900-2 §9.4.3 does not actually restrict `temp_param_update`
to `PDU_COPT_SENDRECV` — it is a general `ComPrimitiveCtrlData` field, and
nothing in the spec's COP model prevents `PDU_COPT_STARTCOMM` from wanting to
perform its protocol-initialisation sequence (5-baud/fast-init) against a
temporarily-staged physical ComParam set (e.g. different K-line timing) that
was never promoted via `CoptUpdateparam`. `CoptStartcomm` was rejected as
"out of scope" by ADR-044/063 purely because it hadn't been implemented yet,
not because the D-PDU model forbids it.

Naively extending the `CoptSendrecv` `temp_param_update` mechanism
(apply Working, transmit, revert to Active) to `CoptStartcomm` verbatim runs
into a problem `CoptSendrecv` never has: `CoptStartcomm` has a *persistent*
side effect — the periodic tester-present message, which keeps running via
`PassThruStartPeriodicMsg` long after the COP itself finishes. If the
tester-present message were built from a borrowed Working set the same way
the init frame is, a `temp_param_update` `CoptStartcomm` would leave an
ongoing periodic send effectively "promoting" Working's values for the rest
of the CLL's lifetime (or until `CoptStopcomm`) — violating ADR-063's
"Working is never promoted" invariant, just applied to a side effect that
outlives the COP rather than the Active ComParam snapshot itself.

`CoptStopcomm` was also named as out of scope by ADR-044 ("`CoptStartcomm`,
`CoptStopcomm`, and `CoptDelay` also do not call `set_config`"), which
remains true after this ADR: `handle_stop_comm` reads no ComParams and
writes no hardware config regardless of `temp_param_update`, so the flag has
nothing to affect there.

## Decision

`CoptStartcomm`'s ComParam resolution is deferred to the poll task the same
way ADR-063/064 did for `CoptSendrecv`. `TxItem::StartComm` is reshaped to
carry only ComParam-independent fields — `cop_handle`, `cll_handle`,
`protocol_id` (the hardware protocol id), `base_tx_flags` (the client's raw
`ComPrimitiveCtrlData.tx_flag` bits), `cop_data` (the raw init payload), the
UniqueRespIdTable `entries` snapshot (still taken at RPC time — like
`SendRecvTx::Deferred`, the table is not part of Active/Working, so an
RPC-time snapshot is correct here, ADR-064), and `source: ComParamSource`
(`Working` when `temp_param_update` is set, else `Active`). Every
ComParam-dependent element — tester-present message content/interval/
TxFlags/addressing, ISO-TP framing, fast-init frame TxFlags — is resolved by
`handle_start_comm` (`events.rs`) at execution time, from whichever set
`source` names, mirroring `handle_send_recv`'s resolution of a
`SendRecvTx::Deferred` item. This makes a `CoptUpdateparam` queued
immediately ahead of a `CoptStartcomm` on the same CLL visible to it, by
construction (single FIFO poll task per physical channel).

Resolution is split into two independent helpers in `rpc_primitive.rs`,
reflecting that the transient init transaction and the persistent
tester-present side effect must read *different* ComParam sets:

- `resolve_init_tx_flags(params, entries, base_tx_flags) -> u32` resolves the
  K-line fast-init wakeup frame's TxFlags from `params` — Active ordinarily,
  or Working for the duration of a `temp_param_update` transaction. This is
  scoped entirely to the init step and never outlives it.
- `resolve_tester_present(protocol, active, entries, software_isotp,
  base_tx_flags) -> Result<ResolvedTesterPresent, String>` resolves the
  tester-present message/interval/TxFlags/ISO-TP framing **always from the
  live Active set**, regardless of `source` — the periodic message is a
  persistent product of the COP, not part of the transient init transaction,
  so it must never be built from a set that gets reverted before the COP
  even finishes.

`handle_start_comm`'s sequence, for `source == Working`
(`temp_param_update` set):

1. Resolve `init_tx_flags` from Working, and `resolve_tester_present` from
   the live Active set (both read once, at the top of this dispatch — no
   other item for this CLL can run concurrently to mutate state in between).
   A `resolve_tester_present` failure (e.g. a missing addressing ComParam for
   a non-empty tester-present payload) ends the whole COP immediately with an
   execution-time `PduErrEvtTesterPresentError` + `PduCopstFinished`, not a
   synchronous `StartComPrimitive` `INVALID_ARGUMENT` (ADR-063/064's
   trade-off, extended here) — no init has run and `comm_started` is
   untouched.
2. Push Working to hardware via `apply_params_to_hardware`. If this
   `SET_CONFIG` call itself fails, revert to Active immediately and fail the
   whole COP (`PduErrEvtProtErr` + `PduCopstFinished`) — do **not** silently
   proceed with a possibly half-applied Working set.
3. Run the protocol init sequence (5-baud/fast-init) using `init_tx_flags`.
   On failure, revert hardware to the live Active set **before** emitting
   `PduErrEvtInitError` + `PduCopstFinished` — the previous code's init
   failure path had no revert at all, which this ADR fixes as part of
   bracketing the transaction correctly.
4. Revert hardware to the live Active set (read fresh, not the snapshot
   `resolve_tester_present` used — reverting means "whatever is actually
   Active right now," the same rule `handle_send_recv`'s
   `temp_param_update` revert already follows).
5. Only now build and start the periodic tester-present, from the
   already-resolved (Active-derived) message — never from Working, and never
   before the revert above completes.
6. Update `comm_started`/`tester_present_periodic_id` and emit
   `PduCllstCommStarted` + `PduCopstFinished`, as before.

For `source == Active` (`temp_param_update` unset), steps 2/4 (the
hardware apply/revert) are skipped entirely — behavior is unchanged except
that resolution is now live rather than snapshotted (ADR-063/064's fix,
extended to `CoptStartcomm`).

`CoptStopcomm` is extended to accept `temp_param_update` as a pure no-op:
`rpc_start_com_primitive` no longer needs any special-casing for it (it never
inspected the flag before, and `handle_stop_comm` reads no ComParams and
calls no `SET_CONFIG` regardless), but the ADR-044 lock gate (below) must not
be extended to it, since it performs no hardware write for the flag to
protect.

**ADR-044 lock gate extended to `CoptStartcomm`:** `rpc_start_com_primitive`'s
`writes_physical_com_params` check (originally `CoptUpdateparam`, or
`CoptSendrecv` with `temp_param_update`) now also covers `CoptStartcomm` with
`temp_param_update` set — it performs the identical `apply_params_to_hardware`
`SET_CONFIG` push for its init transaction, so it is rejected the same way if
another CLL sharing the physical resource holds `LOCK_PHYSICAL_COM_PARAMS`.
`CoptStopcomm` is deliberately **not** added to this gate: it writes nothing
regardless of the flag.

## Consequences

- A `CoptUpdateparam` queued immediately ahead of a `CoptStartcomm` on the
  same CLL is now reflected by it, closing the same class of race
  ADR-063/064 closed for `CoptSendrecv` — by construction (single FIFO poll
  task), not by scheduling luck.
- A `temp_param_update` `CoptStartcomm` can stage a one-off physical
  ComParam set (e.g. different K-line timing) for its init sequence without
  ever calling `CoptUpdateparam`, matching ISO 22900-2 §9.4.3's general
  intent — while the periodic tester-present that follows is always built
  from Active, so the transient borrow can never leak into an ongoing
  side effect. Working itself is never written by this path (ADR-063: the
  caller's staged Working params survive, and are found unchanged by a later
  `CoptUpdateparam`).
- Both the temp-apply-failure and the init-failure paths now revert hardware
  to Active before failing the COP — the pre-ADR-066 init-failure path had no
  revert at all, which was only latent because `temp_param_update` was not
  previously accepted for `CoptStartcomm`.
- `StartComPrimitive(COPT_STARTCOMM)` with `temp_param_update` set can now be
  rejected by `LOCK_PHYSICAL_COM_PARAMS`, in addition to the existing
  `LOCK_PHYSICAL_TX_QUEUE` check; `CoptStopcomm` is unaffected by either lock
  change here.
- **Trade-off, matching ADR-063/064's for `CoptSendrecv`:** a
  `CoptStartcomm`'s tester-present resolution failure (e.g. a missing
  addressing ComParam) is no longer a synchronous `StartComPrimitive`
  `INVALID_ARGUMENT` — it surfaces as an execution-time
  `PduErrEvtTesterPresentError` + `PduCopstFinished` instead, since there is
  no ComParam state to validate against until the COP's turn in the FIFO
  queue comes up.
- `tests/grpc_mock/startcomm_comparam.rs` pins the new behavior: a
  `CoptUpdateparam` queued immediately before a `CoptStartcomm` (no sleep) is
  reflected in its tester-present resolution
  (`iso15765_queued_updateparam_is_reflected_by_startcomm_tester_present`); a
  `temp_param_update=1` `CoptStartcomm` pushes exactly two `SET_CONFIG`
  batches (apply Working, then revert Active) bracketing its init step,
  leaves hardware on Active afterward, still starts the (Active-derived)
  periodic tester-present, and leaves Working undisturbed for a later
  `CoptUpdateparam` to promote
  (`iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts`);
  the ADR-044 lock extension
  (`start_com_primitive_startcomm_temp_param_update_respects_physical_com_param_lock`);
  and `CoptStopcomm`'s no-op acceptance of the flag
  (`stopcomm_temp_param_update_is_accepted_as_a_no_op`).

# ADR-063: `temp_param_update` Resolves the Live Working Set in the Poll Task, Not a Snapshot Taken at `StartComPrimitive` Time

**Date:** 2026-07-06
**Status:** Superseded by ADR-067
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`,
             `CoptSendrecv` branch, `resolve_send_recv_tx`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`),
             `j2534-0404-service/src/service.rs` (`SendRecvTx`, `TxItem::SendRecv`)

## Context

`ComPrimitiveCtrlData.temp_param_update` (ISO 22900-2 §9.4.3) is documented
(ADR-044, `rpc-api-guide.md`) as: apply the Working ComParam set for this one
`CoptSendrecv`, as if it were Active, then revert — without ever calling
`CoptUpdateparam` or permanently promoting Working to Active.

The implementation only did half of this. `rpc_start_com_primitive`
(`rpc_primitive.rs`) built `data` (the ID/header-prefixed
`PassThruMessage.Data`), `hw_tx_flags`, and the ISO-TP framing/timeout
options once, synchronously, before the COP was even enqueued — and it
always read `link.active` for this, regardless of `temp_param_update`.
Separately, `handle_send_recv` (`events.rs`) read `link.working` (live, at
cycle time) and pushed it to hardware via `apply_params_to_hardware`, but
that function only forwards ComParam IDs with a J2534 `SET_CONFIG`
equivalent (`ComParamId::to_j2534_config_id`) — every service-level
ComParam ID (0x8000–0x80FF) unconditionally returns `None` there and is
skipped.

`CP_RequestAddrMode` (0x8078, ADR-054) is exactly such a service-level
ComParam: it has no J2534 hardware config parameter at all, it only selects
which CAN ID `tx_header.rs` builds into the message. The consequence: a
client that used `SetComParam(CP_RequestAddrMode, 2)` (and
`CP_CanFuncReqId`) to stage a one-off switch to functional addressing,
intending `temp_param_update` to apply it for a single `CoptSendrecv`
without promoting Working to Active, got no effect at all — the message was
still built from Active's (unstaged) addressing mode. The same gap applied
to every other service-level ComParam this service reads for TX
construction (`CP_CanFuncReqId`/`Format`/`ExtAddr`,
`CP_PhysReqFormatPriorityType`/`CP_FuncReqFormatPriorityType`,
`CP_PhysReqTargetAddr`/`CP_FuncReqTargetAddr`,
`CP_SCITransmitMode`/`CP_SCISetProgVoltage` TxFlags, ISO-TP framing type /
`N_Bs` timeout) and, in the receive phase, `CP_P2Max` and the RC21/23/78
auto-handling ComParams.

### A snapshot-at-submission-time fix has its own race

A first fix resolved an `effective` ComParamSet once in
`rpc_start_com_primitive` — `link.working.clone()` when `temp_param_update`
was set, else `link.active.clone()` — used for every ComParam-dependent
decision, snapshotted at `StartComPrimitive` call time.

This regressed a correctness property the *previous* code had by accident:
the pre-existing hardware `SET_CONFIG` push already read `link.working`
live, from inside the poll task, at the moment this COP's cycle actually
ran — which, being strictly FIFO with every other queued item on the same
CLL, naturally reflected the effect of any `CoptRestoreParam` (Active →
Working, no hardware call) already dequeued ahead of it. Freezing the
snapshot at `StartComPrimitive` submission time breaks this: `StartComPrimitive`
returns as soon as its `TxItem` is enqueued onto the channel, not after the
poll task actually finishes processing it. A client that issues
`CoptRestoreParam` and then, immediately after, `CoptSendrecv` with
`temp_param_update` set, has no guarantee the poll task has dequeued and
applied the `CoptRestoreParam` yet by the time the `CoptSendrecv` RPC
handler reads `link.working` — the resolved addressing could reflect the
stale, not-yet-reverted Working set.

## Decision

Resolution of everything `temp_param_update` borrows from Working —
addressing, message construction, size validation, TxFlags, ISO-TP framing
— is deferred to the poll task itself, and reads the *live* Working set at
the moment this COP's first cycle actually runs there, not a snapshot taken
when `StartComPrimitive` was called. Since the poll task processes every
`TxItem` for a CLL strictly in FIFO order on one logical thread, this
guarantees — by construction, not by scheduling luck — that any
`CoptRestoreParam`/`CoptUpdateparam` already queued ahead of this COP has
already taken effect.

The addressing/message-construction/validation logic (previously inline in
`rpc_start_com_primitive`) is extracted into
`rpc_primitive::resolve_send_recv_tx(protocol, hw_protocol_id, params,
entries, cop_data, software_isotp, base_tx_flags) -> Result<ResolvedSendRecvTx,
String>`. When `temp_param_update` is set, `rpc_start_com_primitive` does
*no* ComParam-dependent resolution at all — it enqueues a
`SendRecvTx::Deferred { cop_data, entries, base_tx_flags, source:
ComParamSource::Working }`, just the raw inputs `resolve_send_recv_tx`
needs, none of which depend on Working/Active. `handle_send_recv`
(`events.rs`), on this COP's first cycle (`is_continuation == false`), reads
the live `l.working`/`l.protocol`/`l.software_isotp` and calls
`resolve_send_recv_tx` itself. A resolution failure here — a missing
addressing ComParam, a payload outside the valid TX size range, an
ISO15765-2 functional-addressing Single Frame violation (ADR-055) — is
reported as an execution-time `PduErrEvtFrameStruct` error event followed by
`PduCopstFinished`, not a synchronous `INVALID_ARGUMENT`, since there is no
ComParam state to validate against until this COP's turn in the FIFO queue
comes up.

> **Superseded in part by ADR-064**: at the time of this decision,
> `temp_param_update` unset still resolved eagerly from the Active set in
> `rpc_start_com_primitive`, synchronously, before the COP was enqueued — the
> same race this ADR fixes for Working, just not yet recognized as also
> applying to Active (via a queued `CoptUpdateparam`). ADR-064 extends the
> `Deferred`/`source` mechanism below to every `CoptSendrecv`, eager or not;
> `SendRecvTx::DeferToWorking` was accordingly generalized to
> `SendRecvTx::Deferred { ..., source: ComParamSource }` (`Active` or
> `Working`). See ADR-064 for the current, complete picture.

Resolution happens exactly once per COP — on the first cycle, never per
subsequent cycle: `SendRecvTx::Resolved` (the outcome of resolving a
`Deferred` item) is what every cycle of a cyclic send actually uses, and is
what `CycleContinuation` carries forward for later cycles. A `SetComParam`
call made after this COP's first cycle has already resolved does not
retroactively affect its later cycles; a new `CoptSendrecv` is required to
pick it up.

`SendRecvTx::Resolved` also carries `temp_params: Option<ComParamSet>` — the
exact Working set this resolution was resolved from (`Some`), used by the
poll task for the temporary hardware `SET_CONFIG` push
(`apply_params_to_hardware`) and the response-phase `CP_P2Max`
timeout/RC21/23/78 handling config (`RcHandlingConfig::from_params`); `None`
reads the live Active set for both, as for any ordinary `CoptSendrecv`.
Reverting hardware after the cycle still reads the *live* Active set (not
the resolved snapshot) — restoring "whatever is actually Active right now"
is correct regardless of what this COP borrowed. Working itself is never
written by this path (ISO 22900-2 §9.4.3: the caller's staged Working
params survive).

`CP_P3Func`/`CP_P3Phys` (ADR-060) is explicitly excluded from all of the
above: the inter-request gap protects the shared physical bus across every
CLL on it, not just this one COP's transaction, so it continues to read the
live Active set unconditionally. `CoptStartcomm` remains entirely out of
scope (ADR-044): `temp_param_update` is only defined for `PDU_COPT_SENDRECV`
(ADR-053's context), and `CoptStartcomm` still resolves addressing from
`link.active` only.

> **Extended by ADR-066**: at the time of this decision, `CoptStartcomm` was
> considered entirely out of scope for `temp_param_update`. ADR-066 extends
> the live-resolution mechanism above to `CoptStartcomm` (bounded to its
> transient init transaction; the periodic tester-present remains
> Active-derived, preserving the "Working is never promoted" invariant
> stated here) and to `CoptStopcomm` (accepted as a no-op). See ADR-066 for
> the current, complete picture.

## Consequences

- A `CoptSendrecv` with `temp_param_update` set now genuinely reflects the
  full Working set for that COP — `CP_RequestAddrMode` and every other
  service-level ComParam TX construction depends on, not only the J2534
  hardware-native ones — matching ISO 22900-2 §9.4.3's intent that
  `temp_param_update` covers the whole COP, not just the outgoing
  `SET_CONFIG` write.
- This is FIFO-consistent with any `CoptRestoreParam`/`CoptUpdateparam`
  already queued ahead of it on the same CLL, by construction (single poll
  task, strict per-channel ordering) — not dependent on how fast the poll
  task happens to drain the queue relative to gRPC round-trip timing.
- **Trade-off**: a `temp_param_update` `CoptSendrecv`'s addressing/size/
  framing validation errors are no longer synchronous `StartComPrimitive`
  `INVALID_ARGUMENT`s — they surface as an execution-time
  `PduErrEvtFrameStruct` error event plus `PduCopstFinished`. At the time of
  this decision an ordinary `CoptSendrecv` (`temp_param_update` unset) was
  unaffected and kept synchronous validation; ADR-064 extends this same
  trade-off to it too, for the matching `CoptUpdateparam` race on the Active
  side.
- `tests/grpc_mock/comparam_tx.rs::iso15765_temp_param_update_uses_working_request_addr_mode_without_updateparam`
  pins the core behavior: stages `CP_RequestAddrMode=2`/`CP_CanFuncReqId` via
  `SetComParam` alone (no `CoptUpdateparam`), confirms a plain `CoptSendrecv`
  still sends physical (Active unaffected), a `temp_param_update=1`
  `CoptSendrecv` sends functional (Working honored for this COP), and a
  following plain `CoptSendrecv` reverts to physical (Working never
  promoted or reset).
  `iso15765_temp_param_update_respects_queued_restore_param_ordering` pins
  the FIFO-ordering guarantee (queues `CoptRestoreParam` immediately
  followed, with no sleep, by a `temp_param_update` `CoptSendrecv` staged for
  functional addressing, and asserts the send is physical) — though, in this
  in-process test harness, the poll task drains the queue fast enough
  relative to a gRPC round-trip that this assertion does not empirically
  distinguish the fix from the snapshot-at-submission-time design it
  replaces; the guarantee here is structural (single FIFO queue, resolution
  reads live state from inside the same poll task that drained the earlier
  item), not something a timing-based test can reliably force either way.

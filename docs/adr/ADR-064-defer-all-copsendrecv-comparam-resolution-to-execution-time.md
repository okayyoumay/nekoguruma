# ADR-064: Defer ALL CoptSendrecv ComParam Resolution to Poll-Task Execution Time, Not Just `temp_param_update`

**Date:** 2026-07-06
**Status:** Superseded by ADR-067
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`,
             `CoptSendrecv` branch, `resolve_send_recv_tx`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`),
             `j2534-0404-service/src/service.rs` (`SendRecvTx`, `ComParamSource`)

## Context

ADR-063 fixed a race specific to `temp_param_update`: resolving the Working
set at `StartComPrimitive` call time (rather than live, in the poll task,
right before this COP's first cycle transmits) could observe a stale value
if a `CoptRestoreParam` was already queued ahead of it on the same CLL —
`StartComPrimitive` returns as soon as its `TxItem` is enqueued, not after
the poll task actually processes it.

The exact same class of race exists for an *ordinary* `CoptSendrecv`
(`temp_param_update` unset), which resolves addressing/message
construction/size validation/TxFlags/ISO-TP framing from `link.active` — a
single snapshot taken once, at the very top of `rpc_start_com_primitive`
(`self.get_link_state(handle).await?`), before any of that resolution runs.
`CoptUpdateparam` (Working → Active, plus the hardware `SET_CONFIG` push) is
the analogous queued mutator here: it also returns as soon as enqueued, not
after the poll task promotes Working to Active. A client that issues
`CoptUpdateparam` and then, immediately after, an ordinary `CoptSendrecv`,
has no guarantee the poll task has applied the `CoptUpdateparam` yet by the
time the `CoptSendrecv`'s snapshot is taken — the resolved addressing could
reflect the stale, not-yet-promoted Active set. (`CoptRestoreParam` is
irrelevant to this side: it only ever writes Working, never Active.)

Every ComParam-dependent element of `resolve_send_recv_tx` (addressing,
message construction, size validation, TxFlags, ISO-TP framing, the
underlying `PassThruMessage` construction) is subject to this race
regardless of which ComParam set it reads from. One element is not:
`cop_data` must be non-empty in software ISO-TP mode, which depends only on
`cop_data.len()` and the link's `software_isotp` flag (fixed once
connected) — never on any Active/Working ComParam value — so a queued
`CoptRestoreParam`/`CoptUpdateparam` cannot invalidate it.

## Decision

`resolve_send_recv_tx` (introduced in ADR-063) is now called from exactly
one place: `handle_send_recv`'s (`events.rs`) resolution of a
`SendRecvTx::Deferred` item, on this COP's first cycle, reading whichever
ComParam set is live at that moment. `rpc_start_com_primitive` no longer
calls it eagerly at all — the eager/Active branch ADR-063 left in place for
`temp_param_update` unset is removed.

`SendRecvTx::DeferToWorking` (ADR-063) is generalized to
`SendRecvTx::Deferred { cop_data, entries, base_tx_flags, source }`, where
`source: ComParamSource` is `Active` or `Working`. Every `CoptSendrecv`
enqueues this variant — `rpc_start_com_primitive` sets
`source = ComParamSource::Working` when `temp_param_update` is set, else
`ComParamSource::Active` — and `Resolved` is never constructed directly by
`rpc_start_com_primitive` anymore; it only ever comes from the poll task
resolving a `Deferred` item (or from a `CycleContinuation` carrying an
already-resolved cyclic send forward, unchanged, per ADR-063).

The one ComParam-independent check (`cop_data` non-empty under software
ISO-TP) is hoisted out of `resolve_send_recv_tx` and validated eagerly in
`rpc_start_com_primitive`, before enqueueing, for every `CoptSendrecv`
regardless of `temp_param_update` — there is no ComParam state involved, so
there is nothing for a queued `CoptRestoreParam`/`CoptUpdateparam` to
invalidate, and a synchronous `INVALID_ARGUMENT` remains correct and cheap
for this case.

`CP_P3Func`/`CP_P3Phys` (ADR-060) remains explicitly out of scope, as under
ADR-063: it protects the shared physical bus across every CLL, not just this
COP's transaction, and continues to read the live Active set unconditionally
regardless of `source`. `CoptStartcomm` also remains out of scope (ADR-044):
`temp_param_update` is only defined for `PDU_COPT_SENDRECV`.

> **Extended by ADR-066**: ADR-066 extends this same live-resolution
> mechanism to `CoptStartcomm`/`CoptStopcomm`, and defines `temp_param_update`
> for `CoptStartcomm` too (bounded to its transient init transaction). See
> ADR-066.

## Consequences

- Every `CoptSendrecv` — not just a `temp_param_update` one — is now
  FIFO-consistent with any `CoptUpdateparam`/`CoptRestoreParam` already
  queued ahead of it on the same CLL, by construction (single poll task,
  strict per-channel ordering), not dependent on how fast the poll task
  happens to drain the queue relative to gRPC round-trip timing.
- **Trade-off, extending ADR-063's to the common path**: addressing/size/
  framing validation errors for *every* `CoptSendrecv` are no longer
  synchronous `StartComPrimitive` `INVALID_ARGUMENT`s. They surface as an
  execution-time `PduErrEvtFrameStruct` error event plus `PduCopstFinished`
  instead — only the enum value is delivered this way, not the descriptive
  message string `resolve_send_recv_tx` previously returned synchronously
  (that string is still logged via `tracing::warn!` server-side for
  diagnostics, just not sent to the client). The `cop_data`-non-empty
  check (software ISO-TP) is the one exception and remains a synchronous
  `INVALID_ARGUMENT`, since it is ComParam-independent.
- `tests/grpc_mock/harness.rs::send_data_expect_rejected` (synchronous
  `Status` assertion) is removed — every one of its 8 call sites tested a
  ComParam-dependent failure (missing addressing, TX size range, ISO15765-2
  Single Frame limit) and is replaced by the new
  `send_data_expect_error_event` (subscribes first, asserts
  `start_com_primitive` succeeds, then waits for `PduErrEvtFrameStruct` +
  `PduCopstFinished` and that nothing reached the mock).
- `tests/grpc_mock/comparam_tx.rs::iso15765_plain_sendrecv_respects_queued_updateparam_ordering`
  pins the Active-side FIFO guarantee this ADR adds: queues
  `CoptUpdateparam` immediately followed, with no sleep, by a plain
  `CoptSendrecv` staged (via `SetComParam` alone) for functional addressing,
  and asserts the send is functional — mirroring
  `iso15765_temp_param_update_respects_queued_restore_param_ordering`
  (ADR-063) on the Working/`CoptRestoreParam` side. As with that test, the
  guarantee is structural (single FIFO queue, resolution reads live state
  from inside the same poll task that drained the earlier item); the
  in-process test harness's poll task drains the queue fast enough relative
  to a gRPC round-trip that this assertion does not reliably distinguish the
  fix from the design it replaces on timing alone.

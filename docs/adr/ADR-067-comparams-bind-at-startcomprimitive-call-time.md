# ADR-067: ComParams Bind to the ComPrimitive at StartComPrimitive Call Time (Snapshot), per ISO 22900-2

**Date:** 2026-07-07
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`ParamBinding`, `SendRecvTx`, `TxItem`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`,
             `resolve_send_recv_tx`, `resolve_tester_present`, `resolve_init_tx_flags`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`, `handle_start_comm`,
             `handle_update_param`),
             `j2534-0404-service/src/service/comparam_support.rs` (`bustype_params_differ`)

## Context

ADR-063/064/066 built an increasingly elaborate "resolve live, in the poll
task, right before this COP's turn in the FIFO queue comes up" model for
`CoptSendrecv`/`CoptStartcomm`, on the premise that a snapshot taken at
`StartComPrimitive` call time could race a `CoptUpdateparam`/`CoptRestoreParam`
already queued ahead of it on the same CLL — `StartComPrimitive` returns as
soon as its `TxItem` is enqueued, not after the poll task actually processes
it, so (the argument went) only reading the ComParam set live, from inside the
poll task itself, could guarantee FIFO-consistent behavior.

Re-reading ISO 22900-2 §9.4.3 shows this premise does not hold. A
ComPrimitive's ComParam state is meant to bind once, at the moment
`StartComPrimitive` is *called* — not whenever the poll task later happens to
get around to executing it. Under the live-resolution model, two callers
racing each other (a `SetComParam`/`CoptUpdateparam` and a `CoptSendrecv`) get
a result that depends on how fast the poll task happens to drain its queue
relative to gRPC round-trip timing for each call — exactly the kind of
non-determinism ADR-063/064/066 thought they were eliminating by moving
resolution into the poll task, but which the poll task's own execution
timing reintroduces from a different angle. Binding at call time removes this
entirely: whichever call is made first (not enqueued first, not executed
first) determines what "current" ComParam state means for that COP, for its
whole life. A `CoptUpdateparam`/`CoptRestoreParam`/`SetComParam` issued *after*
a `StartComPrimitive` call returns can never retroactively affect that
already-bound COP.

This also means ADR-064's "defer everything, including plain `CoptSendrecv`,
so resolution failures surface as execution-time error events instead of
synchronous errors" trade-off was an unnecessary cost: once resolution binds
at call time, there is no reason a resolution failure (missing addressing
ComParam, TX size out of range, ISO15765-2 Single Frame violation) cannot be
validated synchronously and returned as `INVALID_ARGUMENT`, exactly as before
ADR-064.

Separately, three further claims from a closer reading of §9.4.3's
`temp_param_update` model had not been implemented:

- **Working writeback.** After a `temp_param_update=1` COP's call, the
  caller's Working set is meant to be reset from Active — not survive
  untouched forever, as ADR-063 assumed ("the caller's staged Working params
  survive"). `temp_param_update` stages a *one-off* override, not a permanent
  fork of Working from Active.
- **BUSTYPE guard.** `temp_param_update` must not be usable to stage a
  `PDU_PC_BUSTYPE`-class ComParam change (baud rate, CAN bit timing, UART
  config, termination, network line) — these describe the physical bus
  itself, not one COP's transaction, so ISO 22900-2 defines
  `PDU_ERR_TEMPPARAM_NOT_ALLOWED` specifically for this case.
- **UPDATEPARAM's own call-time snapshot.** ADR-063/064 already moved
  `CoptSendrecv`'s Active-side resolution to read live-at-execution; the
  mirror image — `CoptUpdateparam` itself reading Working live at execution,
  rather than snapshotting it at its own call time — had the exact same
  class of race, just on the promotion side rather than the read side.

## Decision

### A/B — Call-time snapshot binding, eager resolution restored

Every `CoptSendrecv`/`CoptStartcomm`/`CoptStopcomm` call binds, synchronously
inside `rpc_start_com_primitive`, an immutable `ParamBinding`:

```rust
enum ParamBinding {
    /// temp_param_update unset: the call-time Active snapshot.
    Plain(ComParamSet),
    /// temp_param_update set: `effective` (Working at call time) drives
    /// resolution and the temporary hardware push; afterward, hardware is
    /// reverted to the LIVE Active set read at revert time (see §C).
    Temp { effective: ComParamSet },
}
```

That binding is the sole source for every ComParam-dependent resolution the
COP itself performs, for its whole life — addressing, message construction,
TX size validation, TxFlags, **TX-side** ISO-TP framing, and the
response-phase `CP_P2Max`/RC21/23/78 config — including every cycle of a
cyclic `CoptSendrecv` (see the RX-side exception below: this scoping does
not extend to software ISO-TP's channel-wide receive FlowControl
configuration, which is not owned by any one COP). This
reverts ADR-064's deferral: `resolve_send_recv_tx`/`resolve_tester_present`/
`resolve_init_tx_flags` are called eagerly, synchronously, from
`rpc_start_com_primitive` itself, against the bound snapshot, and a
resolution failure is once again a synchronous `StartComPrimitive`
`INVALID_ARGUMENT` (the descriptive error string, not just the enum value,
reaches the client again). `TxItem::SendRecv`/`TxItem::StartComm` carry
already-resolved data (`SendRecvTx`, `rpc_primitive::ResolvedTesterPresent`,
`init_tx_flags`) plus the bound `ParamBinding`; the poll task performs no
ComParam resolution of its own. `ComParamSource` and `SendRecvTx::Deferred`
(and the equivalent live-resolution path ADR-066 added to `handle_start_comm`)
are deleted.

**Exceptions, both pre-existing and both scoped to the physical channel
rather than to one COP:**

- `CP_P3Func`/`CP_P3Phys`'s inter-request gap (unchanged since ADR-060)
  protects the shared physical bus across every CLL on it, not just this one
  COP's transaction, and continues to read the live Active set
  unconditionally (`wait_for_p3_gap`).
- Software ISO-TP's **receive-side** FlowControl configuration
  (`block_size`/`st_min`/`framing`/`n_cr`, `events.rs`'s
  `build_cll_rx_entries`) is channel-scoped, not per-COP: it governs how
  this CLL answers a FirstFrame arriving *at any time* on its raw-CAN
  channel, independent of which (if any) COP is currently running, and reads
  the live Active set for every inbound frame, as it did before this ADR.
  This is a pre-existing, separate design (RX has no "call" to bind a
  snapshot at), not something ADR-067 changes. Only the **TX-side** ISO-TP
  framing/addressing a `CoptSendrecv`/tester-present message itself uses
  (`SoftIsoTpTx`/`SoftIsoTpFraming`, built by `resolve_send_recv_tx`/
  `resolve_tester_present`) is part of the call-time-bound snapshot.

The `bound_comparams` snapshot itself (`working`/`active` clones) is taken
in a single `logical_links` lock acquisition inside `rpc_start_com_primitive`
— early, before any lock check, `cop_handle` allocation, or enqueue — and
threaded unchanged through the BUSTYPE guard (§E) and the `ParamBinding`
construction below. This matters: a version that re-acquired the lock a
second time later (e.g. to build the `ParamBinding` after the guard had
already passed) would leave a TOCTOU window in which a concurrent
`SetComParam` could mutate `working` between the two reads — letting a temp
COP bind a value the guard never actually saw. Reading once and reusing the
clones closes that window structurally, the same way binding at call time
closes the FIFO race ADR-063/064/066 were trying (and failing) to close.

### C — temp scope is this COP only (hardware)

For a `Temp` binding, execution applies `effective` to hardware, runs the
transaction, then reverts hardware — finally-style, on every path including
a failed hardware apply and a failed TX/init. For `CoptStartcomm` this
brackets the init transaction exactly once (apply, init, revert). For
`CoptSendrecv` it brackets **each cycle** of a cyclic send individually:
`effective` is (re-)applied before every cycle's transmit and the revert is
(re-)applied after every cycle's transmit, so hardware sits at Active (not
the borrowed Working values) between cycles, not just after the COP's last
one.

The revert targets the **live Active set, read at revert time in the poll
task** (`revert_hardware_to_live_active`) — NOT an Active snapshot bound at
call time. The revert target is a *restoration duty* ("leave the link in
whatever is Active now"), not a ComParam this COP consumes, so §A's
call-time binding does not apply to it. A call-time Active snapshot here
would be a real bug: a `CoptUpdateparam` already queued ahead of the temp
COP (called before it, executed after the temp COP's call), or interleaved
between cycles of a cyclic temp send, updates hardware and
`LogicalLinkState::active` to new values first — a stale-snapshot revert
would then roll hardware back to the pre-update params while the next plain
COP resolves from the new Active buffer, leaving the two silently diverged.
This preserves ADR-063/066's "restore whatever is actually Active right
now" revert rationale, which survives the switch to call-time binding
unchanged. `apply_params_to_hardware`'s `DATA_RATE` skip (ADR-011) is
unchanged. The Active *buffer* (`LogicalLinkState::active`) is never
overwritten by a temp COP.

`CoptStartcomm`'s periodic tester-present is resolved from the call-time
**Active** snapshot unconditionally — never Working, even when
`temp_param_update` is set — since the periodic message is a persistent
product of the COP that outlives its transient init transaction (claim 8).
It is started after the post-init revert, from the carried
`ResolvedTesterPresent`, same as before ADR-067 except the resolution itself
now happened at call time rather than execution time.

> **Amended (not superseded) by ADR-110**: both the apply (`effective`) and
> the revert (the live Active set `revert_hardware_to_live_active` reads)
> are now passed through `comparam_support::strip_bustype_keys` before ever
> reaching hardware — a `PDU_PC_BUSTYPE`-class ComParam can never be changed
> via `TempParamUpdate` at all (ISO 22900-2 §9.4.16.2.1 c) NOTE 2 / d)),
> because this CLL's own Active (the basis for both the apply and the
> revert described in this section) has no cross-CLL synchronization and can
> already be stale relative to another CLL's real, currently-pushed hardware
> value, independent of whether any lock is held. This closes a confirmed
> regression in ADR-110's first cut, where this section's mechanics alone
> (with no BUSTYPE stripping) allowed a stale `effective`/live-Active read to
> silently clobber another CLL's real hardware value. §E's
> `PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard is unaffected and remains the sole
> call-time physical-ComParam signal for `temp_param_update` (see the
> amendment note on §H below) — the strip described here is a structural
> safety net underneath that guard, not a replacement for it.

### D — Working writeback

For `temp_param_update=1` on any of the three COP types (including
`CoptStopcomm`, which touches no hardware at all), `rpc_start_com_primitive`
sets `link.working = link.active.clone()` right before returning — after
the BUSTYPE guard and lock checks have passed and the COP has been
successfully enqueued, but before the poll task has necessarily executed
anything. This flips ADR-063's "Working is never reset" invariant: the
caller's staged Working values are consumed by this one COP and then
discarded, matching ISO 22900-2's "temp_param_update stages a one-off
override" model rather than "permanently forks Working from Active." One
consequence worth being explicit about: a `SetComParam` that lands *between*
the call-time snapshot (`bound_comparams`, taken early in the same
`rpc_start_com_primitive` invocation) and this writeback — i.e. anywhere
within the same `StartComPrimitive` handler's execution, since both belong
to one gRPC call — is itself discarded by the writeback, not merely ignored
by the COP's own resolution. This is consistent with, not an added edge
case to, the one-off-override-consumed intent: the whole handler invocation
is the "call," and nothing that happens strictly inside it is meant to
survive past the writeback.

### E — BUSTYPE guard

At `temp_param_update=1` `StartComPrimitive` time, for all three COP types,
if Working differs from Active on any ComParam of class `PDU_PC_BUSTYPE`,
`rpc_start_com_primitive` returns `PDU_ERR_TEMPPARAM_NOT_ALLOWED`
(`Status::failed_precondition`, with that string in the message, matching
this codebase's existing convention of returning a descriptive `tonic::Status`
rather than a distinct wire encoding for D-PDU `PduError` codes — no other
RPC in this service currently surfaces a `PduError` value as anything other
than a `Status` code + message) synchronously: no enqueue, no lock check, no
writeback, no side effects. This check is independent of, and runs *before*,
the ADR-044 `LOCK_PHYSICAL_COM_PARAMS` check — the two are unrelated (one
CLL's own attempt to stage a bus-physical change vs. a conflict with another
CLL's lock).

`comparam_support::bustype_params_differ` compares two small lists
(`BUSTYPE_UNUM32`/`BUSTYPE_BYTES`) against Working/Active. The list is drawn
directly from `comparam-protocol-support.md`'s existing "Physical Layer
ComParams (BUSTYPE class)" table (already documenting ISO 22900-3's class
assignment for every physical-layer ComParam this service supports):
`CP_Baudrate`, `CP_BitSamplePoint`(`_Ecu`), `CP_SamplesPerBit`(`_Ecu`),
`CP_SyncJumpWidth`(`_Ecu`), `CP_ListenOnly`, `CP_TerminationType`(`_Ecu`),
`CP_NetworkLine`, `CP_K_L_LineInit`, `CP_K_LinePullup`, `CP_UartConfig`
(mapped to `DATA_BITS`), `CP_CANFDBaudrate`/`CP_CANFDBitSamplePoint`/
`CP_CANFDSyncJumpWidth`, `CP_J1850IFRCtrl`, and (bytefield) `CP_CanBaudrateRecord`.
`CP_Parity` (`ComParamId(j2534_0404::PARITY)`) was, as of this ADR, deliberately
**excluded**: it is a J2534-specific alias with no distinct D-PDU `CP_*` name
of its own (D-PDU folds parity into `CP_UartConfig`, which this service maps
to `DATA_BITS` alone) — its BUSTYPE membership was judged genuinely
ambiguous, so it was classified conservatively (excluded, so a Working/Active
difference on `CP_Parity` alone never tripped the guard).

> **Superseded by ADR-110's Codex-review amendment (PR #116):** the
> "conservatively excluded" decision above is wrong and has been reversed —
> `CP_Parity` is now **included** in `BUSTYPE_UNUM32`. A Codex review found
> that `apply_bustype_lock`'s `hw_set` exclusion and `strip_bustype_keys`
> (both ADR-110) reuse this same list for a different purpose than this
> section's own guard: not "is this ISO `PDU_PC_BUSTYPE`-labeled ComParam,"
> but "does this ComParam reach a native `PassThruIoctl SET_CONFIG` write
> that another CLL's lock or a temp bracket must never touch." By that
> measure `CP_Parity` is unambiguous — `expand_uart_config` always forwards
> an explicit `PARITY` entry to hardware, and it wins over any
> `CP_UartConfig`-derived value — so excluding it left a hole where a
> non-owning CLL (or a `temp_param_update` call) could smuggle a physical
> UART-parity change past both guards. The list is one shared classification
> keyed by physical hardware effect, not two independently-maintained ones —
> see ADR-110's amendment section. A side effect, intended and spec-correct
> per ISO 22900-2 §9.4.16.2.1 c) NOTE 2, not a regression: this section's own
> `bustype_params_differ`/`PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard now also
> rejects a `temp_param_update` call that stages a `CP_Parity` difference
> from Active.

### F — UPDATEPARAM call-time snapshot

`rpc_start_com_primitive`'s `CoptUpdateparam` branch captures
`link.working.clone()` at call time into `TxItem::UpdateParam { params }`.
`handle_update_param` applies `params` (not a live re-read of Working) to
hardware and, only on success, sets `link.active = params` — the *timing* of
the Active promotion (at execution, on hardware success) is unchanged from
before this ADR; only the Working *read* moved to call time. A `SetComParam`
issued after the `CoptUpdateparam` call must not be promoted by it.
`CoptRestoreParam` is unchanged (Active → Working, live, at execution — it
never races anything since it only ever reads/writes in one direction and
performs no hardware call).

### G — UniqueRespIdTable: unchanged

The table snapshot for `CoptSendrecv`/`CoptStartcomm` is still taken at
`StartComPrimitive` call time (as it already was under ADR-064/066 — the
table was never part of Active/Working). No Working/Active-style buffering
is introduced for it by this ADR; that remains explicitly out of scope.

> **Resolved by ADR-068**: the UniqueRespIdTable gets its own Working/Active
> split. The call-time-snapshot *timing* described here is unchanged — a COP
> still binds the table at `StartComPrimitive` call time — but the snapshot
> now reads a real `active_unique_resp_id_table` field, and (the one
> deliberate divergence from this ADR's `ParamBinding` model) it does so
> **unconditionally**, even when `temp_param_update=1`: there is no
> Working-side table for a temp COP to borrow, unlike `ParamBinding::Temp`'s
> `effective` ComParamSet.

### H — Locks (ADR-044) unchanged in scope

`temp_param_update=1` `CoptSendrecv`/`CoptStartcomm` and `CoptUpdateparam`
remain gated by `LOCK_PHYSICAL_COM_PARAMS`; `CoptStopcomm` remains ungated
(it performs no hardware writes even with the flag set). The BUSTYPE guard
(§E) is a new, separate, prior check — see the amendment note added to
ADR-044.

> **Amended (not superseded) by ADR-110**: ADR-110 removes ADR-044's separate,
> synchronous `LOCK_PHYSICAL_COM_PARAMS` check that used to sit alongside this
> section's own `temp_param_update` guard. §E's `PDU_ERR_TEMPPARAM_NOT_ALLOWED`
> guard itself is unchanged and remains the sole call-time physical-ComParam
> signal for `temp_param_update` — but the lock check is not removed because
> it became redundant with §E (an earlier draft of ADR-110 reasoned this way;
> `edge-case-hunter` confirmed that reasoning does not hold, since §E only
> compares this CLL's own Working/Active, which says nothing about whether
> this CLL's own Active still matches the real hardware state another CLL
> may be protecting). It is removed because a lock check was never the right
> tool for the risk `temp_param_update` actually poses: see §C's own
> amendment note above for the structural fix (`strip_bustype_keys`) that
> replaces it, and ADR-110 for `CoptUpdateparam`'s separate, execution-time
> resolution.

## Consequences

- `ComParamSource`, `SendRecvTx::Deferred`/`Resolved` are deleted;
  `SendRecvTx` is now a plain, already-resolved struct, and `ParamBinding`
  replaces the source-tag/temp_params-option pattern ADR-063 introduced.
  `TxItem::StartComm` drops its `entries`/`base_tx_flags`/`source` fields in
  favor of already-resolved `tester_present`/`init_tx_flags`/`binding`.
  `TxItem::UpdateParam` gains `params: ComParamSet`.
- Resolution failures for `CoptSendrecv`/`CoptStartcomm` (addressing, TX size,
  ISO15765-2 Single Frame, tester-present) are synchronous
  `StartComPrimitive` `INVALID_ARGUMENT`s again — ADR-064/066's
  execution-time-error-event trade-off is gone. `tests/grpc_mock/harness.rs`
  regains `send_data_expect_rejected` (removed by ADR-064);
  `send_data_expect_error_event` is removed.
- `tests/grpc_mock/comparam_tx.rs::iso15765_temp_param_update_uses_working_request_addr_mode_without_updateparam`:
  the temp COP still sends functional (Working honored for that one send),
  but Working now equals Active immediately afterward (writeback) — the
  "Working is never reset" assertion is inverted.
  `iso15765_temp_param_update_respects_queued_restore_param_ordering` and
  `iso15765_plain_sendrecv_respects_queued_updateparam_ordering` (ADR-064) no
  longer have a structural FIFO guarantee to pin (call order, not enqueue
  order, now determines the outcome) — both now explicitly wait for the
  earlier mutating COP's `PduCopstFinished` before issuing the COP under
  test, and document the call-order rationale.
- `tests/grpc_mock/startcomm_comparam.rs::iso15765_queued_updateparam_is_reflected_by_startcomm_tester_present`
  similarly now waits for `CoptUpdateparam`'s `PduCopstFinished` before
  calling `CoptStartcomm`.
  `iso9141_temp_param_update_startcomm_borrows_working_for_init_then_reverts`
  keeps its borrow/revert assertions, but its final leg now asserts Working
  writeback (Working == Active immediately after the temp call, so a
  subsequent `CoptUpdateparam` is a no-op) instead of "Working survives."
- Two existing `LOCK_PHYSICAL_COM_PARAMS` lock tests
  (`locks_and_param_classes.rs`/`startcomm_comparam.rs`) needed an extra
  `CoptUpdateparam` step for their second CLL: a CLL that *joins* an
  already-open physical channel keeps a default (empty) Active set until it
  issues `CoptUpdateparam` (a pre-existing, separately-documented quirk —
  ADR-060's Consequences) — without syncing it first, the BUSTYPE guard
  itself (comparing empty Active vs. the joining CLL's populated Working,
  e.g. `CP_Baudrate`) rejected the call before the test ever reached the
  lock check it meant to verify.
- New tests (`tests/grpc_mock/param_binding.rs`): claim 9 pinning (a
  `CoptDelay` holds the FIFO open across every RPC call, so a `SetComParam`
  issued after a `temp_param_update` `CoptSendrecv`'s call — while that send
  is still queued behind the Delay — has no effect on it); claim 4 pinning
  (same shape, for `CoptUpdateparam`); the BUSTYPE guard's synchronous
  rejection and no-side-effects behavior for `CoptSendrecv`/`CoptStartcomm`;
  and `CoptStopcomm`'s writeback.
- `docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`,
  `docs/glossary.md`, `j2534-0404-service/docs/implementation-notes.md`, and
  `j2534-0404-service/docs/comparam-protocol-support.md` are updated
  alongside this ADR to describe call-time binding instead of live
  resolution.

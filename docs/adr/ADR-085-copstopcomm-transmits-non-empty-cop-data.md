# ADR-085: CoptStopcomm Non-Empty cop_data Transmits a Final Fire-and-Forget Message

**Date:** 2026-07-14
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs`, `src/service/rpc_primitive.rs`,
`src/service/events.rs`

## Context

`StartComPrimitive(cop_type=COPT_STOPCOMM, cop_data=<non-empty>)` silently
discarded `cop_data`: `TxItem::StopComm` (`service.rs`) carried no data field,
`rpc_start_com_primitive`'s `CoptStopcomm` branch never read
`request.cop_data`, and `handle_stop_comm` (`events.rs`) never called
`PassThruWriteMsgs`. This is a data-loss bug for any client that uses
`CoptStopcomm` to send a final protocol message before tearing down
communication — e.g. a KWP2000 `StopCommunication` request, which ISO 22900-2
and several ECU protocols expect to be sent, not merely implied by the CLL
going back to `Online`.

The prior design scoped `CoptStopcomm` to "no hardware config write"
(adapter-design.md; ADR-044: "`CoptStartcomm`, `CoptStopcomm`, and
`CoptDelay` also do not call `set_config`"; ADR-066, superseded by ADR-067:
"`CoptStopcomm` ... writes no hardware config regardless of
`temp_param_update`, so the flag has [no effect]"). That scope is about
`PassThruIoctl SET_CONFIG` — ComParam writes — and remains entirely correct;
it never addressed bus transmission of `cop_data` at all, which was simply
being dropped rather than deliberately excluded. This ADR closes that gap
without touching the ComParam-write scope those prior decisions established.

## Decision

Non-empty `cop_data` on `CoptStopcomm` now triggers a single fire-and-forget
bus transmit, resolved through the exact same pipeline `CoptSendrecv` already
uses (`resolve_send_recv_tx`, ADR-049/ADR-050/ADR-055), always against the
**Active** ComParam snapshot — never Working, regardless of
`temp_param_update`. `temp_param_update` remains a pure no-op for
`CoptStopcomm` with respect to hardware, unchanged from ADR-044/ADR-067 (the
Working→Active writeback on success still runs for `CoptStopcomm`, as
before; only the hardware-write scope this ADR is about is unaffected).
Empty `cop_data` is byte-for-byte the pre-ADR-085 behavior: no transmit, no
resolution, no new lock check.

### `rpc_start_com_primitive` (`rpc_primitive.rs`)

In the `CoptStopcomm` branch, `tx` is resolved BEFORE the existing
cancel-queued-primitives step (Codex-review fix — an earlier draft resolved
it after, so a synchronously-rejected call, e.g. oversized `cop_data`, still
marked every other queued COP on the link cancelled even though the rejected
call never enqueued a `TxItem::StopComm` and so never actually superseded
them; regression test:
`stopcomm_rejected_oversized_cop_data_does_not_cancel_other_queued_cops`):

- `request.cop_data.is_empty()` → `tx: None`.
- Otherwise, resolve `tx_flags` from `request.cop_ctrl_data` (same as
  `CoptSendrecv`), take the call-time-bound `bound_active` snapshot (the
  same single-critical-section snapshot `CoptSendrecv`/`CoptStartcomm` bind,
  ADR-067) — **not** `bound_working`, unconditionally — and call
  `resolve_send_recv_tx` against it and `bound_active_table`. A resolution
  error (missing addressing ComParam, SAE J2534-1 TX size range violation
  ADR-049, ISO15765-2 functional Single Frame limit ADR-055) is a synchronous
  `INVALID_ARGUMENT`, and the COP is never enqueued (`self.primitives` entry
  removed) and no sibling COP on the link is cancelled — identical contract
  to `CoptSendrecv`'s own resolution failure.
- On success, `Some(SendRecvTx { .. })` is carried on the enqueued
  `TxItem::StopComm`, and only then are other queued COPs on the link marked
  cancelled.

The existing `LOCK_PHYSICAL_TX_QUEUE` check (previously gated on
`CoptSendrecv`/`CoptStartcomm` only) now also applies to `CoptStopcomm` when
`cop_data` is non-empty — it is about to actively transmit on the shared
physical bus, the same reason `CoptSendrecv`/`CoptStartcomm` are checked.
Empty-data `CoptStopcomm` stays exempt: it must never be blocked from
tearing down communication by another CLL's TX-queue lock, since it performs
no transmit in that case. This is a narrower gate than a blanket
`CoptStopcomm` inclusion would be — deliberately, to avoid regressing
teardown availability for the common (empty-`cop_data`) case.

The `LOCK_PHYSICAL_COM_PARAMS` check and the `writes_physical_com_params`
exclusion for `CoptStopcomm` (ADR-044/ADR-066) are unaffected: this ADR adds
no `SET_CONFIG` call anywhere, so `CoptStopcomm` — with or without
`cop_data` — still writes no hardware ComParam config and is still never
blocked by `LOCK_PHYSICAL_COM_PARAMS`.

### `TxItem::StopComm` (`service.rs`)

Gains `protocol_id: u32` and `tx: Option<SendRecvTx>`, mirroring
`TxItem::SendRecv`'s shape. `tx: None` preserves the pre-ADR-085 poll-task
behavior exactly.

### `handle_stop_comm` (`events.rs`)

Ordering, per this ADR: **(1)** stop the periodic tester-present message (as
today, unchanged) → **(2)** transmit `tx` when `Some` (new) → **(3)** clear
`comm_started` and emit `PduCllstOnline`/`PduCopstFinished` (as today,
unchanged) — the transmit step is inserted strictly between the existing
teardown step and the existing terminal-status step, never reordering either.

`comm_started` itself is cleared as the FIRST action of step 3, immediately
before the terminal status emission — not earlier, alongside the periodic-
message teardown in step 1 (Codex-review fix, P2: `rpc_get_status` and
`CoptStartcomm`'s "comm already started" precondition both read
`comm_started` live, so clearing it before step 2 let a client observe
Online-equivalent state, and even start a new `CoptStartcomm`, before the
promised final message in step 2 had actually been sent — worse the longer
step 2's `CP_P3Func`/`CP_P3Phys` gap wait or a software-ISO-TP FlowControl
wait took). The `ChannelLost`/`HardError` early-return paths below are
unaffected: `handle_channel_hard_error` clears `comm_started` for every CLL
on the channel independently, so they never reach step 3's clear regardless.

The transmit step:

- Is receive-less (`num_receive_cycles: 0` into `wait_for_p3_gap`) — a
  fire-and-forget send, matching `CoptSendrecv`'s own fire-and-forget shape
  when `NumReceiveCycles == 0` (ADR-058); `CoptStopcomm` has no
  `ComPrimitiveCtrlData.expected_response_array` to wait against in the
  first place.
- Still honors `CP_P3Func`/`CP_P3Phys` inter-request gap enforcement
  (`wait_for_p3_gap`, ADR-060), same as any other CAN-family send.
- Runs `wait_for_p3_gap` with `cancellable: false`. By the time this step
  runs, this COP's stop-comm teardown (the periodic tester-present message)
  has already stopped irreversibly — there is no "abort before commit"
  window left in which a `CancelComPrimitive` on this COP's own handle could
  meaningfully back out, even though `comm_started` itself is not cleared
  until after this step (see above). `defer_if_blocked: false`: this is a
  one-shot terminal send, not an idle-mode keepalive dispatch (`Deferred` is
  ADR-083-specific and does not apply here).
- Traced against `wait_for_p3_gap`'s actual implementation (not assumed from
  its doc comment): with `cancellable: false`, the function's wait loop
  `continue`s on every iteration instead of consulting `cancelled_cops`, so
  `P3GapOutcome::Cancelled` is genuinely unreachable from this call site —
  matched with `unreachable!`, mirroring `handle_send_recv`'s existing
  `Deferred => unreachable!("defer_if_blocked = false")` pattern.
- `P3GapOutcome::HardError` (from `wait_for_p3_gap`'s underlying `poll_rx`
  call detecting a hard channel error) has already run
  `handle_channel_hard_error` by the time it is observed here — which emits
  `PDU_ERR_EVT_LOST_COMM_TO_VCI`, `PduCopstCancelled` for every COP still in
  `primitives` on the channel (including this StopComm COP itself, since it
  is still tracked there), and `PduCllstOffline` for every affected CLL.
  `handle_stop_comm` must not emit anything further in this case — doing so
  (e.g. still emitting `PduCllstOnline`) would contradict a stronger,
  already-reported terminal state (`Offline`) with a weaker one (`Online`).
  This exactly mirrors `TxFailure::ChannelLost`'s existing contract in
  `handle_send_recv`, and the new code returns early the same way.
- `Err(TxFailure::Cancelled)` from `transmit_request` (reachable only via
  `isotp_send`'s own independent FlowControl-wait cancellation check on this
  COP's own handle, mid multi-frame send — unrelated to `wait_for_p3_gap`'s
  `cancellable` flag, which gates a completely different check inside a
  different function). **Revised by the round-6 amendment below**: this
  bullet describes the original (round-1) fall-through behavior, which the
  round-6 amendment supersedes by making this branch unreachable instead. It
  is handled differently from
  `handle_send_recv`'s contract: `handle_send_recv` emits
  `PduCopstCancelled` and skips the terminal-status step entirely, because
  for `CoptSendrecv` nothing about the CLL's state has committed yet at that
  point. For `CoptStopcomm`, the periodic-message teardown already committed
  irreversibly *before* the transmit was attempted (`comm_started` itself is
  still `true` at this point, cleared only in step 3 below) — emitting
  `PduCopstCancelled` and skipping the `Online`/`Finished` step here would
  leave the periodic message stopped with no path back to `CommStarted`,
  while the client's last-reported CLL status is still `CommStarted` and
  `comm_started` is still `true` internally — an inconsistent state the
  client has no way to reconcile. Instead, this case falls through to the
  normal `PduCllstOnline`/`PduCopstFinished` emission, the same as any other
  transmit failure this step treats as best-effort
  (`Err(TxFailure::Event(_))`, `stop_periodic_message` failure in the
  existing step immediately before this one).
- `Err(TxFailure::Event(error_event))` emits the corresponding
  `PduErrorEvent` (same as `CoptSendrecv`) and falls through to
  `PduCllstOnline`/`PduCopstFinished`, same reasoning as the `Cancelled` case
  above.
- On success, records the send in `last_func_tx`/`last_phys_tx` for the next
  `CP_P3Func`/`CP_P3Phys` gap check (`no_response_required: true`, since this
  transmit is unconditionally receive-less), the same bookkeeping
  `CoptSendrecv` performs.

### Amendment: `stop_comm_pending` guard against a second concurrent `CoptStopcomm`

The P2 Codex-review fix above (deferring `comm_started`'s clear to
immediately before the terminal `PduCllstOnline`/`PduCopstFinished`
emission, instead of alongside the periodic-message teardown) closed one
race but widened another: `comm_started` now stays `true` for the CLL's
*entire* stop-comm sequence — periodic-message teardown plus the optional
final-message transmit, which can itself wait on a `CP_P3Func`/`CP_P3Phys`
gap or a software-ISO-TP FlowControl handshake. `CoptStopcomm`'s own RPC
precondition (`rpc_start_com_primitive`'s `CoptStopcomm` branch) only
rejects when `!comm_started`, so a SECOND `CoptStopcomm` call arriving
anywhere in that now-widened window — e.g. a naive client retry — is
routinely **accepted**, not rejected. The second call's cancel-siblings step
then marks the FIRST (still-executing) StopComm's own `cop_handle` as
cancelled, which can abort its in-flight multi-frame ISO-TP transmit
(`isotp_send`'s FlowControl wait consults `cancelled_cops`); if the first
completes normally instead, a second `handle_stop_comm` runs after it in the
same FIFO poll task, producing a duplicate final message and a duplicate
`PduCllstOnline`/`PduCopstFinished` sequence.

`LogicalLinkState` gains `stop_comm_pending: bool`. `rpc_start_com_primitive`
's `CoptStopcomm` branch performs an atomic test-and-set on it inside the
SAME critical section as the existing `comm_started` TOCTOU re-check (a
single `logical_links` lock hold, via `get_mut`):

1. `!comm_started` → `failed_precondition("comm is not started; CoptStopcomm
   requires a prior successful CoptStartcomm")` (unchanged from the base
   decision above).
2. Otherwise, `stop_comm_pending == true` →
   `failed_precondition("a CoptStopcomm is already in progress for this
   ComLogicalLink")` — synchronous rejection of the second concurrent call.
3. Otherwise, set `stop_comm_pending = true` before the lock is dropped.

Both rejection paths still perform the existing `self.primitives` cleanup
AFTER dropping the `logical_links` lock, preserving the pre-existing lock
ordering (`logical_links` never held nested inside `primitives` or vice
versa in a new way).

**Rollback.** Every synchronous failure exit between the test-and-set and
the successful `tx_queue.send(TxItem::StopComm { .. })` re-acquires
`logical_links` and clears `stop_comm_pending` back to `false` before
returning — otherwise a `CoptStopcomm` that fails synchronously (e.g. the
oversized-`cop_data` `resolve_send_recv_tx` rejection, or a `tx_queue`
send failure) would permanently brick `CoptStopcomm` for that CLL, since no
future call could ever get past step 2 above. Both exits after the
test-and-set that need this:

- The `resolve_send_recv_tx` error path (`Err(err) =>` arm).
- The `tx_queue.send(TxItem::StopComm { .. })` failure path.

*Update (Codex-review fix, PR #92):* every rollback site above cleared
`stop_comm_pending` unconditionally on whatever `LogicalLinkState` was live
under `handle` at rollback time, with no check that it was still the SAME
connection this RPC call actually set the flag on. If a rollback's
`primitives.lock().await`/`logical_links.lock().await` genuinely suspends
(both are shared `Mutex`es contended by other concurrent RPC calls and the
poll task in production), a disconnect-then-reconnect of the same
`cll_handle` can advance `LogicalLinkState.connect_generation` and accept a
brand-new `CoptStopcomm` that sets its own `stop_comm_pending = true` before
the stale rollback resumes. The stale rollback then clears whatever
`stop_comm_pending` is live — the NEW call's own guard — letting a second
concurrent `CoptStopcomm` be accepted under the new generation, exactly the
duplicate-teardown race the base `stop_comm_pending` guard exists to
prevent. This is not a new reset point (the flag is not being reset at a new
lifecycle event); it closes a gap in the four rollback sites that already
existed — two pre-existing from this amendment (the `resolve_send_recv_tx`
error path and the `tx_queue.send` failure path above) and two added by
ADR-087's `num_receive_cycles` validation (`< -2` and `== -1` rejections,
see below). All four now go through one private helper,
`rollback_stop_comm_pending(handle, cop_handle, connect_generation)`
(`rpc_primitive.rs`), which still removes the `self.primitives` entry
unconditionally but clears `stop_comm_pending` ONLY when the live
`LogicalLinkState.connect_generation` still equals the `connect_generation`
this specific RPC call captured (the same value already checked earlier in
this function, per ADR-086). When it does not match, the live link belongs
to a different (reconnected) session, and whatever set its
`stop_comm_pending` — most likely a legitimate new `CoptStopcomm` under the
new generation — owns clearing it via its own normal
completion/rollback/reset-point path, unaffected by this stale rollback.

**Reset points**, mirroring `comm_started`'s own reset points exactly (each
site that clears `comm_started = false` also clears `stop_comm_pending =
false` in the same critical section):

- `handle_stop_comm`'s own normal completion (`events.rs`, immediately
  before the terminal `PduCllstOnline`/`PduCopstFinished` emission, where
  `comm_started` is cleared per the base decision above).
- `handle_channel_hard_error` (`events.rs`) — covers `handle_stop_comm`'s
  `ChannelLost`/`HardError` early-return paths, which skip the point above
  entirely.
- `rpc_disconnect_com_logical_link` (`rpc_link.rs`).
- `cancel_held_tx_items` (`events.rs`) — a fourth reset point, NOT mirroring
  `comm_started` (which this function does not touch at all): a `CoptStopcomm`
  accepted while the CLL's TX queue is suspended (`PDU_IOCTL_SUSPEND_TX_QUEUE`)
  is siphoned into `tx_held` before the poll task ever dispatches it, so it
  never reaches `handle_stop_comm`. If `PDU_IOCTL_CLEAR_TX_QUEUE` or module
  `RESET` then drains and cancels that held item via this function, `comm_started`
  correctly stays `true` (the stop genuinely never happened), but nothing else
  clears `stop_comm_pending` — every later `CoptStopcomm` on the CLL would be
  permanently rejected with "already in progress" until disconnect/hard-error
  (Codex-review fix, found one round after the base `stop_comm_pending`
  mechanism above). Fixed by scanning the drained `tx_held` items for a
  `TxItem::StopComm` and clearing `stop_comm_pending` only when one is present
  — never unconditionally, since `cancel_held_tx_items` runs for every
  suspend/clear/reset regardless of whether a StopComm was ever queued, and an
  unconditional clear would incorrectly unblock a StopComm that is not held
  but is instead genuinely still executing (dispatched, not sitting in
  `tx_held`) at the same moment.
- `should_skip_cancelled_item` (`events.rs`) — a fifth reset point, added by
  the round-4 amendment below: a `CoptStopcomm` still sitting in the poll
  task's mpsc queue (queued, not yet dispatched, and not parked in
  `tx_held`) that is explicitly cancelled via `PDU_IOCTL_CLEAR_TX_QUEUE` or
  `CancelComPrimitive` now clears `stop_comm_pending` in the same critical
  section where it is marked cancelled, for the same reason as the
  `cancel_held_tx_items` reset point above: without it, every later
  `CoptStopcomm` on the CLL would be permanently rejected with "already in
  progress".
- `rpc_cancel_com_primitive` (`rpc_primitive.rs`) — a sixth reset point,
  added by the round-5 amendment below: a `CoptStopcomm` already parked in
  `tx_held` (siphoned there by `PDU_IOCTL_SUSPEND_TX_QUEUE`) that is
  cancelled via a bare `CancelComPrimitive` — with no later resume, clear, or
  disconnect to trigger the fourth reset point (`cancel_held_tx_items`) —
  now clears `stop_comm_pending` synchronously, in the same critical section
  that extracts the item from `tx_held`, for the same "otherwise permanently
  rejected" reason as every other reset point above.

`rpc_destroy_com_logical_link` needs no corresponding change: it removes the
`LogicalLinkState` entry from the map entirely, so there is no stale flag to
reset.

### Amendment: a queued (not-yet-dispatched) `CoptStopcomm` is now fully cancelled by `CLEAR_TX_QUEUE` / `CancelComPrimitive` (round 4)

The round-3 amendment above fixed `cancel_held_tx_items`, which covers a
`CoptStopcomm` parked in `tx_held` (accepted while the TX queue was
suspended). It did not cover the more common case: a `CoptStopcomm` still
sitting in the poll task's mpsc queue — enqueued, not yet dispatched, and
never parked in `tx_held` at all. That case is handled by
`should_skip_cancelled_item` (`events.rs`), which decides whether a queued
item should be skipped (and `PduCopstCancelled` emitted) or dispatched.

`should_skip_cancelled_item` carried a pre-ADR-085 carve-out: when the item
was a `CoptStopcomm` that had been marked cancelled (`cancelled_cops`, set by
either `PDU_IOCTL_CLEAR_TX_QUEUE`'s `ioctl_clear_tx_queue` or an explicit
`CancelComPrimitive`), the function deliberately did **not** skip it — it let
`handle_stop_comm` run to completion regardless, on the theory (predating
this ADR) that `CoptStopcomm` "must execute to clear `comm_started` state"
even when cancelled. Once this ADR gave `CoptStopcomm` a non-empty `cop_data`
transmit, that carve-out meant a queued `CoptStopcomm` cancelled via
`CLEAR_TX_QUEUE` or `CancelComPrimitive` would silently transmit its payload
anyway — contradicting `CLEAR_TX_QUEUE`'s own documented "clear pending TX"
contract, and contradicting the intent of an explicit `CancelComPrimitive`
call. This is a round-4 fix on this ADR (Codex review, PR #88).

**Decision:** the carve-out is removed. A queued, explicitly-cancelled
`CoptStopcomm` is now cancelled exactly like any other COP type:
`PduCopstCancelled` is emitted, `handle_stop_comm` never runs (so its
`cop_data` payload never transmits), and `comm_started` stays `true` — a
truthful, retryable state, since the TP session genuinely never stopped.
`stop_comm_pending` is cleared in the same critical section that marks the
item cancelled (see the fifth reset point above), so a follow-up
`CoptStopcomm` is not permanently rejected with "already in progress". A COP
is either cancelled or it executes — never a partial "cancelled but still
transmits" hybrid, which is what the removed carve-out produced.

This applies uniformly to both cancellation sources, since
`PDU_IOCTL_CLEAR_TX_QUEUE` (`ioctl_clear_tx_queue`, `rpc_misc.rs`) and an
explicit `CancelComPrimitive` (`rpc_cancel_com_primitive`, `rpc_primitive.rs`)
both mark a queued COP cancelled by writing the same `LogicalLinkState`
field, `cancelled_cops`, which `should_skip_cancelled_item` reads without
distinguishing which caller set it.

An already-dispatched `CoptStopcomm` (`executing_cop`, mid-`handle_stop_comm`)
is unaffected by this change either way: `ioctl_clear_tx_queue` already
excludes `executing_cop` from the set of COPs it marks cancelled, and
`dispatch_tx_item` only consults `should_skip_cancelled_item` *before*
dispatch begins — once a `CoptStopcomm` is executing, this function is never
consulted again for it.

### Amendment: a held (`tx_held`) `CoptStopcomm` cancelled by a bare `CancelComPrimitive` now clears `stop_comm_pending` synchronously (round 5)

`rpc_cancel_com_primitive` is a pure "mark and defer" RPC: it inserts
`cop_handle` into `LogicalLinkState.cancelled_cops` and returns immediately —
the actual cancellation (skip the item, emit `PduCopstCancelled`, remove from
`primitives`) only happens later, when the poll task dequeues that item and
runs `should_skip_cancelled_item` (the fifth reset point above), or, for an
item already parked in `tx_held`, when `cancel_held_tx_items` drains
`tx_held` during `PDU_IOCTL_CLEAR_TX_QUEUE`/`RESET`/disconnect (the fourth
reset point above).

Gap: if a `CoptStopcomm` is already sitting in `tx_held` (parked there by
`PDU_IOCTL_SUSPEND_TX_QUEUE`), a bare `CancelComPrimitive` call on it only
writes `cancelled_cops` — nothing re-examines `tx_held` until a *later*
resume/clear/reset/disconnect. Until one of those occurs, `stop_comm_pending`
stays stuck `true`, permanently rejecting any retry with "a CoptStopcomm is
already in progress" — even though `GetStatus` already reports the COP
Cancelled via the `cancelled_cops` check. This is a round-5 fix on this ADR
(Codex review, PR #88).

**Decision:** `rpc_cancel_com_primitive` eagerly extracts a held
`TxItem::StopComm` matching the cancelled `cop_handle` from `tx_held`, right
there in the RPC, completing the cancellation synchronously: the item is
removed from `tx_held`, `stop_comm_pending` is cleared in the same critical
section (the sixth reset point above), `PduCopstCancelled` is emitted, and
the COP is removed from `primitives` — mirroring `should_skip_cancelled_item`'s
explicit-cancel ordering (notify first, then remove, so `GetStatus` keeps
returning `PduCopstCancelled` right up until the event is dispatched). This
is deliberately **StopComm-only**: every other held COP type keeps the
existing mark-and-defer behavior unchanged. No guard depends on their timing,
and `GetStatus` already reports them Cancelled immediately via
`cancelled_cops` membership regardless of when the deferred event actually
fires — only `CoptStopcomm` couples a link-level precondition flag
(`stop_comm_pending`) to queue residency, so only `CoptStopcomm` needs this
asymmetric eager path.

**Alternative considered and rejected:** relaxing the `stop_comm_pending`
precondition check itself — e.g. "ignore a pending StopComm that's already
cancelled in `tx_held`" instead of eagerly extracting it. Rejected because
both later processors of the *old* cancelled item
(`should_skip_cancelled_item`, `cancel_held_tx_items`) unconditionally clear
`stop_comm_pending` whenever they eventually see it. If a *new* StopComm B
were accepted while the old cancelled StopComm A still sits in `tx_held`
(under the relaxed check), A's eventual drain would clear the flag now
guarding B — recreating exactly the duplicate-teardown race the round-2
`stop_comm_pending` guard exists to prevent. Eager extraction avoids this
because A is physically gone from `tx_held` (and `stop_comm_pending` is
already `false`, ready for B) before the RPC returns — there is no longer a
stale A left for any later drain to rediscover.

### Amendment: `isotp_send`'s own FlowControl-wait cancellation check is now gated `cancellable: false` for `handle_stop_comm` too (round 6)

The round-1 design made `handle_stop_comm`'s `wait_for_p3_gap` call
`cancellable: false` ("point of no return": the periodic tester-present
teardown has already committed irreversibly by the time this step runs, so a
`CancelComPrimitive` on this COP must not be able to abort it). That covered
only the P3 gap wait. On a software-ISO-TP link with a multi-frame payload,
`transmit_request` delegates to `isotp_send`, whose FlowControl-wait loop has
its OWN, separate, unconditional cancellation check — with no `cancellable`
parameter to suppress it. A `CancelComPrimitive` on the StopComm's own
`cop_handle`, arriving while `isotp_send` was mid-way through a multi-frame
send (FirstFrame plus some but not all ConsecutiveFrames already written),
aborted the remaining frames. Per ISO 15765-2, the ECU that received an
incomplete FirstFrame sequence times out (N_Cr) and discards the WHOLE
message — this is not "partial delivery", it is "no delivery" — while
`handle_stop_comm`'s `Err(TxFailure::Cancelled)` branch (see above) silently
fell through to `PduCllstOnline`/`PduCopstFinished` as if the final message
had gone out. This is a round-6 fix on this ADR (Codex review, PR #88).

**Decision:** `cancellable: bool` is threaded through `transmit_request`,
`transmit_request_inner`, and `isotp_send` (positioned between
`count_as_bus_activity` and `ctx` in all three signatures, mirroring
`wait_for_p3_gap`'s existing parameter of the same name/semantics).
`handle_stop_comm`'s call site passes `false`; every other call site
(`handle_send_recv`'s `CoptSendrecv` write, the RC21/RC23 re-request path,
and `send_idle_tester_present_once`'s idle-mode dispatch) passes `true`,
preserving their existing cancellation contracts bit-for-bit. In
`isotp_send`, the cancellation check is skipped entirely when `cancellable`
is `false` — the `cancelled_cops` entry, if one was inserted, is left alone
and cleaned up later by the existing post-completion cleanup in
`dispatch_tx_item`, exactly mirroring how `wait_for_p3_gap`'s
`cancellable: false` path already behaves. The whole StopComm transmit step
(P3 gap wait, then the actual frame writes) is now uniformly non-cancellable
once started. `TxFailure::Cancelled` has exactly one constructor in the
codebase — inside the check just gated above — so with `cancellable: false`
at `handle_stop_comm`'s call site, that branch becomes genuinely
unreachable; it is now matched with `unreachable!(...)`, mirroring
`P3GapOutcome::Cancelled`'s existing treatment a few lines above it in the
same function.

### Amendment (round 7): `handle_stop_comm` re-validates channel identity against a concurrent `DisconnectComLogicalLink`/`DestroyComLogicalLink`

*Context:* Rounds 1-6 handled two out-of-band terminators
(`TxFailure::ChannelLost`, `P3GapOutcome::HardError`), both signaled in-band
by the transmit machinery itself. A third terminator — RPC-driven disconnect
— has no in-band signal: it clears the link state and emits
`PduCllstOffline` (ADR-019) while `handle_stop_comm` is parked in its
non-cancellable P3 wait or ISO-TP transmit, and `cancel_link_cops` emits
`PduCopstCancelled` for the still-in-`primitives` executing COP (ADR-019
§4). The unconditional terminal block then emitted
`PduCllstOnline`/`PduCopstFinished` regardless, contradicting both. This was
flagged as an unverified "round-7 candidate" during design-advisor's PR #88
review of round 6, and has now been traced, confirmed real, and fixed.

*Decision:* Port `handle_start_comm`'s existing guards (added in an earlier
round for the identical class of race on that handler): (1) a
`still_on_this_channel` check (`channel_id == Some(ctx.channel_id)`, not
`connected` — survives a same-handle reconnect onto a different channel)
re-validated after the P3 wait, before transmitting; (2) the same check
folded into the SAME `logical_links` lock acquisition as the
`comm_started`/`stop_comm_pending` clear in the terminal block. On failure
of either, a first-wins `primitives.remove(&cop_handle)` decides whether
this function emits `PduCopstCancelled` (so it doesn't duplicate one
`cancel_link_cops` already emitted), and nothing further is emitted. The
`stop_periodic_message` failure path needs no separate check: it already
falls through to the same terminal block, so guard (2) covers it too.

*Consequences:* Clients observe ADR-019's canonical forced-deactivation
sequence (`Cancelled`/`Offline`) with no trailing stale `Online`/`Finished`.
A no-I/O micro-window between the terminal check and the status emission
remains — identical to the one `handle_start_comm` already accepts for its
own `CommStarted` emission — closing it uniformly for both handlers is
deliberately deferred, not attempted here.

*Update (ADR-086):* the `still_on_this_channel` check documented here
(`channel_id == Some(ctx.channel_id)` alone) was later found insufficient on
a *shared* physical channel: a disconnect-then-reconnect of the same
`cll_handle` rejoins the same channel with the identical `ChannelId`, which
this guard alone cannot distinguish from a continuous connection. ADR-086
extends both of this amendment's guard sites (and `handle_start_comm`'s
three guards) with an additional `connect_generation` check that closes
that gap.

Two lower-severity analogous
gaps were identified but explicitly deferred (tracked as a P2 backlog item
in `j2534-0404-service/docs/implementation-notes.md`): `handle_send_recv`'s
unconditional `PduCopstFinished` after `TxFailure::Event`/at cycle
completion, and `handle_update_param`'s hardware `SET_CONFIG` + `Finished`
commit — neither contradicts a CLL-level status the way the StopComm case
did, since neither emits one.

## Alternatives Considered

1. **Wait for a response after the transmit** — Rejected: `CoptStopcomm` has
   no `ComPrimitiveCtrlData.expected_response_array` or `NumReceiveCycles` to
   define what "a response" would even mean; ISO 22900-2 gives `CoptStopcomm`
   no receive-phase semantics at all. Fire-and-forget, matching
   `CoptSendrecv`'s own `NumReceiveCycles == 0` shape, is the only
   well-defined behavior.

   *Update (ADR-087):* this rejection's premise was wrong —
   `CoptStopcomm` DOES gain a bounded, non-cancellable receive phase; see
   ADR-087.
2. **Transmit before stopping the periodic tester-present message** —
   Rejected: would risk a tester-present frame interleaving with (or
   immediately following) the client's final message on the bus, which is
   exactly the race the client is trying to avoid by sending a deliberate
   final message as part of teardown. Stopping the periodic message first
   guarantees the final transmit is the last thing this CLL puts on the bus.
3. **Resolve and transmit synchronously inside `rpc_start_com_primitive`,
   bypassing the poll task's FIFO queue** — Rejected: every other COP type
   that touches the physical bus (`CoptSendrecv`, `CoptStartcomm`) is
   dispatched through the poll task's single-threaded FIFO queue precisely so
   physical bus operations from different COPs (and different CLLs sharing a
   channel) never interleave arbitrarily. A synchronous transmit here would
   special-case `CoptStopcomm` out of that ordering guarantee for no reason;
   resolution (call-time, synchronous, ADR-067) and transmission (poll-task,
   FIFO-ordered) intentionally stay split the same way they already are for
   `CoptSendrecv`.
4. **Treat a TX failure as a hard COP failure (skip `PduCllstOnline`, leave
   `comm_started` cleared but report something other than `Finished`)** —
   Rejected: by the time the transmit is attempted, the stop-comm state
   transition has already committed (see `TxFailure::Cancelled` discussion
   above) — there is no failure-atomic way to "undo" stopping communication
   just because the final message didn't make it out. Reporting the TX
   failure via the normal `PduErrorEvent` channel (as `CoptSendrecv` already
   does for its own TX failures) while still completing the state transition
   is the only contract that doesn't leave the CLL in an unreportable
   in-between state.
5. **Build a bespoke, simpler frame-construction path for this one call site
   instead of reusing `resolve_send_recv_tx`** — Rejected: `cop_data` is
   payload-only (ADR-050) regardless of which COP type sent it; every
   ADR-049/ADR-055 validation `CoptSendrecv` already enforces applies
   identically here, and a bespoke path would either silently skip that
   validation (reopening a smaller version of the exact class of bug this
   ADR fixes) or duplicate it and risk the two copies drifting.
6. **Coalesce a second concurrent `CoptStopcomm` into the first, returning
   the first call's `cop_handle`** — Rejected: breaks per-handle event
   correlation (`GetStatus`/`GetEventItem` on the second caller's own
   `cop_handle` would never resolve, since no COP was actually allocated for
   it), and ISO 22900-2 gives no basis for one `StartComPrimitive` call
   silently returning another call's handle.
7. **Let the second call through, but exempt the first StopComm's own
   `cop_handle` from the cancel-siblings (`cops_to_cancel`) step** — Rejected:
   fixes only the in-flight-cancellation half of the bug. It does nothing
   about the second consequence: if the first StopComm completes normally
   before the second is processed, the second still enqueues its own
   `TxItem::StopComm` and runs a second `handle_stop_comm`, producing a
   duplicate final message and a duplicate `PduCllstOnline`/`PduCopstFinished`
   sequence. Synchronous rejection of the second call is the only fix that
   closes both consequences at once.

## Consequences

- `StartComPrimitive(COPT_STOPCOMM)` with non-empty `cop_data` now performs
  one `PassThruWriteMsgs` (or, in software-ISO-TP mode, the segmented
  equivalent) between stopping the periodic tester-present message and
  emitting `PduCllstOnline`, closing the data-loss bug.
- `StartComPrimitive(COPT_STOPCOMM)` with non-empty `cop_data` can now fail
  synchronously with `INVALID_ARGUMENT` (resolution failure) or
  `RESOURCE_EXHAUSTED` (another CLL holding `LOCK_PHYSICAL_TX_QUEUE`) —
  neither was previously possible for `CoptStopcomm`, since it never touched
  `cop_data` or the TX-queue lock at all. Empty-`cop_data` `CoptStopcomm`
  behavior, including its exemption from `LOCK_PHYSICAL_TX_QUEUE`, is
  unchanged.
- A second `CoptStopcomm` call on the same CLL while the first is still
  queued/executing now fails synchronously with `FAILED_PRECONDITION` ("a
  CoptStopcomm is already in progress for this ComLogicalLink") instead of
  being routinely accepted and racing the first — closing the amendment's
  cancellation/duplicate-teardown bug. A `CoptStopcomm` after the CLL's
  stop-comm sequence has fully completed is unaffected.
- A queued (not-yet-dispatched) `CoptStopcomm` cancelled via
  `PDU_IOCTL_CLEAR_TX_QUEUE` or an explicit `CancelComPrimitive` now emits
  `PduCopstCancelled` and never transmits its `cop_data` payload, instead of
  silently executing anyway — closing the round-4 gap in `CLEAR_TX_QUEUE`'s
  "clear pending TX" contract. `comm_started` stays `true` and
  `stop_comm_pending` is cleared, so a follow-up `CoptStopcomm` is accepted
  normally. An already-dispatched (executing) `CoptStopcomm` is unaffected.
- `CancelComPrimitive` on a `CoptStopcomm` already parked in `tx_held` (via
  `PDU_IOCTL_SUSPEND_TX_QUEUE`) now takes effect synchronously, closing the
  round-5 gap: `PduCopstCancelled` is emitted and `stop_comm_pending` is
  cleared without waiting for a later resume/clear/disconnect.
  `comm_started` stays `true` (the stop genuinely never happened), and a
  follow-up `CoptStopcomm` is accepted immediately — no longer stuck
  rejecting with "already in progress" until some unrelated later event.
  Held non-StopComm COPs are unaffected: they keep the pre-existing
  mark-and-defer behavior.
- A client cancelling an executing StopComm may transiently see
  `PduCopstCancelled` from a `GetStatus` poll (since `cancelled_cops` still
  gets the entry inserted by the RPC, just never consulted by the now
  non-cancellable transmit) before the terminal `PduCopstFinished` event
  eventually arrives — this is not new: it is the same pre-existing behavior
  every other non-cancellable window in this codebase already has (e.g.
  ADR-083's mode-0 periodic-start gate), accepted as-is, not something this
  round-6 amendment needs to fix. *Update (Codex-review fix, PR #92):* this
  acceptance was predicated on the window being genuinely transient — true
  for the transmit step here (bounded to a P3 gap wait plus a few frames of
  I/O) and left unchanged. ADR-087 later added a non-cancellable *receive*
  phase after this transmit, reusing the identical `cancellable: false`
  mechanism, whose window can run for the full `CP_P2Max`/IS-MULTIPLE-ceiling/
  RC-completion duration — no longer transient. That later window actively
  drains the stale `cancelled_cops` entry on every poll pass instead of
  inheriting this acceptance; see ADR-087's own Consequences for the
  reasoning. This transmit-step residual is unaffected and remains accepted
  as-is.
- `TxItem::StopComm` and `handle_stop_comm`'s signatures change (new
  `protocol_id`/`tx` fields; `handle_stop_comm` now takes `&ChannelPollCtx`
  like `handle_send_recv` instead of discrete arguments), a mechanical
  refactor with no behavioral effect on the `tx: None` path.
- `docs/j2534-0404-architecture.md`'s `TxItem` listing,
  `docs/rpc-api-guide.md`'s `COPT_STOPCOMM` references, and
  `j2534-0404-service/docs/adapter-design.md`'s COP execution model summary
  are updated in the same change to describe the new transmit step; none of
  their existing "writes no hardware ComParam config" claims change meaning
  or need retraction, since those are specifically about `SET_CONFIG`, not
  payload transmission.

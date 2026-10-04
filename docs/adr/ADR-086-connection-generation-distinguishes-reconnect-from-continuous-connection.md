# ADR-086: `connect_generation` Distinguishes a Same-Channel Reconnect from a Continuous Connection

**Date:** 2026-07-14
**Status:** Accepted (amended by ADR-100)
**Affects:** `j2534-0404-service` service, rpc_link, rpc_primitive, events

## Context

`handle_start_comm` and `handle_stop_comm` both run non-cancellable waits
(`wait_for_p3_gap`, a multi-frame ISO-TP transmit) between accepting a COP
and emitting its terminal status. A concurrent `DisconnectComLogicalLink`/
`DestroyComLogicalLink` is not gated by `cancelled_cops` and can race any of
these waits. `DisconnectComLogicalLink` deliberately leaves the CLL's
`logical_links` entry in place and only clears `channel_id`/`channel_key`/
`connected`/`comm_started`/`tester_present_state` — so both handlers grew a
`still_on_this_channel`-style guard (`handle_start_comm`'s three sites:
ADR-083's mode-0 gate and Step-3 write-back, ADR-084's mode-1 gate;
`handle_stop_comm`'s two sites: ADR-085's round-7 amendment) that re-checks
`channel_id == Some(ctx.channel_id)` after the wait, before transmitting or
writing back terminal state, bailing out (first-wins `PduCopstCancelled`,
deferring to whichever side — this guard or `cancel_link_cops` — removes the
`cop_handle` from `primitives` first) when it no longer matches.

`channel_id` conflates two distinct identities: the physical J2534 channel,
and this CLL's own logical connection to it. On a *shared* physical channel
(`SharedChannel::ref_count > 1`), disconnecting one CLL does not tear the
channel down — another CLL sharing it keeps the channel (and its poll task)
alive — it only clears the disconnecting CLL's own `channel_id`. A
subsequent reconnect of the *same* `cll_handle`, at the same protocol/baud,
rejoins the same still-open shared channel and sets `link.channel_id` back
to the *identical* `ChannelId` value. Every `still_on_this_channel`-style
guard, checking `channel_id` equality alone, then incorrectly treats a COP
captured against the OLD (already-disconnected-and-cancelled) session as
still valid: it can transmit that COP's stale, pre-disconnect payload and
emit `PduCllstOnline`/`PduCopstFinished` for the CLL's brand-new session,
even though `cancel_link_cops` already reported
`PduCopstCancelled` for that COP at disconnect time.

This was found by Codex review against `handle_stop_comm`'s ADR-085
round-7 guard specifically, but `handle_start_comm`'s three guards (added in
an earlier PR-88 round, unrelated to the StopComm fix) share the exact same
flaw, since they were built on the same `channel_id`-only check. Both are
fixed together here, per design-advisor's recommendation, since the
mechanism and root cause are identical.

### Round 5: two remaining gaps in the `connect_generation` mechanism itself

**Gap 1 (P1): `CoptStopcomm`'s own `connect_generation` capture was taken
from a later, unpaired snapshot than the data it was meant to validate,
inverting the guard for a specific interleaving.** `rpc_start_com_primitive`
captures an early `link` snapshot near the top of the function (before
`match cop_type`); a shared, later critical section (the ADR-067 call-time-
binding block used by `CoptSendrecv`/`CoptStartcomm`/`CoptStopcomm` alike)
separately captures `bound_comparams`/`bound_active_table` and, since round 4,
`connect_generation`, from a *second*, independent `logical_links` lock
acquisition. `CoptStopcomm`'s own branch then had a *third*, even later,
critical section (its `stop_comm_pending` test-and-set) that re-captured
`connect_generation` yet again, shadowing the round-4 capture with a value
read after everything else. The enqueued `TxItem::StopComm`'s `tx` payload is
built from the *early* snapshot's `protocol`/`hw_protocol_id`/
`software_isotp`, combined with the *shared block*'s `bound_active`/
`bound_active_table`. If a disconnect+reconnect+successful-restart of the
same `cll_handle` completed between the early snapshot and the late
`stop_comm_pending` block, the late block's re-capture observed the *new*,
current generation — passing every downstream `connect_generation` guard —
while `tx` itself still carried data resolved from the *old* session. The
guard was, in effect, inverted for this interleaving: the more recent the
race, the more likely the (correct-looking, but actually stale) capture was
to match the live value.

**Gap 2 (P2): the pre-dispatch `should_skip_cancelled_item` check never
consulted `connect_generation` at all.** `cancel_link_cops` (invoked by
`DisconnectComLogicalLink`) removes every COP mapped to the disconnecting
`cll_handle` directly from `primitives` and emits `PduCopstCancelled`
immediately — it does not use the `cancelled_cops` marker set the way
`CancelComPrimitive`/`CLEAR_TX_QUEUE` do. `should_skip_cancelled_item`, which
runs just before a dequeued item is dispatched, only checked `l.connected`
and `cancelled_cops` membership — neither of which can detect "this queued
item's captured generation no longer matches the live one", which is exactly
the state left behind by a disconnect+reconnect that landed while the item
was still queued (even though `connected` is true again post-reconnect, and
the item was never in `cancelled_cops` to begin with, since `cancel_link_cops`
bypasses that set entirely). A stale `TxItem::StopComm`/`TxItem::StartComm`
still sitting in the queue at disconnect+reconnect time therefore passed this
check and reached `handle_start_comm`/`handle_stop_comm`, both of which emit
`PduCopstExecuting` — a status event *after* the client already saw
`PduCopstCancelled` for that exact COP — and, for `handle_stop_comm`
specifically, unconditionally steal/clear `LogicalLinkState.tester_present_state`
(and call `stop_periodic_message` for a `Periodic` one) *before* its own
in-handler `still_on_this_channel` guard ever runs — capable of tearing down
a brand-new session's tester-present state if the reconnect landed on a
different physical channel whose own fresh `CoptStartcomm` had already armed
one.

## Decision

Add a service-global monotonic counter, `J2534Service::next_connect_generation:
Arc<Mutex<u64>>`, alongside the existing `next_cll_handle`/`next_cop_handle`
counters, with a `next_connect_generation()` helper mirroring
`next_logical_link_handle`/`next_primitive_handle` but without their
uniqueness loop or "0 is reserved" skip — this is a plain monotonic value,
not a map key.

Add `LogicalLinkState::connect_generation: u64`, defaulting to `0` at
`CreateComLogicalLink` (never-yet-connected). `finalize_connected_link`
(`rpc_link.rs`) stamps a freshly-allocated generation on *every* finalized
`ConnectComLogicalLink`, including a reconnect of the same `cll_handle` onto
the same physical channel — not just on a brand-new physical channel. The
counter's own mutex is independent of `logical_links`; no lock-ordering
hazard is introduced by awaiting it while `logical_links` is held.

`TxItem::StartComm` and `TxItem::StopComm` each gain a `connect_generation:
u64` field, captured at `StartComPrimitive` call time (ADR-067's discipline):
`CoptStopcomm`'s capture happens inside the same critical section that
already test-and-sets `stop_comm_pending` (closing the same TOCTOU class
ADR-085 already closes for that flag); `CoptStartcomm`'s capture happens in
the shared call-time critical section that also binds `bound_comparams`/
`bound_active_table`, after the `comm_started`-already-started precondition
check, so it reflects the connection the COP is actually being accepted
against.

Every existing `still_on_this_channel`-style guard (three in
`handle_start_comm`, two in `handle_stop_comm` — five sites total) is
extended from `channel_id == Some(ctx.channel_id)` to `channel_id ==
Some(ctx.channel_id) && connect_generation == <live value>`. A reconnect
always bumps the live generation, so a stale COP's call-time-captured value
no longer matches even when `channel_id` happens to match again. The
bail-out logic itself (first-wins `primitives.remove` + optional
`PduCopstCancelled` + `return`) is unchanged at every site — only the
boolean predicate deciding whether to take that path is extended.

### Amendment (PR #90, P1 Codex finding): `handle_start_comm`'s K-line init
### path runs irreversible side effects before any guard ever ran

The three original `handle_start_comm` sites above (ADR-083's mode-0 gate,
ADR-084's mode-1 gate, and their shared Step-3 write-back) all sit *after*
`run_protocol_init` — the K-line five-baud/fast-init hardware handshake.
`cancel_link_cops` (invoked by `DisconnectComLogicalLink`) is not gated on
the COP still executing: it removes `cop_handle` from `primitives` and emits
`PduCopstCancelled` for it immediately, even while `handle_start_comm` is
still running in the poll task. A disconnect+reconnect of the same
`cll_handle` onto a still-shared physical channel, landing while
`run_protocol_init` is executing or in the brief window right after it
returns, let the stale COP: send real five-baud/fast-init wakeup traffic to
the ECU, write the negotiated `DATA_RATE` into the reconnected session's
Working/Active ComParam sets, and deliver a synthetic init-response event to
the new session's subscribers — all despite the client having already been
told this COP was `Cancelled`. None of the three original sites could catch
this, since they run after all of that has already happened.

`handle_start_comm` gains five more sites, all sharing the exact same
`channel_id == Some(ctx.channel_id) && connect_generation == connect_generation`
predicate:

- **Guard A** (pre-init): immediately before the `Temp`-binding
  `apply_params_to_hardware` call that precedes `run_protocol_init`. A clean
  bail here needs no hardware revert — nothing has touched the hardware yet.
  Uses the same first-wins bail-out idiom as every other site.
- **Guard B** (post-init): inside `run_protocol_init`'s `Ok(response_bytes)`
  arm, immediately after the unconditional `last_bus_activity` stamp and
  before the `CP_Baudrate` (DATA_RATE) readback/write-back block. This is
  the site that actually catches the race described above: the wakeup
  traffic already happened (irreversible) and the stamp above records that
  truth unconditionally, but the DATA_RATE write-back and synthetic response
  delivery below it must not land on a session already told `Cancelled`. If
  stale and `binding` is `Temp`, reverts hardware to the live Active set
  first (ADR-067's obligation still applies), then performs the same
  first-wins bail-out.
- **Two defense-in-depth re-checks, not independent bail-out points**: the
  DATA_RATE write-back and the synthetic-response `rx_buf` lookup each fold
  the same predicate into a `.filter(...)` on their `logical_links` lookup,
  closing the TOCTOU between Guard B's check and each write site. Guard B is
  still the sole authoritative decision point for this arm; a `None` filter
  result at either write site is skipped silently (no additional
  `PduCopstCancelled`, since Guard B already decided).
- **Err-arm check**: at the top of `run_protocol_init`'s `Err(err)` arm,
  before the existing `if matches!(&binding, ParamBinding::Temp { .. })`
  revert call. The revert (if `binding` is `Temp`) is unconditional
  regardless of staleness — the temp config is physically on the hardware
  either way. If stale, bails out with the same first-wins
  `PduCopstCancelled` idiom instead of the normal
  `PduErrEvtInitError` + `PduCopstFinished` pair, which would otherwise be a
  duplicate terminal status for a COP the client already saw `Cancelled`.

Counting all of the above, `handle_start_comm` now has eight
`still_on_this_channel`-predicate sites (the original three plus these
five), and `handle_stop_comm` still has two, for ten sites total across the
two handlers.

### Amendment (PR #90, round 5, Codex findings): closing the capture-inversion and pre-dispatch gaps

**Fix for Gap 1**: the shared ADR-067 critical section's `connect_generation`
capture (the one already introduced in round 4) now immediately consistency-
checks itself against `rpc_start_com_primitive`'s *initial* `link` snapshot,
before any `cop_handle` allocation: if the two disagree, the call spans a
reconnect and is rejected synchronously with `FAILED_PRECONDITION` (no
cleanup needed, since nothing has been allocated yet). This applies
uniformly to every `cop_type` that reaches this shared block — `CoptSendrecv`
and `CoptStartcomm` get the same one-connection guarantee as `CoptStopcomm`,
for free, matching this block's existing "capture unconditionally regardless
of which branch below uses it" style. `LinkView` (the snapshot type
`get_link_state` returns) gained a `connect_generation` field so the initial
snapshot has something to compare against. `CoptStopcomm`'s own later
`stop_comm_pending` critical section no longer re-captures
`connect_generation` at all — the round-4 capture, now consistency-checked at
the point it is taken, is trustworthy and is threaded straight through under
the same binding. That same critical section additionally rejects (mirroring
its existing `comm_started`/`stop_comm_pending` rejection idiom exactly, same
drop-then-remove-then-return-`Err` shape) if the live generation has moved
past the captured value by the time `stop_comm_pending` is about to be set —
covering the narrower window between the shared block's capture and
`CoptStopcomm`'s own later block, which only `CoptStopcomm` has (no other
`cop_type` has a second, later critical section of its own).

**Fix for Gap 2**: `TxItem` gains a `connect_generation() -> Option<u64>`
helper (mirroring the existing `handles()` helper's shape) returning the
captured generation for `StartComm`/`StopComm`, `None` for every other
variant. `should_skip_cancelled_item` gains an `item_generation: Option<u64>`
parameter; when the live link is connected, it now also computes
`generation_stale = item_generation.is_some_and(|g| g != l.connect_generation)`
and folds it into the existing skip decision as an *additional*, orthogonal
condition alongside `explicit_cancel` — a generation-stale item is routed
through the same "implicit cancel" path an offline CLL already uses
(best-effort `primitives.remove`, notify only if first to remove — the
client's earlier `PduCopstCancelled` from `cancel_link_cops` already won that
race in practice, so this path stays silent). `generation_stale` deliberately
does *not* clear `stop_comm_pending`: `DisconnectComLogicalLink` already
resets that flag as part of its own teardown, and a new session on the same
`cll_handle` may have its own legitimate `CoptStopcomm` in progress with the
flag set — clearing it here would corrupt that new session's state.
`dispatch_tx_item` threads `item.connect_generation()` into the check
alongside the existing `item.handles()` call (both borrow `&item`, called
before the later ownership-consuming `match item { ... }`).

(This round left `TxItem::SendRecv`/`TxItem::UpdateParam` without a
`connect_generation` field as a deliberately out-of-scope residual; round 9,
below, closes it.)

## Consequences

- Both `handle_start_comm`'s and `handle_stop_comm`'s guards now correctly
  reject a stale COP across a same-channel disconnect-then-reconnect of the
  same `cll_handle`, in addition to the cases they already caught
  (disconnect to a cleared `channel_id`, reconnect onto a *different*
  channel).
- No new lock-ordering edges: `next_connect_generation`'s mutex is
  independent of `logical_links`/`shared_channels`/`primitives`.
- The pre-existing no-I/O micro-window between each guard's lock release and
  its status emission (documented in ADR-085's round-7 amendment and
  `handle_start_comm`'s own guard comments) is unchanged and not addressed
  here — this ADR closes the *identity* gap (which connection a COP belongs
  to), not the *timing* gap (residual scheduler-yield window after the
  identity check).
- `connect_generation == 0` is reserved for "never connected" and is never
  observed as a live comparison value in practice, since `StartComPrimitive`
  already requires a connected CLL before any COP referencing
  `connect_generation` can be accepted.
- (PR #90 amendment) `handle_start_comm` gains pre-init and post-init guards
  (Guard A and Guard B above), because the K-line init path performs
  irreversible side effects (real ECU bus traffic, DATA_RATE write-back,
  synthetic response delivery) *before* the pre-existing tester-present/
  terminal guards ever run — those three original guards, sitting entirely
  after `run_protocol_init`, structurally cannot catch a stale reconnect that
  lands during or immediately after the init handshake.
- (PR #90 amendment) `last_bus_activity` stamping is deliberately exempt from
  the new post-init guard (Guard B) — it records channel-scoped physical
  truth (real bus traffic occurred), not link-scoped bookkeeping, and sibling
  mode-1 CLLs sharing the channel must defer their idle windows regardless of
  which CLL caused the traffic (ADR-083's own rationale). The stamp stays
  unconditional even when Guard B's own check right after it finds the COP
  stale.
- (PR #90 amendment) On a stale-detected bail after a `Temp`-binding hardware
  apply (both Guard B's failure path and the `Err`-arm check's fix), the
  ADR-067 hardware revert obligation still runs unconditionally — staleness
  cancels the COP's bookkeeping/status, not its hardware-cleanup obligation.
- (PR #90, round 5) A `StartComPrimitive` call whose own two internal
  `logical_links` reads (the initial snapshot and the shared ADR-067 capture
  block) straddle a disconnect+reconnect of its own CLL is now synchronously
  rejected with `FAILED_PRECONDITION` where it previously would have silently
  proceeded on a mixed-time (old link, new generation) snapshot; clients
  should retry against the current connection. `CoptStopcomm` additionally
  gets a second, later rejection point (its own `stop_comm_pending` critical
  section) covering the narrower window past the shared capture.
- (PR #90, round 5) A queued `TxItem::StartComm`/`TxItem::StopComm` that goes
  stale while still sitting in the poll task's FIFO — because
  `cancel_link_cops` cancelled it directly, bypassing `cancelled_cops`, while
  a reconnect (possibly onto a different physical channel with its own,
  independent poll task) already re-armed a new session on the same
  `cll_handle` — is now skipped before dispatch, matching the
  `PduCopstCancelled` the client already received, instead of reaching
  `handle_start_comm`/`handle_stop_comm` and corrupting that new session's
  state or emitting a `PduCopstExecuting` after the fact.
- (PR #90, round 5) `TxItem::SendRecv`/`TxItem::UpdateParam` remained without
  a `connect_generation` field at this point — a known, deliberately deferred
  residual for the identical queued-then-stale scenario. Round 9, below,
  closes it.

### Amendment (PR #90, round 6, Codex finding): `handle_stop_comm`'s own
### tester-present teardown ran with no pre-teardown guard at all

Round 5's pre-dispatch `should_skip_cancelled_item` fix (Gap 2 above) closes
the "stale while still queued" window, but a *new* window remains once an
item passes that pre-dispatch check and is actually dispatched:
`handle_stop_comm`'s own body, immediately after its unconditional
`PduCopstExecuting` emission, went straight to `std::mem::replace`-ing
`LogicalLinkState.tester_present_state` and, for a `Periodic` value, calling
`stop_periodic_message` — with no `still_on_this_channel` check anywhere
before that point. A disconnect+reconnect+fresh `CoptStartcomm` completing in
the narrow window between dispatch and this teardown step steals/clears
whatever `tester_present_state` the brand-new session just armed, and, for a
`Periodic` one, sends a real `PassThruStopPeriodicMsg` tearing down the new
session's periodic message. This mirrors exactly the class of bug the PR #90
amendment above already fixed for `handle_start_comm`'s pre-init side effects
— unlike that function, though, `handle_stop_comm`'s two pre-existing guards
(the pre-transmit check inside `if let Some(tx) = tx { ... }`, and the
terminal block's check) both sit *after* this teardown step, so for an
empty-`cop_data` `CoptStopcomm` call (`tx: None`) there was, before this
round, no generation check anywhere in the function prior to its very last
(terminal-block) check.

`handle_start_comm`'s Guard A pattern is ported verbatim into
`handle_stop_comm` as a new **Guard 0**, placed immediately after the
function's own unconditional `PduCopstExecuting` emission (matching the
established precedent, carried over from the Guard A amendment above, that
this ordering — status emitted, then the guard checked — is acceptable; only
the *destructive* side effects that follow need guarding, not the status
emission itself) and before the `periodic_id` extraction / `mem::replace`
block. Same first-wins bail-out idiom as every other site in this file.

`handle_stop_comm` now has three `still_on_this_channel`-predicate sites (up
from two): this new pre-teardown Guard 0, the existing pre-transmit guard,
and the existing terminal-block guard — each guarding a distinct time window
(dispatch-to-teardown, the P3-gap-wait/transmit window, and the
post-transmit-to-terminal-status window, respectively), none redundant with
the others. Counting `handle_start_comm`'s eight sites, the two handlers now
have eleven `still_on_this_channel`-predicate sites total.

A dedicated regression test covering this specific dispatch-to-teardown
window was attempted but found infeasible to construct deterministically
with this crate's existing mock harness (see
`j2534-0404-service/docs/implementation-notes.md` for the concrete reasoning);
the full existing test suite was confirmed to still pass with Guard 0 in
place, verifying no regression to the common/happy path.

## Consequences (round 6 addendum)

- (PR #90, round 6) A disconnect+reconnect+fresh-`CoptStartcomm` race landing
  in the narrow dispatch-to-teardown window inside `handle_stop_comm` is now
  caught before any destructive side effect, for both empty- and
  non-empty-`cop_data` `CoptStopcomm` calls — previously the empty-`cop_data`
  path had zero protection until the terminal check.

### Amendment (PR #90, round 8, Codex findings): Guard 0's own TOCTOU, and an
### unguarded transmit-error report

Round 6's Guard 0 (above) closed the dispatch-to-teardown window, but two
narrower gaps remained, both flagged by Codex against the round-7 commit that
introduced Guard 0:

**Finding 1 (P2): Guard 0 itself was two separate critical sections.** Guard
0 read `still_on_this_channel` under one `logical_links` lock acquisition,
released it, and only then took a *second* acquisition to perform the
`tester_present_state` `std::mem::replace`. On the production multi-threaded
Tokio runtime (unlike this crate's `current_thread` test runtime), the poll
task can be preempted between those two acquisitions: a disconnect+reconnect
+fresh `CoptStartcomm` landing in that exact gap still passes Guard 0's check,
then the stale task resumes and performs the replace anyway — the same class
of bug Guard 0 itself was built to close, just narrowed to a smaller window.
Fixed by folding the check and the mutation into one critical section, the
same way the terminal block (round 5) already does: `periodic_id` is now
`Option<Option<u32>>`, computed via a single `logical_links.lock().await` +
`get_mut` + guarded `and_then`, where the outer `None` means "stale or
missing" (bail, same first-wins idiom) and the inner `Option<u32>` is the
extracted periodic handle, atomically paired with the guard that authorized
extracting it.

**Finding 2 (P2): the pre-transmit guard did not cover the transmit's own
error-reporting path.** The pre-transmit `still_on_this_channel` check (the
existing guard immediately before `transmit_request`) only protects entry
into the transmit call — `transmit_request` itself performs real I/O and can
span a disconnect+reconnect completing during the call. Its
`Err(TxFailure::Event(error_event))` arm called `send_error_event`
unconditionally, so a stale COP's transmit failure still updated
`last_error` and notified subscribers on the *reconnected* CLL. Fixed by
adding the same `still_on_this_channel` predicate immediately before
`send_error_event` in that arm; when stale, the event is skipped entirely.
This does not leave the COP without a terminal status: the terminal block
below independently re-checks generation before any status emission,
regardless of which branch of the transmit match was taken.

`handle_stop_comm`'s site count is unchanged at three (Guard 0's fold does
not add a new site, it merges what were two acquisitions into one), plus the
transmit-error recheck, which is a defense-in-depth re-check folded into the
existing pre-transmit guard's protected region rather than an independent
bail-out point — mirroring `handle_start_comm`'s own "defense-in-depth
re-check, not an independent bail-out point" pattern for its DATA_RATE/
synthetic-response write sites (see the PR #90 amendment above).

## Consequences (round 8 addendum)

- (PR #90, round 8) Guard 0 now closes its own TOCTOU: the
  `still_on_this_channel` check and the `tester_present_state` mutation it
  authorizes can no longer be split by a scheduler preemption on the
  multi-threaded production runtime.
- (PR #90, round 8) A disconnect+reconnect completing *during*
  `transmit_request`'s own I/O (as opposed to before it, which the
  pre-existing pre-transmit guard already caught) no longer causes a stale
  COP's transmit failure to update `last_error`/notify subscribers on the
  reconnected CLL's session.
- As with round 6, a dedicated regression test for either window was
  attempted and found infeasible to construct deterministically with this
  crate's mock harness (single acquisition and I/O-bound `transmit_request`
  cannot be interleaved with a reconnect from synchronous test code); see
  `j2534-0404-service/docs/implementation-notes.md`. The full existing test
  suite was confirmed to still pass with both fixes in place.

### Amendment (PR #90, round 9, Codex finding): `TxItem::SendRecv`/
### `TxItem::UpdateParam` carried no `connect_generation` at all

Round 5 documented, rather than fixed, a narrower residual: `TxItem::SendRecv`
carried no `connect_generation` field, so neither the pre-dispatch
`should_skip_cancelled_item` check nor any in-handler guard could detect a
queued or in-flight `CoptSendrecv` that went stale across a disconnect+
reconnect of its `cll_handle`. `TxItem::UpdateParam` shared the same gap,
undocumented until Codex flagged it against this round. A stale, dispatched
`CoptSendrecv` could transmit stale, pre-disconnect data onto the
reconnected session's bus, and — worst case — an IS-CYCLIC
(`NumReceiveCycles == -1`) receive has no deadline of its own and is normally
ended only by `cancelled_cops`, which `cancel_link_cops` bypasses entirely: a
stale IS-CYCLIC `CoptSendrecv` would poll forever, surviving the reconnect for
the rest of the process's life. A stale, dispatched `CoptUpdateparam` could
write stale ComParams to hardware via `PassThruIoctl SET_CONFIG` and promote
them to the reconnected session's Active set.

**The fix**: `TxItem::SendRecv` and `TxItem::UpdateParam` each gain a
`connect_generation: u64` field, captured the same way `StartComm`/`StopComm`
already do — reusing the existing shared ADR-067 call-time critical section in
`rpc_start_com_primitive` (the same already-consistency-checked capture
`CoptStopcomm`'s constructor already threads through), not a fresh
`logical_links` read. `TxItem::connect_generation()` now returns `Some` for
both variants (previously `None`), which — with no further code change —
immediately extends the existing round-5 pre-dispatch
`should_skip_cancelled_item` check to both: a `CoptSendrecv`/`CoptUpdateparam`
that goes stale while still queued behind other work is now skipped before
`handle_send_recv`/`handle_update_param` ever runs, exactly like
`CoptStartcomm`/`CoptStopcomm` already were.

**`handle_send_recv` gains four in-handler sites plus one inside
`wait_for_expected_response`**, all sharing the same
`channel_id == Some(ctx.channel_id) && connect_generation == connect_generation`
predicate and the established first-wins bail-out idiom:

- **S1** (pre-apply): immediately before the `Temp`-binding hardware apply
  that precedes this cycle's transmit. Nothing has touched hardware yet, so a
  clean bail is safe.
- **S2** (pre-transmit): inside the `P3GapOutcome::Ready` arm, immediately
  before `transmit_request`. Because a `Temp` binding may already have pushed
  params to hardware by S1's point, S2 does NOT bail out directly — it sets a
  `stale` flag (sibling to the existing `channel_lost`/`cancelled` flags) and
  lets control fall through to the unconditional `revert_hardware_to_live_active`
  call (the ADR-067 hardware-cleanup obligation, which staleness cancels this
  COP's bookkeeping but never its hardware-cleanup duty), then bails out
  immediately after. A folded recheck in the `Err(TxFailure::Event(..))` arm
  skips `send_error_event` when stale — mirrors `handle_stop_comm`'s round-8
  transmit-error recheck (`transmit_request` performs real I/O and can span a
  reconnect completing mid-call) — **and must also set `stale = true`, not
  merely skip the event** (edge-case-hunter finding, caught before this round
  was committed): an initial draft of this recheck only gated
  `send_error_event` and left `stale` unset, which let control fall through to
  the plain `!write_ok` path further down and emit a contradictory
  `PduCopstFinished` for a COP the client may already have seen
  `PduCopstCancelled` for. Fixed to route through `stale` like every other
  bail path in this function.
- **S3** (post-revert / pre-receive): after the `!write_ok` check, before the
  receive phase begins. The `Temp` revert has already run by this point, so a
  clean bail is safe again.
- **S4** (terminal/continuation): after the receive phase returns
  `CycleComplete`, before the send-cycle accounting. If stale: first-wins
  `PduCopstCancelled` instead of the normal `PduCopstFinished`, and — this is
  a deliberate design decision, not an oversight — **no continuation cycle is
  scheduled for a stale item**. One COP is tied to one connection generation
  for its entire cyclic lifetime; a reconnect landing mid-cycle cancels the
  COP outright rather than resurrecting it under the new generation. The
  cyclic continuation's own re-enqueue (unchanged from before this round)
  copies `connect_generation` through unmodified alongside every other
  call-time-bound field (`tx`/`binding`/`expected_response`/etc.) — it is
  never re-read from the live link for a follow-up cycle, by design.
- **Per-pass check inside `wait_for_expected_response`** (folded into the
  function's existing per-pass `logical_links` lock acquisition, alongside the
  pre-existing `cancelled_cops` check — no new lock taken): this is the most
  important site in this round. An IS-CYCLIC receive has no deadline of its
  own and is otherwise ended only by `cancelled_cops`, which
  `cancel_link_cops` bypasses; without this check a stale IS-CYCLIC receive
  would loop forever past a reconnect. A folded, defense-in-depth recheck
  immediately before the RC21/RC23 negative-response re-request's
  `transmit_request` call skips that specific re-request when stale (protects
  a side effect mid-loop, not an independent decision point — the per-pass
  check further below is still authoritative and runs regardless, for every
  outcome of that re-request **except** its own `Err(TxFailure::Event(..))`
  arm).
- **RC21/RC23 retransmit error recheck** (edge-case-hunter finding, caught
  before this round was committed): the RC21/RC23 re-request's own
  `Err(TxFailure::Event(ev))` arm `return`s `ReceivePhaseOutcome::Terminal`
  immediately — an initial draft's comment claimed the per-pass check "runs
  regardless," which is false for this specific arm, since `return` skips it
  entirely. `transmit_request` performs real I/O here too and can span a
  disconnect+reconnect completing mid-call even though the pre-retransmit
  folded recheck above already passed. Without its own recheck, a stale COP
  whose retransmit then failed would get an unconditional `PduErrEvtTxError` +
  `PduCopstFinished` — a contradictory terminal status after the client had
  already seen `PduCopstCancelled` from `cancel_link_cops`. Fixed with the
  same first-wins guard idiom as every other bail-out site in this file,
  immediately before the `send_error_event`/`PduCopstFinished` pair.

**`handle_update_param` gains two in-handler sites** (U1, U2), plus the
round-8 "extend, don't duplicate" pattern applied to three already-existing
boolean expressions:

- **U1** (pre-apply): folded into the existing critical section that reads
  `hw_protocol_id`. Produces a three-way outcome — link missing (unchanged,
  falls through to the normal terminal `PduCopstFinished`), stale (new:
  first-wins bail with `PduCopstCancelled`, the `SET_CONFIG` IOCTL is never
  issued), or live (proceed as before).
- **U2** (pre-promotion): folded into the existing critical section that
  writes `link.active = params` and computes the tester-present re-arm
  snapshot. `apply_params_to_hardware` (between U1 and U2) is a real hardware
  `.await` a reconnect can complete during, so U2 re-checks rather than
  trusting U1 alone. When stale: the `active` write, the
  `promote_unique_resp_id_table` call (avoiding stale `FLOW_CONTROL_FILTER`
  hardware I/O and an active-table write), and the tester-present re-arm
  block are all skipped, with a first-wins `PduCopstCancelled` bail. The
  `SET_CONFIG` IOCTL U1 already issued before this point is an accepted,
  documented channel-scoped residual on a stale U2 bail — `CoptUpdateparam`
  has no revert obligation the way a `Temp` binding does elsewhere in this
  file.
- The re-arm snapshot's own `channel_ok` computation, and the two existing
  post-`.await` rechecks inside the tester-present re-arm block (the
  `still_matches` gate before sending, and the final `tester_present_state`
  write-back), each get `&& connect_generation == connect_generation` folded
  into their existing `channel_id`-only boolean expression — extending, not
  duplicating, exactly the round-8 pattern.

**Test coverage**: the pre-dispatch generation-stale skip is testable with
the same `CoptDelay`-queued-behind technique
`stopcomm_stale_queued_item_does_not_tear_down_a_reconnected_sessions_tester_present`
uses, since it is the same shared `should_skip_cancelled_item` code path —
`j2534-0404-service/tests/grpc_mock/cop_ctrl_cycles.rs`'s
`sendrecv_stale_queued_item_is_skipped_before_dispatch_after_reconnect`
confirms a stale queued `CoptSendrecv` is now skipped before
`handle_send_recv` ever runs (no `PduCopstExecuting`, no transmit on the
original channel), and was confirmed to fail (transmit and status events
observed) with `TxItem::connect_generation()` reverted to returning `None`
for `SendRecv`, proving it is a genuine regression test. The in-handler
guards (S1–S4, the `wait_for_expected_response` sites, U1, U2) face the same
construction problem as every prior round's in-handler guard: this crate's
mock harness runs on a single-threaded (`current_thread`) test runtime with
no natural preemption point between dispatch and an in-handler check within
one async function's sequential `.await` points, so a dedicated regression
test for any individual in-handler site was not attempted, matching rounds
6/8's documented reasoning; see
`j2534-0404-service/docs/implementation-notes.md`. The full existing test
suite was confirmed to still pass with every fix in this round in place,
including the two edge-case-hunter findings above. Notably, those two
findings were themselves caught by a dedicated `edge-case-hunter` review pass
run against this round's diff before it was committed — not by Codex, and not
by any test — which is exactly the kind of gap this crate's test-harness
infeasibility notes across every prior round warn is possible: an in-handler
race with no natural preemption point in the single-threaded mock harness can
carry a real logic bug that only static/independent review catches.

## Consequences (round 9 addendum)

- (PR #90, round 9) A queued `TxItem::SendRecv`/`TxItem::UpdateParam` that
  goes stale while still sitting in the poll task's FIFO is now skipped
  before dispatch, via the same pre-dispatch mechanism that already protected
  `TxItem::StartComm`/`TxItem::StopComm` since round 5 — no code changes were
  needed in `should_skip_cancelled_item` itself, only in `TxItem::
  connect_generation()`'s coverage.
- (PR #90, round 9) A dispatched, in-flight `CoptSendrecv` that goes stale
  mid-cycle — including during an IS-CYCLIC receive with no deadline of its
  own — is now cancelled rather than continuing to transmit/receive on behalf
  of a session the client was already told was `Cancelled`.
- (PR #90, round 9) A dispatched, in-flight `CoptUpdateparam` that goes stale
  between its call-time capture and its hardware `SET_CONFIG` write, or
  between that write and the Active-table promotion, no longer promotes stale
  ComParams to the reconnected session's Active set or performs stale
  `FLOW_CONTROL_FILTER` hardware I/O. The `SET_CONFIG` IOCTL itself, once
  already issued before a stale U2 bail, remains an accepted channel-scoped
  residual (`CoptUpdateparam` has no revert obligation).
- (PR #90, round 9) A cyclic `CoptSendrecv`'s follow-up cycle never
  re-captures `connect_generation` from the live link — this is a considered
  design decision (one COP is tied to one connection generation for its
  entire cyclic lifetime), not an oversight; a reconnect mid-cycle cancels the
  COP via S4/the per-pass `wait_for_expected_response` check rather than
  silently resurrecting it under the new generation.
- Counting all of the above (before the round-10 amendment below),
  `handle_start_comm` has eight `still_on_this_channel`-predicate sites,
  `handle_stop_comm` has three, `handle_send_recv` (including
  `wait_for_expected_response`) has six (S1–S4, the per-pass IS-CYCLIC check,
  and the RC21/RC23 retransmit error recheck), and `handle_update_param` has
  two, for nineteen sites total across the four handlers. (Round 11, below,
  updates this further: `handle_start_comm` gains two more sites -- ten, up
  from eight -- and two previously-uncovered handlers, `handle_delay` and
  `handle_restore_param`, gain sites of their own, for twenty-four sites
  total across six handlers.)

### Amendment (PR #90, round 10, Codex findings): the *apply-failure* side of
### the round-9 guards, not just the transmit-failure side

Round 9 added `stale`-routing recheck logic to `handle_send_recv`'s
`Err(TxFailure::Event(..))` transmit-failure arm and `handle_update_param`'s
success (`all_ok`) path, but missed the mirror-image failure path in each
handler — the hardware *apply* itself failing, as opposed to the transmit
that follows a successful apply:

**Finding 1 (P2, `handle_send_recv`)**: `apply_params_to_hardware` (bound to
`temp_params_ok`, for a `Temp` binding) is itself a real hardware `.await` a
disconnect+reconnect can complete during — exactly like `transmit_request`
below it, which round 9 already guarded. When `apply_params_to_hardware`
returns `false`, `write_ok`'s `if temp_params_ok { .. } else { false }`
`else` arm took the plain `false` path with no staleness check at all,
falling through to the ordinary `!write_ok` → `PduCopstFinished` treatment —
a contradictory terminal status if `cancel_link_cops` had already cancelled
this COP during the race. Fixed with the same `stale`-routing recheck round 9
already uses for the transmit-failure arm, in the `write_ok` computation's
final `else` branch (the `!temp_params_ok` case): checks generation, sets
`stale = true` on mismatch, still returns `false` either way (so the
unconditional `Temp`-binding hardware revert further down still runs
regardless — the ADR-067 obligation is unaffected by this fix).

**Finding 2 (P2, `handle_update_param`)**: symmetric gap on the `apply_params_
to_hardware`-fails (`!all_ok`) branch of `handle_update_param`'s own hardware
apply. U1 (pre-apply) had already checked before the call started, but the
call itself can span a disconnect+reconnect completing during it — the exact
same class of gap U2 already closes for the *success* path, just unaddressed
on the failure path. The `!all_ok` branch unconditionally emitted
`PduErrEvtProtErr` and fell through to the terminal `PduCopstFinished`, with
no recheck. Fixed with a direct-bail recheck (not routed through a flag, the
way `handle_send_recv`'s Finding 1 fix is — `CoptUpdateparam` has no
unconditional cleanup obligation past this point, unlike `handle_send_recv`'s
`Temp`-binding revert): on stale, first-wins `PduCopstCancelled` bail and
`return`, before `PduErrEvtProtErr` is ever emitted.

Both findings follow the same shape as round 9's own two edge-case-hunter
catches: a guard added for one outcome of a hardware `.await` (success, or a
specific failure variant) while a sibling outcome of that same `.await`
remained unguarded. `handle_send_recv` gains no new site count (Finding 1 is
folded into the existing `write_ok` computation, not an independent site);
`handle_update_param`'s existing U1/U2 count is unchanged for the same
reason (Finding 2 is folded into the existing `!all_ok` branch). Site counts
are unchanged from the round-9 tally above — nineteen total.

## Consequences (round 10 addendum)

- (PR #90, round 10) A temp-param `CoptSendrecv` whose `apply_params_to_
  hardware` call itself fails during a disconnect+reconnect race no longer
  gets a contradictory `PduCopstFinished` after `cancel_link_cops` already
  cancelled it.
- (PR #90, round 10) A `CoptUpdateparam` whose `SET_CONFIG` sequence fails
  during a disconnect+reconnect race no longer emits `PduErrEvtProtErr` for
  the reconnected session or falls through to a contradictory
  `PduCopstFinished`.
- Test coverage: same infeasibility class as every prior in-handler guard in
  this file — no dedicated regression test, for the reasons documented in the
  round-6/8/9 notes above. The full existing test suite was confirmed to
  still pass with both fixes in place.

### Amendment (PR #90, round 11, Codex findings + design-advisor exhaustive
### audit): the last gap in `handle_start_comm`'s `Temp`-binding path,
### `TxItem::Delay`/`TxItem::RestoreParam` generation coverage, and three
### residual event-only sites

A design-advisor audit of the entire poll-task dispatch surface in
`events.rs`/`service.rs` (against the round-10 commit) found four remaining
categories of gap, none overlapping the sites already closed above.

**`handle_start_comm`'s `Temp`-binding apply gap.** Guard A (pre-init) runs
before the `Temp`-binding `apply_params_to_hardware` call; the call's own
`.await` is real hardware I/O a disconnect+reconnect can complete during,
even though Guard A already ran immediately before it. Both outcomes of that
call were unguarded:

- **Failure path**: the existing `if !applied { .. }` arm did the ADR-067
  hardware revert unconditionally (correct, unaffected by this fix), then
  unconditionally emitted `PduErrEvtProtErr` + `PduCopstFinished`. A
  `still_on_this_channel` snapshot is now taken immediately before the
  revert call (matching this arm's own established check-then-act ordering);
  the revert itself remains unconditional regardless of the snapshot's
  result (the temp config really is on hardware either way), but the
  `PduErrEvtProtErr`/`PduCopstFinished` pair immediately after the revert is
  now skipped, replaced with the standard first-wins `PduCopstCancelled`
  bail, when the snapshot is stale.
- **Success path (Guard A2)**: a fresh `still_on_this_channel` check,
  labeled Guard A2, runs immediately after the `Temp`-binding block closes
  (only when `binding` was `Temp` -- a `Plain` binding has no intervening
  `.await` since Guard A beyond the already-documented micro-windows). This
  is the fix Codex actually flagged: the very next side effects are
  `frame_tester_present_data`'s error-event emission and, decisively,
  `run_protocol_init`'s irreversible K-line wire traffic -- the existing
  post-init Guard B structurally cannot prevent that traffic, since it only
  runs after it has already gone out. On stale, Guard A2 reverts hardware to
  the live Active set first (ADR-067's obligation, mirroring Guard B's own
  stale-arm pattern), then bails out with the standard first-wins idiom.

`handle_start_comm` gains two sites from this fix (the failure-path
snapshot-then-bail, and Guard A2), bringing its total from eight to ten.

**`TxItem::Delay`/`TxItem::RestoreParam` had no `connect_generation` at
all.** Unlike `SendRecv`/`StartComm`/`StopComm`/`UpdateParam` (round 9
closed the last two of those), neither `Delay` nor `RestoreParam` carried a
captured generation, so a queued-then-stale instance of either passed
`should_skip_cancelled_item` unconditionally, and neither handler had any
in-handler staleness awareness once dispatched.

- Both variants gain a `connect_generation: u64` field, populated at
  `rpc_start_com_primitive`'s `CoptDelay`/`CoptRestoreParam` construction
  sites from the same already-captured, already-consistency-checked shared
  ADR-067 critical-section variable every other generation-carrying variant
  already reuses (no fresh `logical_links` read). `TxItem::connect_generation()`
  now returns `Some` for both, immediately extending the existing round-5
  pre-dispatch `should_skip_cancelled_item` check to cover them for free.
- `TxItem::ResumeWake` deliberately gains no field: it is intercepted by the
  poll loop before `dispatch_tx_item` is ever called (see the
  `!matches!(item, TxItem::ResumeWake { .. })` guard in `poll_channel_events`,
  and the `debug_assert!` in `dispatch_tx_item`'s own unreachable
  `ResumeWake` arm), is content-free, and has zero side effects of its own --
  there is nothing for a generation field to protect.
- **`handle_restore_param`** gains one site: the pre-fix code silently
  no-op'd on a missing link and unconditionally copied Active into Working
  otherwise, with no generation awareness and no terminal-status
  distinction for either case. The check and the `working`/
  `working_unique_resp_id_table` write are now folded into ONE critical
  section (round-8 Guard-0 style): `still = link.channel_id == Some(ctx.
  channel_id) && link.connect_generation == connect_generation`, computed
  inside the same `get_mut` match arm that performs the write, gating the
  write on `still`. Both "link missing" and "stale" now route through the
  same first-wins `PduCopstCancelled` bail after the lock is released,
  instead of the pre-fix silent no-op on link-missing.
- **`handle_delay`** gains two sites, supplementing (not replacing) its
  existing `cancelled_cops`-consulting mechanism -- explicit
  `CancelComPrimitive` still needs that path; only a disconnect+reconnect
  needs the new one, and both are checked every tick:
  - **Per-tick**: folded into the SAME `logical_links` lock acquisition as
    the existing `cancelled_cops` check, mirroring `wait_for_expected_
    response`'s own `(was_cancelled, is_stale)` tuple pattern exactly. On
    `is_stale`, sets a new `delay_stale` flag (declared alongside
    `delay_cancelled`/`delay_hard_error`) and breaks out of the loop early,
    rather than holding the shared channel's poll task for the remainder of
    a potentially long, client-controlled duration.
  - **Terminal**: one fresh `logical_links` lock, re-checking the same
    predicate, runs before whichever branch would emit `PduCopstFinished` --
    mandatory even for `delay_ms == 0` (zero ticks ever run, so the per-tick
    check never gets a chance to) and for a reconnect landing during the
    very last tick's `dispatch_due_idle_tester_present` call. This ONE
    terminal check covers both "went stale mid-loop" (`delay_stale` already
    true -- redundant but harmless, since `connect_generation` only ever
    moves forward) and "was already stale by the time we got here", routing
    either case through the same first-wins bail. The existing
    `delay_cancelled` branch's unconditional `PduCopstCancelled` emission is
    completely unchanged (explicit-cancel contract: `CancelComPrimitive`
    deliberately leaves the entry in `primitives` so `GetStatus` still
    returns `Cancelled` until this event fires -- this must never become
    first-wins-gated).
  - **Test coverage**: unlike every prior in-handler guard in this file's
    history, `handle_delay`'s per-tick `tokio::time::sleep` is a natural
    preemption point, making a deterministic regression test possible with
    this crate's single-threaded mock harness.
    `cop_ctrl_cycles.rs`'s `delay_dispatched_item_goes_stale_after_
    reconnect_on_same_channel` starts a `CoptDelay` on a shared physical
    channel, disconnects and reconnects the same `cll_handle` at the same
    `DATA_RATE` while it is already dispatched and mid-sleep (rejoining the
    identical `ChannelId` with a freshly-bumped `connect_generation`), and
    confirms `cancel_link_cops`'s own `PduCopstCancelled` fires but
    `handle_delay`'s own staleness detection does not also emit a status
    for the same `cop_handle`. Confirmed to fail (a duplicate status event
    observed) with the per-tick `is_stale` short-circuited to `false` and
    the terminal recheck's result hardcoded to `true`, proving it is a
    genuine regression test.

**R1-R3: three residual "event-only" gaps**, the same class of bug rounds 8
and 10 already fixed elsewhere -- an error/event emission after a real I/O
`.await` with no generation recheck immediately before it. None of these
affect terminal COP status (guarded elsewhere); only whether a stray event
fires on the wrong (reconnected) session. Each is a defense-in-depth
recheck folded into an existing call site rather than an independent
bail-out point, so none add to the site tallies below (matching the
established convention for this class of fix, e.g. round 8 Finding 2 and
round 9's RC21/RC23 retransmit recheck):

- **R1**: `handle_start_comm`'s mode-0 `start_periodic_message` `Err` arm
  (`ctx.api.lock().await` is the `.await` in question) now skips
  `PduErrEvtTesterPresentError` when stale.
- **R2**: `send_idle_tester_present_once` gains an `Option<u64>` generation
  parameter. Its two call sites that originate from a generation-carrying
  COP (`handle_start_comm`'s mode-1 branch, `handle_update_param`'s
  tester-present re-arm block) pass `Some(connect_generation)`, gating the
  function's own `Err`-arm event on a fresh `still_on_this_channel` check.
  Its third call site (`dispatch_due_idle_tester_present`) passes `None`
  unchanged -- that call site's existing `armed_at`-based staleness
  mechanism is already correct and adequate, and must not be duplicated.
  `frame_tester_present_data` gets the identical treatment (an `Option<u64>`
  parameter, `Some` from `handle_update_param`'s re-arm block since
  `promote_unique_resp_id_table`'s I/O sits immediately before it, `None`
  from `handle_start_comm`'s call site since it runs immediately after
  Guard A2 with no intervening `.await`).
- **R3**: `wait_for_expected_response`'s terminal `PduErrEvtRxTimeout`
  emission (the "window closed without enough matches" case) is now gated
  on a fresh `still_on_this_channel` check; the function still returns
  `ReceivePhaseOutcome::CycleComplete` either way -- the caller's S4 guard
  remains the sole owner of the actual terminal COP status.

**R4: one accepted, out-of-scope residual, documented rather than fixed.**
`promote_unique_resp_id_table`'s own internal window in `rpc_link.rs`: a
reconnect completing during its `FLOW_CONTROL_FILTER` I/O can re-land the
same Active-table write on the new session. Judged not worth the plumbing
-- the new session's own connect already installs its own filters
independently, so the re-landed write is redundant rather than harmful.
Alongside it, two already-existing, deliberately-unconditional
channel-scoped emissions remain correct as-is and are not gaps:
`handle_stop_comm`'s `stop_periodic_message` `Err` event, and
`handle_start_comm`'s orphan-periodic-stop-failure event -- both report a
rogue hardware periodic message that affects the live physical channel
regardless of which CLL's COP happens to be reporting it, so gating them on
any single CLL's generation would be wrong. The already-documented
post-check no-I/O emission micro-windows from earlier rounds remain
accepted residuals too, unchanged by this round.

Counting all of the above: `handle_start_comm` now has ten
`still_on_this_channel`-predicate sites (up from eight), `handle_stop_comm`
still has three, `handle_send_recv` (including `wait_for_expected_response`)
still has six, `handle_update_param` still has two, `handle_delay` gains two
(newly covered), and `handle_restore_param` gains one (newly covered), for
twenty-four sites total across the six handlers.

## Consequences (round 11 addendum)

- (PR #90, round 11) A `Temp`-binding `CoptStartcomm` whose
  `apply_params_to_hardware` call spans a disconnect+reconnect race, on
  either outcome (failure or success) of that call, no longer sends real
  K-line wakeup traffic, writes DATA_RATE, or delivers a synthetic response
  for a session the client was already told was `Cancelled` -- closing the
  last gap in the PR #90 K-line-init amendment above.
- (PR #90, round 11) A queued `TxItem::Delay`/`TxItem::RestoreParam` that
  goes stale while still sitting in the poll task's FIFO is now skipped
  before dispatch, via the same pre-dispatch mechanism that already
  protected every other generation-carrying variant.
- (PR #90, round 11) A dispatched, in-flight `CoptDelay` that goes stale
  mid-sleep is now ended early (per-tick) and, in every case (including
  `delay_ms == 0` and a last-tick race), routed through a single terminal
  recheck that emits `PduCopstCancelled` instead of a contradictory
  `PduCopstFinished` when stale -- verified by a genuine regression test,
  the first in this ADR's history for an in-handler guard.
- (PR #90, round 11) `CoptRestoreParam` now distinguishes "link missing"
  from "stale" the same way every other handler does (first-wins
  `PduCopstCancelled` for both), instead of silently no-op'ing on
  link-missing while remaining fully unguarded against staleness.
- (PR #90, round 11) Three narrower "stray event on the wrong session"
  gaps (R1-R3) are closed as defense-in-depth rechecks, not independent
  guard sites -- none affect any COP's terminal status.
- (PR #90, round 11) One residual (R4, `promote_unique_resp_id_table`'s own
  `FLOW_CONTROL_FILTER` window) is accepted and documented rather than
  fixed, alongside two pre-existing deliberately-unconditional
  channel-scoped emissions that were re-confirmed correct as-is, not gaps.

### Pre-commit `edge-case-hunter` catch, round 11

An `edge-case-hunter` pass run against this round's diff before it was
committed (matching the same practice from round 9) found one real gap R2
missed and one test-coverage weakness, neither caught by the design-advisor
audit or the implementer's own verification:

**A fourth `send_idle_tester_present_once` call site was left ungated.**
`dispatch_due_idle_tester_present`'s own call passed `generation: None`, on
the theory that its pre-call `still_due` check (which already validates
`channel_id`/`armed_at`/interval before the call) made a post-call recheck
redundant. That reasoning only covers entry into the call —
`transmit_request`'s own `ctx.api.lock().await` inside
`send_idle_tester_present_once` is a real hardware `.await` a
disconnect+reconnect can complete during, the exact class of race R1-R3
already fixed for this function's other emission sites. With `generation:
None`, the `Err` arm's own recheck short-circuits to always-true, so a stale
idle-tester-present send's failure would unconditionally fire
`PduErrEvtTesterPresentError` — updating `last_error` and notifying the
*reconnected* session for a send that belonged to the old one. Fixed by
capturing the live `connect_generation` in the same critical section that
already computes `still_due` (changing its type from `bool` to
`Option<u64>`, where `Some` carries the live generation) and threading it
through as `Some(..)` instead of `None` — the same mechanism the other two
`send_idle_tester_present_once` call sites already use, no new plumbing
required.

**The `handle_delay` regression test only proved the per-tick and terminal
checks don't double-emit together, not that the per-tick check does
anything on its own.** `cancel_link_cops`'s own first-wins removal from
`primitives` at disconnect time means the terminal recheck alone is
sufficient to suppress the original test's one absence assertion even with
the per-tick `is_stale` short-circuit fully reverted — the loop would just
run to its natural deadline instead of breaking early, then the (still
correct) terminal check would find the COP already removed and emit
nothing. Strengthened by giving `cll_b` (already created and kept alive to
keep the shared channel open) its own event subscription and, immediately
after the reconnect, queuing a `CoptDelay(0)` on it: since `cll_a`'s stale
delay and `cll_b`'s new one share the same physical channel's poll-task
FIFO, `cll_b`'s item can only dispatch promptly (well under 250ms) if the
per-tick check actually broke `cll_a`'s stale delay out of its loop early —
without it, `cll_b`'s item would sit blocked behind `cll_a`'s delay for the
remainder of its original, several-hundred-millisecond duration.

### Amendment (PR #90, round 12, Codex findings): the round-11 fixes' own
### blind spots -- a post-await window inside `handle_update_param`, and an
### event-emission window inside a "freshly-entered" call

Two more findings, both against the round-11 commit, both the same shape as
round 9/10/11's own recurring lesson: a fix closes one `.await`'s race
window and, in doing so, reveals that a *sibling* `.await` — inside the very
same code path, sometimes inside a function the fix just finished wiring up
— was never covered.

**Finding 1 (P2): `handle_update_param`'s `promote_unique_resp_id_table`
call had no recheck of its own.** U2 (round 9) guards the `active` write and
the decision to call `promote_unique_resp_id_table` at all, but
`promote_unique_resp_id_table` itself — on an ISO15765 link with a changed
UniqueRespIdTable — awaits FLOW_CONTROL_FILTER teardown/install, a real
hardware `.await` no less capable of spanning a disconnect+reconnect than
any other in this file. Nothing re-checked staleness after it returned:
execution fell straight into the tester-present re-arm block (using the
pre-promotion `rearm_snapshot`, silently stale) and then the unconditional
terminal `PduCopstFinished`. Fixed with a three-way post-promotion check,
matching U1's own "link missing" precedent rather than inventing a new one:
link missing (`None`) falls through unchanged (`PduCopstFinished` below,
re-arm already skipped via `rearm_snapshot`'s own outer `None`) — changing
this would have regressed U1's documented "link missing → still Finished"
behavior for no reason, since a missing link was never itself evidence of a
stale *generation*; stale (`Some(false)`) bails first-wins with
`PduCopstCancelled` before the re-arm block or terminal status; live
(`Some(true)`) proceeds exactly as before.

**Finding 2 (P2): `handle_start_comm`'s own `frame_tester_present_data` call
passed `generation: None`, on a subtly wrong justification.** Round 11's R2
fix added a `generation: Option<u64>` parameter to `frame_tester_present_data`
specifically so its internal software-ISO-TP-size-check failure arm could
gate its own `send_error_event(...).await` against staleness. The round-11
amendment reasoned that `handle_start_comm`'s call site could pass `None`
because it runs immediately after Guard A2 with zero intervening `.await` —
true, but beside the point: that reasoning protects the *call's entry*, not
the `.await` that happens *inside* the callee once entered. A disconnect+
reconnect landing during `frame_tester_present_data`'s own
`send_error_event` call — which can only happen once already inside the
function, regardless of how fresh the call's entry was — would still record
a stray `PduErrEvtTesterPresentError` on the reconnected session. Fixed by
passing `Some(connect_generation)` at this call site too, matching
`handle_update_param`'s tester-present re-arm call site exactly; the
function's own doc comment is corrected to state the actual rule
("entry-freshness does not protect an `.await` inside the call").

Neither finding adds a new `still_on_this_channel`-predicate site by this
ADR's counting convention: Finding 1 is folded into the existing post-U2
control flow (not an independent guard elsewhere in the function), and
Finding 2 is a parameter-value correction at an existing call site, not a
new guard. Site tally is unchanged at twenty-four.

## Consequences (round 12 addendum)

- (PR #90, round 12) A `CoptUpdateparam` that goes stale during
  `promote_unique_resp_id_table`'s own FLOW_CONTROL_FILTER I/O no longer
  runs the tester-present re-arm block against a stale snapshot or falls
  through to a contradictory `PduCopstFinished` — while still preserving
  the pre-existing "link missing" behavior unchanged.
- (PR #90, round 12) `handle_start_comm`'s software-ISO-TP tester-present
  size-check failure event is now correctly gated on the captured
  `connect_generation`, closing the same class of "stray event on the
  reconnected session" gap R1-R3 closed elsewhere in round 11 — this call
  site was simply missed by an incorrect (though independently reviewed and
  confirmed at the time) freshness argument.
- Test coverage: same infeasibility class as every other in-handler guard in
  this file — no dedicated regression test for either finding. The full
  existing test suite (233 tests) was confirmed to still pass unchanged with
  both fixes in place.

### Amendment (PR #90, round 13, Codex finding): round 12's `frame_tester_present_data`
### fix gated the wrong thing — the return path itself, not just the internal event

Round 12 threaded `connect_generation` into `frame_tester_present_data` so
its own internal `send_error_event(...).await` (the software-ISO-TP
size-check soft-failure arm) could gate itself against a disconnect+
reconnect landing during that specific `.await`. That closed the stray-event
half of the problem, but Codex correctly points out it left the other half
open: the helper always returns a plain `Vec<u8>` regardless of whether it
detected staleness internally — it has no way to tell its caller "a
reconnect happened while I was awaiting, treat this COP as cancelled." Both
callers, on return, continue straight ahead as if nothing happened:

- `handle_start_comm` walks into `run_protocol_init` — the exact irreversible
  K-line wire traffic Guard A/Guard A2 exist specifically to keep a stale COP
  from reaching.
- `handle_update_param`'s tester-present re-arm block falls through, at the
  end of its own multi-`.await` sequence (this call, `wait_for_p3_gap`,
  `send_idle_tester_present_once`), to the function's single, previously
  fully-unconditional terminal `PduCopstFinished`.

**Fix for `handle_start_comm`**: a new **Guard A3**, immediately after
`frame_tester_present_data(...).await` returns and before the tester-present
data is used for anything further. Same revert-then-bail shape as Guard A2
(revert to the live Active set first, only when `binding` was `Temp` — the
temp config may already be on hardware; unconditional regardless of
staleness, same ADR-067 obligation), then the standard first-wins bail.

**Fix for `handle_update_param`**: rather than chasing each of that block's
remaining unguarded `.await`s individually (`frame_tester_present_data`'s own
internal event `.await`, `wait_for_p3_gap`'s `.await`,
`send_idle_tester_present_once`'s `.await`, and even `send_error_event`'s
`.await` in the pre-existing `!all_ok` branch above), a single **terminal
recheck** immediately before the function's shared final `PduCopstFinished`
closes all of them at once — the same "protect the terminal status, not
every individual path to it" strategy `handle_send_recv`'s S4 and
`handle_stop_comm`'s terminal block already use. Three-way, matching U1's own
"link missing" precedent exactly like the round-12 post-promotion check:
link missing (`None`) still emits `PduCopstFinished` unchanged; stale
(`Some(false)`) bails first-wins `PduCopstCancelled`; live (`Some(true)`)
emits `PduCopstFinished` as before. This is NOT redundant with round 12's
post-promotion check — that one protects the re-arm block's own side effects
(the actual bus send) from running under a stale snapshot; this one protects
the terminal status emission itself against staleness introduced anywhere in
the block that ran after it, including inside `frame_tester_present_data`,
which the post-promotion check runs too early to catch.

Both fixes are genuine new sites, unlike round 12's two findings (which were
folded into existing control flow / an existing call-site's parameter):
Guard A3 brings `handle_start_comm` to eleven sites (up from ten after round
11). `handle_update_param`'s new terminal recheck brings it to three (up
from two, unchanged through round 12). Updated tally: twenty-six
`still_on_this_channel`-predicate sites across the six handlers.

## Consequences (round 13 addendum)

- (PR #90, round 13) A `CoptStartcomm` whose tester-present payload triggers
  the software-ISO-TP size-check soft-failure path, racing a disconnect+
  reconnect during that path's own event emission, is now correctly
  cancelled before reaching `run_protocol_init`'s irreversible wire traffic —
  round 12's fix alone only suppressed the stray event, not the COP's
  continued execution.
- (PR #90, round 13) A `CoptUpdateparam` whose tester-present re-arm block
  goes stale at any point during its own multi-`.await` sequence — not just
  during `promote_unique_resp_id_table`, which round 12 already covered — no
  longer falls through to a contradictory `PduCopstFinished`.
- Test coverage: same infeasibility class as every other in-handler guard in
  this file. The full existing test suite (233 tests) was confirmed to still
  pass unchanged with both fixes in place.

### Amendment (PR #92 review round, Codex finding): RX attribution itself was
### never generation-aware -- a gap in the shared engine, not in any
### `still_on_this_channel` guard

Every guard documented above protects a *decision point* -- whether to
transmit, write back state, or emit a terminal status -- by re-checking
`channel_id`/`connect_generation` immediately before the guarded action. None
of them protect the *shared RX attribution pipeline* itself:
`poll_rx_inner`'s `MatchProbe` arm, which decides whether a matched frame's
`ResultData`/`cop_handle` is attributed to the probing COP, was keyed on
`entry.handle == p.target_cll` alone -- plain `cll_handle` equality, no
`connect_generation` comparison at all. `wait_for_expected_response`'s own
per-pass staleness check (the `(was_cancelled, is_stale)` block documented in
round 9 above) runs at the *bottom* of its loop, strictly after that same
pass's `poll_rx_and_check_match` call has already run and already decided
attribution -- and, when a match is found with `matches_needed` satisfied,
the function `return`s `CycleComplete` immediately, skipping that pass's
staleness check entirely. A same-channel disconnect+reconnect of
`target_cll`, completing between one pass's staleness check and the next
pass's attribution decision (or landing on the very first pass after
reconnect, before staleness is ever rechecked), let a fresh session's first
matching frame be delivered as `ResultData` under the OLD, stale COP's
`cop_handle` -- for both `CoptSendrecv` and `CoptStopcomm` alike, since both
share this one engine. Found by Codex review of PR #92's ADR-087 addition
(the StopComm round-7/ADR-087 S3 guard, and this same gap already latent for
`CoptSendrecv`, which has the identical S3-guard-then-loop shape): the PR
simply added a second call site (`CoptStopcomm`'s receive phase) to a
pre-existing gap in the shared engine, not a StopComm-specific bug.

**Decision: generation-aware attribution, threaded through the same snapshot
that already carries `target_cll`.** `CllRxEntry` (the per-CLL routing
snapshot `build_cll_rx_entries` produces, consumed by `poll_rx_inner`) gains
a `connect_generation: u64` field, populated from `l.connect_generation`
inside the SAME `logical_links` lock acquisition `build_cll_rx_entries`
already holds to build every other field -- no new lock. `MatchProbe` (the
per-call probe state `poll_rx_and_check_match` builds and threads into
`poll_rx_inner`) gains the identical field, populated from
`ExpectedResponseWait::connect_generation` -- the same call-time-captured
value (ADR-086, main decision above) every other guard in this receive-phase
engine already trusts, threaded through `poll_rx_and_check_match`'s new
parameter with no fresh `logical_links` read of its own.
`poll_rx_inner`'s `MatchProbe` arm is extended from `entry.handle ==
p.target_cll` to `entry.handle == p.target_cll && entry.connect_generation ==
p.connect_generation`, gating BOTH of its sub-branches -- the plain
expected-response match AND the pending-RC (0x78/0x21/0x23) detection -- not
just one of them; a pending-RC frame from a fresh session must not be allowed
to extend or re-request against a stale COP's wait either, or the narrower
bug would simply move from "final match misattributed" to "pending-RC
misattributed".

An alternative -- adding a "check immediately before the `poll_rx_and_
check_match` call" guard inside `wait_for_expected_response`'s loop, mirroring
every other `still_on_this_channel` site in this ADR -- was rejected: it is
exactly the class of fix this ADR's own prior rounds (9, 10, 12) already
learned does not work for this shape of bug. `.await`s remain between such a
check and the actual attribution decision (`build_cll_rx_entries`'s own
`logical_links` lock acquisition, `ctx.api.lock().await` inside
`poll_rx_inner`), so a pre-call check narrows the window but cannot close it
-- the same lesson already stated for every `.await`-spanning guard elsewhere
in this file. Fixing the attribution layer itself, rather than adding another
call-site guard, closes the window unconditionally regardless of how many
`.await`s separate the check from the decision, and fixes `CoptSendrecv` and
`CoptStopcomm` simultaneously since both share the one engine.

The pre-existing loop-bottom `(was_cancelled, is_stale)` staleness check is
unchanged and remains the sole authority for *terminating* the wait; this
round's fix addresses a different concern -- preventing *misattribution*
while the wait is still legitimately running, which the termination check
structurally cannot do since it runs after attribution has already happened
for that pass. `poll_rx` (the plain background-poll function used outside any
receive-phase wait) is unaffected: it always passes `probe: None`, so the new
comparison inside the `Some(p)` arm never evaluates for it.

`ADR-087`'s own "Guards" section previously described its pre-receive S3-
equivalent `still_on_this_channel` check as closing this exact window; that
claim is corrected there (see that ADR's amendment) -- the S3 guard remains a
useful, cheap early bail for a reconnect that has already completed by the
time it runs, but it does not and cannot close the window for any later pass,
which is what this amendment actually fixes.

**Test coverage**: unlike most in-handler guards in this ADR's history (which
this file's own infeasibility notes document extensively --
`j2534-0404-service/docs/implementation-notes.md`), `wait_for_expected_
response`'s poll loop has a genuine `tokio::time::sleep` between passes,
giving a real (if narrow) natural preemption point. `stopcomm_data_tx.rs`'s
`stopcomm_receive_phase_reconnect_mid_wait_does_not_misattribute_new_frame_
to_stale_cop` starts a `CoptStopcomm` with `NumReceiveCycles = 1` and an
`expected_response_array` entry against a mock ECU that has not yet
answered, disconnects and immediately reconnects the SAME `cll_handle` at the
SAME `DATA_RATE` (kept alive via a sibling CLL sharing the physical channel,
the same technique round 11's `handle_delay` test uses) with zero
intervening event-stream reads between the reconnect and an immediate,
synchronous response-frame injection (minimizing the number of `.await`
points between the generation bump and the injected match, since each
intervening `.await` is a scheduling opportunity for the stale wait's own
next pass to run to completion first and self-terminate via its loop-bottom
staleness check before the injected frame is ever seen), then asserts the
resulting `ResultData` event's `cop_handle` is not the stale COP's. Confirmed
to fail (observing the misattributed `ResultData`) in 9 of 10 isolated runs
with the `entry.connect_generation == p.connect_generation` conjunct
reverted, and to pass in 15 of 15 isolated runs with the fix restored -- the
9/10 (not 10/10) failure rate under the reverted condition reflects genuine
wall-clock/scheduling variance inherent to racing a real TCP loopback
round-trip (`DisconnectComLogicalLink`/`ConnectComLogicalLink` are real gRPC
calls) against the poll loop's ~10ms (`POLL_INTERVAL_MS`) cadence on this
crate's single-threaded `current_thread` test runtime, not test flakiness on
correct code: the fixed code passed every single isolated run attempted (15
in a row), so the test never spuriously fails against the actual fix -- only
its power to catch a *reverted* fix is occasionally (roughly 1 in 10 runs)
lost to timing, which is disclosed here rather than hidden.

## Consequences (this round's addendum)

- A same-channel disconnect+reconnect of a CLL with an in-flight receive
  phase (`CoptSendrecv` or `CoptStopcomm`) landing between one poll pass's
  loop-bottom staleness check and the next pass's RX attribution decision no
  longer misattributes a fresh session's frame -- `ResultData`, `cop_handle`,
  and (for a pending-RC frame) the pending-RC-driven deadline extension/
  re-request -- to the stale COP.
- The loop-bottom staleness check remains the sole termination authority,
  unchanged; this round closes a misattribution window distinct from, and
  structurally unreachable by, that check.
- `poll_rx` (plain background polling, `probe: None`) is unaffected -- zero
  behavior or performance change for any CLL with no active receive-phase
  wait.
- ADR-087's "Guards" section is corrected to no longer claim its S3-
  equivalent guard closes this window; it remains in place as a narrower,
  defense-in-depth early exit, with the actual fix cited to this amendment.
- This is the first genuine regression test for a misattribution race in
  this ADR's entire history (as opposed to the termination-side races rounds
  9/11 already have tests for) -- made possible by the receive-phase poll
  loop's real `tokio::time::sleep`, the same natural-preemption-point
  property that made round 11's `handle_delay` test possible. Its
  bug-catching reliability is empirically ~90%, not 100%, for the reasons
  described above; it never fails against correct code.

See also: ADR-083 (`handle_start_comm`'s original mode-0/Step-3
`still_on_this_channel` guards), ADR-084 (the mode-1 guard reusing the same
pattern), ADR-085's round-7 amendment (`handle_stop_comm`'s guards, the
concrete case Codex flagged), ADR-087 (`CoptStopcomm`'s non-cancellable
receive phase, whose S3 guard's doc comment and "Guards" section are amended
alongside this round).

### Amendment (PR #92 review round, edge-case-hunter finding): stale
### software-ISO-TP reassembly state also survives a reconnect -- a related
### but distinct gap in a different mechanism

Same review pass as the amendment directly above (Codex's RX-attribution
finding against PR #92's ADR-087 addition); this is a second, related gap an
edge-case-hunter review of that same fix surfaced, in a different mechanism
entirely. The amendment above closes *attribution* -- which stale-or-live COP
a matched frame's `ResultData`/`cop_handle` is credited to. It does not, and
by design cannot, protect the *reassembly buffer* a software-ISO-TP frame is
matched against in the first place: `LogicalLinkState.isotp_rx` (the
per-CAN-ID `HashMap<u32, isotp::Reassembly>` used to reassemble segmented
messages in software, ADR-046), shared with the poll task like `rx_buf`, is
allocated once at `CreateComLogicalLink` and was never cleared afterward --
not at reconnect, not at disconnect.

Concretely: a software-ISO-TP CLL with a FirstFrame-seeded partial reassembly
in progress (`isotp_rx[can_id]` holding a `Reassembly::start`'d entry, not yet
completed) at the moment of a disconnect keeps that entry alive across a
same-`cll_handle` reconnect onto the same shared physical channel (the exact
scenario this ADR's main decision and every round above already treats as the
canonical "stale state must not survive a reconnect" case). If a
ConsecutiveFrame then arrives on the same CAN ID after the reconnect -- well
within the ~1000ms N_Cr timeout (`isotp::Reassembly`'s default deadline) --
`events.rs`'s `on_consecutive` handling completes the reassembly by splicing
the PRE-reconnect FirstFrame's bytes with the POST-reconnect
ConsecutiveFrame's bytes into one corrupted, "Frankenstein" `ResultData`. This
frame is delivered with the correct, LIVE `connect_generation` at completion
time (the reassembly completes during the new session, after all), so the
attribution fix in the amendment above does not reject it -- that gate
protects attribution, not reassembly-buffer integrity, which this amendment
addresses separately.

**Decision: clear `isotp_rx` at the same point `connect_generation` is
stamped.** `finalize_connected_link` (`rpc_link.rs`) is already the single
correct point: it stamps a fresh `connect_generation` on every finalized
connect, including a reconnect of the same `cll_handle` onto the same
physical channel (this ADR's main decision), inside a `logical_links.lock()`
critical section. It now also clears the CLL's `isotp_rx` map in that same
place, so a fresh generation never inherits a partial reassembly left over
from a previous one.

`isotp_rx` is `Arc<Mutex<HashMap<u32, isotp::Reassembly>>>` -- a separate
`Mutex` from `logical_links`. The fix clears it via `link.isotp_rx.lock().await.clear()`
*inside* the same `logical_links` critical section that stamps
`connect_generation`, rather than cloning the `Arc` and clearing it after the
`logical_links` guard is dropped (an earlier version of this fix used that
clone-then-clear-outside shape, on the theory that nesting a second lock
inside `logical_links` was a pattern to avoid). Nesting `isotp_rx` inside
`logical_links` here is safe and, on review, preferable: the crate's only
other `isotp_rx` lock sites (`events.rs`'s FirstFrame/ConsecutiveFrame
reassembly handling) acquire no other lock while holding it, so
`logical_links -> isotp_rx` is the only ordering edge between the two
mutexes anywhere in the crate -- no reverse edge, no cycle, no deadlock risk.
Clearing inside the lock also closes the window fully rather than merely
narrowing it: `build_cll_rx_entries` (`events.rs`) also takes
`logical_links` to build its RX snapshot, so any poll-task pass able to
observe the new `connect_generation` is necessarily built *after* the clear
completes -- a legitimate post-reconnect frame can never race the clear and
be wiped, which the clone-then-clear-outside shape could not fully
guarantee (a concurrent poll-task pass could, in principle, observe the new
generation and deliver a frame in the gap between the `logical_links` guard
dropping and the cloned handle's own lock being acquired). This correction
was made during PR #92's review pass, per an edge-case-hunter finding on the
original clone-then-clear-outside shape and a follow-up design-advisor
confirmation that nesting is deadlock-safe here.

`DisconnectComLogicalLink` deliberately gets no matching clear. The
vulnerable window is specifically "stale state surviving *into* a new
connection generation" -- closed entirely by clearing at the reconnect point,
since nothing can reassemble a frame while the CLL is disconnected in the
first place. Clearing at disconnect too would be harmless but redundant, not
a second half of the fix.

**Test coverage**: `can_mode.rs`'s
`software_isotp_reconnect_clears_stale_reassembly_state` seeds a partial
reassembly with a FirstFrame on a distinctive byte range, disconnects and
reconnects the SAME `cll_handle` at the SAME `DATA_RATE` (kept alive via a
sibling CLL sharing the physical channel, the same technique the amendment
above and round 9's family of tests use), then injects a ConsecutiveFrame on
the same CAN ID that would complete the stale reassembly if it survived.
Asserts no `ResultData` is ever delivered for it (a stray ConsecutiveFrame
with no live reassembly entry is correctly dropped, per
`events.rs`'s pre-existing "stray CF with no reassembly in progress" path)
and that no spurious FlowControl reply goes out for it either. It then
completes a genuinely fresh FirstFrame+ConsecutiveFrame sequence after the
reconnect and asserts the resulting payload is exactly the fresh bytes, with
none of the stale or stray bytes mixed in. Verified to fail with the
`isotp_rx.lock().await.clear()` call removed: the test observed exactly the
predicted corruption, `ResultData.data_bytes` equal to the stale FirstFrame's
first six bytes followed by the stray ConsecutiveFrame's four bytes, spliced
across the two sessions as described above. Passes with the clear restored.

## Consequences (this round's addendum)

- A same-channel disconnect+reconnect of a software-ISO-TP CLL with a
  partial reassembly in progress at disconnect time no longer lets a later,
  unrelated ConsecutiveFrame on the same CAN ID complete it after the
  reconnect -- `isotp_rx` starts empty for every fresh `connect_generation`.
- This is a different mechanism from, and does not overlap in code with, the
  RX-attribution fix in the amendment directly above; both were needed to
  fully close "stale per-session state surviving a reconnect" for the
  software-ISO-TP receive path.
- `DisconnectComLogicalLink` is unchanged -- the fix is reconnect-side only,
  by design (see rationale above).

See also: the amendment directly above (same PR #92 review pass, the
RX-attribution gap in the same receive path); ADR-046 (software-ISO-TP mode
and `isotp_rx`'s origin);
`j2534-0404-service/docs/implementation-notes.md` (this fix's backlog entry).

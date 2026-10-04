# ADR-193: TP2.0 Broadcast-Periodic Start/Cancel Serialization Fence

**Date:** 2026-08-25
**Status:** Accepted
**Affects:**
- `j2534-0404-service/src/service/rpc_primitive.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`
- `j2534-0404-service/src/service/rpc_link.rs` (amendment, Codex review round 17, P1, PR #101 —
  comment-only invariant pin in `rpc_destroy_com_logical_link`)
- `j2534-0404-service/src/service/events.rs` (amendment, Codex review round 16, P1, PR #101)
- `j2534-0404-service/src/service/rpc_module.rs` (amendment, Codex review round 17, P1, PR #101)
- `j2534-0404-service/src/service.rs`
- `j2534-0404-service/docs/implementation-notes.md`
- ADR-192 (Status line, Decision item 2's in-flight-reservation mechanism partially superseded)

## Context

ADR-192 Decision item 2 established `Tp20BroadcastPeriodic { message_id: None, .. }` as an
out-of-band "a native `PassThruStartPeriodicMsg` start is in flight" sentinel:
`reserve_tp20_broadcast_periodic` (`rpc_primitive.rs`) writes it under `self.logical_links`
alone and releases that lock; `rpc_start_com_primitive`'s broadcast-periodic branch then
acquires `self.api`, applies any temp-bound ComParams, issues the native start, and only
afterward (`finalize_or_orphan_broadcast_periodic_start`) reconciles the sentinel against
whatever actually happened.

Every terminator this mechanism defines — `CoptCancel`, TX-dispatch suspension termination,
CLL teardown — treated a `None`-sentinel entry as "nothing real to stop yet": it took the
entry and reported the owning COP terminal (`Cancelled`/`Finished`, or a successful suspend)
holding nothing but `self.logical_links`. That lock has no relationship at all to
`self.api`, which the in-flight start was still holding (or had not yet acquired) at that
exact moment. A Codex review (PR #101, round 15, P1) identified the resulting race: a
terminator could complete and report the COP terminal to the client while
`rpc_start_com_primitive` was still queued behind `self.api`, and once that call finally
acquired it, the native start still ran and still emitted SAE J2534-2 clause 19.3.2.3's
five-frame burst synchronously — transmission after a reported cancel, suspend, or teardown.
The existing after-the-fact reconciliation in `finalize_or_orphan_broadcast_periodic_start`
could only stop *further* transmission (the periodic continuation); by the time it ran, the
initial burst was already on the wire and could not be recalled.

This is the fourth-plus round of this PR's review loop to find a genuine gap in this
mechanism (following ADR-192's own round-3 design-advisor consult for the suspension×
broadcast-periodic interaction, and rounds 9-14's session-identity/leak-tracking/error-code
fixes) — per this repo's own escalation practice, it was routed to a fresh `design-advisor`
consult for a holistic mechanism review rather than another narrow patch.

## Decision

**`self.api`'s mutex becomes the serialization fence for every in-flight broadcast-periodic
start.** No path may report a `None`-sentinel reservation's owning COP terminal — or decide
whether a native stop is owed for it — without holding `self.api` first. (Taking the entry off
`link.tp20_broadcast_periodic` is a separate act: it needs no fence, and for a terminator that
also clears session identity it must NOT be fenced — see item 3 and the correction paragraph at
the end of this section.) Four coordinated pieces implement this:

**1. Start-side revalidation.** `rpc_start_com_primitive`'s broadcast-periodic branch now
calls a new `revalidate_tp20_broadcast_periodic_reservation` immediately after acquiring
`self.api` and strictly before the temp-param apply / native start — re-running
`reserve_tp20_broadcast_periodic`'s ownership/session predicates (still this cop_handle's own
`None`-sentinel, `connect_generation` match, `connected`, `!tx_suspended()`) via a
`self.logical_links` read nested inside the held `self.api` guard (ADR-110's sanctioned
`api`-outer/`logical_links`-inner order). It deliberately does NOT re-check
`reserve_tp20_broadcast_periodic`'s own `channel_id.is_some()` arm, so it is not an exact
mirror of that function's predicate list — the omission is benign, since `connect_generation`
+ `connected` already cover transitively every way this CLL could have lost its channel.
Either failure outcome drops `api` and returns without ever calling the native start, but they
differ in who still owns this COP's cleanup:

- **This cop_handle's own reservation is no longer there** (`AlreadyResolved`) — covering all
  three shapes: the CLL vanished from `logical_links` entirely (a completed
  `DestroyComLogicalLink`), a DIFFERENT cop_handle now owns the slot, or this cop_handle's
  entry was genuinely taken by some other path. In every shape, whoever acted (not necessarily
  a path that has REPORTED anything yet: a suspension terminator takes the entry under
  `logical_links` alone and can still be queued behind `self.api` at this exact moment)
  now holds sole responsibility for finalizing this cop_handle. So this exit touches nothing:
  no `logical_links` cleanup is possible or needed (the entry being gone is what this outcome
  means), and `self.primitives` is deliberately left alone, mirroring
  `finalize_broadcast_periodic_start_bookkeeping`'s `Resolution::NotOwned` arm, which does
  nothing for exactly the same reason one step later in the same bracket. The RPC returns `Ok`
  (silent success) without emitting any status of its own. Removing the `primitives` entry
  here instead — which an earlier draft of this fix did, by calling
  `rollback_tp20_broadcast_periodic_reservation` unconditionally — destroyed the terminator's
  ability to report anything at all: every terminator's finalization is gated on that entry
  still being present (`events::emit_terminal_if_live`, `CoptCancel`'s own
  `prims.remove(..).is_some()`), so the COP vanished with no `Cancelled`/`Finished` and no
  `terminal_cops` record, and a later `GetStatus`/`CancelComPrimitive` reported
  `PDU_ERR_INVALID_HANDLE` instead of ADR-128's already-terminal no-op. This exit DOES still
  perform ADR-067 claim D's `temp_param_update` Working-writeback, explicitly and by design:
  it is an early `Ok` return that never reaches `rpc_start_com_primitive`'s own `Ok` tail, and
  claim D's criterion is that the RPC ACCEPTED the COP, not that hardware consumed the staged
  snapshot (`CoptStopcomm` writes back while touching no hardware at all). Skipping it — which
  an earlier draft of this fix did, reasoning that the temp-bound snapshot was never applied —
  left the client's staged Working values in place after a call it was told succeeded, so a
  later `CoptUpdateparam` would promote values that different race timing would have discarded.
- **A different predicate broke with the entry still nominally this cop_handle's own** — no
  one else has taken over, so this call keeps the cleanup duty and rolls back both halves
  itself (`rollback_tp20_broadcast_periodic_reservation`, which is also what the later
  in-bracket failure paths use, and stays ownership-gated so it can never clobber another
  cop_handle's fresh reservation). It returns `Err`, the same shape
  `reserve_tp20_broadcast_periodic`'s own initial check already returns.

**2. `finalize_or_orphan_broadcast_periodic_start` split by lock ownership.** The function is
now two: `finalize_or_orphan_broadcast_periodic_start_locked` performs the
`self.logical_links` resolution (commits the real `message_id` over the sentinel, or detects
`NotOwned` and issues the orphan-stop native call) *while the caller's `self.api` guard from
step 1 is still held*, immediately after the native start returns — so a competing terminator
can never observe a half-committed state. `finalize_broadcast_periodic_start_bookkeeping`
performs everything that needs no fence — the `primitives`/`dispatched` update and the COP
status emissions (`emit_nonterminal_if_live`/`emit_terminal_if_live`, ADR-192's round-12 fix)
— strictly after `self.api` is dropped.

**3. A shared terminator-side primitive.** `take_broadcast_periodic_under_api_locked`
(`rpc_misc.rs`) takes an already-held `self.api` guard as proof-of-fence (a `_locked`-suffixed
shape, this crate's established convention for "caller already holds the lock" variants) and,
under `self.logical_links` nested inside it, resolves to exactly one of: the entry is still
this cop_handle's `None`-sentinel — take it, no
native call needed, safe to report terminal because the start's own revalidation (step 1) can
now only find it gone; the entry is now a committed `Some(real_id)` — take it and run the
caller's ordinary live-stop machinery for that id, exactly as before; the entry is absent (or
owned by a different cop_handle) — another resolver already acted, no-op. It is **id-targeted
only**: `CoptCancel` (`rpc_primitive.rs`) is its sole caller, acquiring `self.api` first. A
CLL-wide teardown deliberately does not use it — see the correction paragraph at the end of
this Decision section. `terminate_tp20_broadcast_periodic_for_suspension`
(`rpc_misc.rs`) independently acquires `self.api` itself as the same fence rather than calling
the shared primitive, since its own reconciliation shape (`Finished`, not `Cancelled`) and its
four existing callers' `channel_id` capture don't fit the shared helper's exact return shape
without their own separate translation.

Its four callers (`ioctl_suspend_tx_queue` and three `CP_SuspendQueueOnError` transition sites
in `events.rs`) are deliberately unchanged: each still TAKES the entry off
`link.tp20_broadcast_periodic` under `self.logical_links` alone, in the same critical section
that sets the suspension flag, before `self.api` is ever in scope. That unfenced take is not a
gap — it is load-bearing, and it is safe for three reasons together. (a) It only removes from
`logical_links`; it never touches `self.primitives`, so it never destroys the tracking a
terminal status depends on. (b) It is precisely what makes a racing start's own revalidation
(step 1) correctly detect "already resolved" and abort before transmitting — the take must be
visible to the start, and taking it in the flag's own critical section is what makes
"suspended" and "no broadcast periodic tracked" a single atomic fact. (c) What actually needs
the fence is not the take but the DECISION taken on the result: what to report, whether a
native stop is owed, and how `primitives` is finalized. That decision runs inside
`terminate_tp20_broadcast_periodic_for_suspension`'s own `self.api` acquisition, and the
racing start (finding revalidation-`AlreadyResolved`) leaves `primitives` untouched for that
decision to complete. Nothing between the unfenced take and the fenced decision reports
anything or mutates `primitives`, from either side.

`DestroyComLogicalLink` needs no change: it removes the CLL's `logical_links` map entry before
ever acquiring `self.api`, which leaves a racing start on exactly one of two equally safe
paths. If the start is already mid-bracket, its own in-bracket resolution (step 2) finds
`NotOwned` and orphan-stops the message under its own already-held guard — Destroy's own later
`self.api` acquisition naturally serializes after that. If the start has not yet reached its
revalidation (step 1), that revalidation resolves `AlreadyResolved` on the "CLL vanished
entirely" shape and aborts immediately: no native start is ever issued, and, as with every
`AlreadyResolved` exit, `self.primitives` is left untouched for whoever owns the cleanup.
Because the whole map entry goes at once, Destroy also cannot produce the partial-state window
the correction paragraph below describes for Disconnect. `CLEAR_PERIODIC_MSGS`
(`rpc_misc.rs`) needs no change either: its existing `pending_clear_generation` deferral
already defers resolving a `None`-sentinel entry rather than taking it outright, for a
different reason (it cannot yet know the eventual `started_epoch` to order itself against) —
that mechanism is unrelated to, and not weakened by, this fence.

**4. Composition with round 15's `ERR_INVALID_MSG_ID` fix.** `CoptCancel`'s native
`stop_periodic_message` call for a committed entry runs under the same `self.api` guard the
take happened under; `self.api` is dropped before the `ERR_INVALID_MSG_ID` check and the
restore-or-leak-track fallback, both of which need `self.shared_channels` (ADR-080's outermost
lock) and must never run while `self.api` is held.

**Correction (same round, second adversarial pass): a CLL-wide teardown fences the DECISION,
never the take.** The initial implementation of this ADR moved
`DisconnectComLogicalLink`'s `link.tp20_broadcast_periodic.take()` out of the early
`self.logical_links` critical section — the one that also clears `channel_id`, takes
`channel_key`, and sets `connected = false` — and down into the fenced section, routing it
through step 3's shared primitive. That was wrong, and it reintroduced this ADR's own bug class
through a different door. Between those two points the CLL was observable, under
`logical_links` alone, carrying a live `tp20_broadcast_periodic` alongside an already-`None`
`channel_id`/`channel_key`. Any terminator that reads the entry with only that lock —
`ioctl_suspend_tx_queue` and the three `CP_SuspendQueueOnError` sites, which capture
`(periodic, channel_id, connect_generation, channel_key)` together, and `CoptCancel`'s session
capture — would then take a committed `message_id` while capturing `channel_id: None`, find no
channel to issue `PassThruStopPeriodicMsg` against, skip the native stop entirely, and report
the COP `Finished`/`Cancelled`. A live, still-transmitting native periodic message would be
left with zero tracking anywhere: not on the link, not in any
`SharedChannel::leaked_periodic_message_ids`.

The fix restores the take to that early critical section, atomic with the session clear, and
keeps only the native-stop/leak-track DECISION inside the `self.api` bracket the function
already holds for its filter/repeat teardown, using its OWN pre-clear `channel_id`/
`channel_key` captures (the "captured, not re-derived" principle this mechanism's earlier
rounds established). That is exactly the shape the suspension-path callers already had, and it
is a concrete illustration of item 3's reasons (a)/(b)/(c) for why their early unfenced take is
safe: the take touches `logical_links` only, never `self.primitives`, so it destroys no
tracking a terminal status depends on; it is what makes a racing start's revalidation correctly
resolve `AlreadyResolved`; and what genuinely needs the fence is the decision on the result, not
the take. Those reasons were written here for the suspension path but not applied uniformly
during implementation — they hold for **every** terminator, and a terminator that clears session
identity of its own has a second, independent reason to take early: the take must be atomic with
that clear or it publishes an inconsistent CLL. Since Disconnect no longer uses step 3's shared
primitive, that helper's `owner: Option<u32>` parameter became dead and was simplified to a
required `cop_handle: u32`.

## Consequences

**Guarantee provided.** For the first three fenced paths (`CoptCancel`, TX-dispatch suspension
termination, CLL disconnect), no frame is ever transmitted after that path has reported its COP
terminal to the client. A termination racing an in-flight start blocks for at most the duration
of one native start bracket (`self.api` held), not indefinitely.

The fourth fenced path — the hard-error dead-channel sweep, added by the round-16 amendment below —
closes
only the in-flight-start/`None`-sentinel race: no frame from a start that had not yet committed
its reservation is ever transmitted after this sweep has reported the racing CLL's COPs terminal.
It does NOT carry the same guarantee for an already-committed periodic (a real `Some(message_id)`
reservation) on a hard-errored channel: this sweep never attempts any native stop call at all, by
design (the channel is already dead), so a periodic that had already committed before the hard
error is simply leak-tracked onto `SharedChannel::leaked_periodic_message_ids` and may well keep
transmitting device-side after `PduCopstCancelled` is reported — exactly as already accepted and
documented for that case (see this ADR's own Fix B/Fix 1 lineage in
`j2534-0404-service/docs/implementation-notes.md`, ADR-192 Decision item 2, which documents
`handle_channel_hard_error`'s dead-link drain as attempting no native stop and leak-tracking a
committed entry instead; see the same file's Prioritized Backlog for the related, still-open
best-effort-stop-failure residual on the finalize side). This amendment is scoped to the
in-flight-start race only; it does not extend the fourth path's guarantee to be equivalent in
scope to the other three.

The fifth fenced path — `ModuleDisconnect`'s force-cleanup, added by the round-17 amendment further
below — closes the same in-flight-start/`None`-sentinel race, on the same terms as the fourth path
above: no frame from a start that had not yet committed its reservation is ever transmitted after
`ModuleDisconnect` has reported the swept CLLs' COPs terminal. It also does NOT carry the fourth
path's already-committed-entry leak-tracking behavior at all — deliberately, and for a different
reason than the fourth path's "channel already dead" rationale: it never leak-tracks a committed
entry because (round 18 amendment below) it instead attempts an explicit best-effort native stop of
its own first. On the branch where a device was actually open, `ModuleDisconnect` calls
`PassThruStopPeriodicMsg` (round 18) for every still-tracked committed periodic on a live
(non-`dead`) channel, then `PassThruDisconnect` on every channel, then `PassThruClose` on the whole
device (SAE J2534-1 §7.2.4 and §7.2.2 respectively — also restated together in §7.2.7's own
periodic-message semantics description) as a backstop for whatever the explicit stop missed or
failed to clear. (Neither `PassThruDisconnect` nor `PassThruClose` runs at all when no device was
open — see the round-17 amendment below for the reachability argument for why that branch can
never have a committed periodic to lose either.) See the round-17 and round-18 amendments below for
the full rationale.

**Irreducible residual.** Once the native start call is issued, its burst has already gone out
by the time it returns — no design can retract it. This is not a defect: any frame the burst
sends always precedes the terminator's own completion/report, since the terminator cannot
proceed past the fence until the start bracket releases `self.api`. The only genuine gap is a
termination racing a start that is CURRENTLY mid-bracket — that termination now correctly
*waits* for the bracket to finish before it can report anything, rather than racing ahead.

**Amendment (Codex review round 16, P1, PR #101): the hard-error dead-channel sweep is now
fenced too, closing this ADR's one remaining accepted residual.** The original text of this
paragraph judged fencing `events::handle_channel_hard_error` out of scope, reasoning that a
`None`-sentinel take under `self.logical_links` alone was a rare double-fault on a
channel already confirmed dead/erroring. A round-16 Codex review flagged the same TOCTOU gap
this ADR closes everywhere else: the sweep's `cancel_link_cops` call could still report a swept
CLL's COPs terminal while a racing start's native `PassThruStartPeriodicMsg` was still in
flight, transmitting after the report. A design-advisor consult validated closing it with the
same take-then-fence-the-decision idiom item 3 established, adapted for a per-channel sweep that
takes MULTIPLE CLLs' entries in one pass rather than one terminator per COP:

- The sweep's per-CLL closure (under `self.logical_links` alone, as before) still takes
  `link.tp20_broadcast_periodic` unconditionally, but now matches explicitly on
  `periodic.message_id` instead of relying on a let-chain to silently discard the `None` arm: a
  committed `Some(message_id)` is leak-tracked onto the dead `SharedChannel` exactly as before; a
  `None`-sentinel now sets a per-sweep `in_flight_start_taken` flag instead of being dropped with
  no trace.
- After that `self.logical_links` critical section ends (so `links` is released) but while
  `self.shared_channels` (`chans`) is still held -- it is not dropped until after the sweep's
  per-CLL `cancel_link_cops` loop, per ADR-139 -- the sweep does ONE batched
  `self.api.lock().await` then immediately drops it, if and only if `in_flight_start_taken` was
  set. No native call is issued (mirrors `terminate_tp20_broadcast_periodic_for_suspension`'s own
  `None`-sentinel branch, which also issues none): the guard's sole purpose is the wait itself.
  Acquiring, then dropping, `self.api` here guarantees the opposite ordering, mirroring
  `terminate_tp20_broadcast_periodic_for_suspension`'s own doc comment (`rpc_misc.rs`): for each
  racing start, either it has not yet reached its own revalidation (and will now find its
  reservation gone under `revalidate_tp20_broadcast_periodic_reservation` and self-abort with no
  native call -- in this case the sweep does NOT wait for that start to finish; the acquisition
  finds `self.api` uncontended and returns immediately, and the start aborts independently, on
  its own schedule, whenever it next revalidates), or it already held `self.api` (in which case
  the sweep does wait, until that start completes its full bracket, including
  `finalize_or_orphan_broadcast_periodic_start_locked`'s own orphan-stop of the just-started
  message) -- before the sweep proceeds to its `cancel_link_cops` loop.
- One batched acquire-then-release correctly fences EVERY `None`-sentinel taken during that
  pass, not just one CLL, because every take happened-before this point, strictly before any
  `cancel_link_cops` call in the loop that follows. Two `handle_channel_hard_error` calls (for
  different dead channels) cannot interleave with each other at this point either, since each
  holds `chans` across its entire body (ADR-139).
- Lock order: `self.api` nests under the already-held `self.shared_channels`, sanctioned by
  ADR-080 (`shared_channels` is outermost; `api`/`logical_links` may nest underneath it) --
  `self.logical_links` is NOT held at this point (released when the per-CLL closure's block
  ended), so ADR-110's `api`-outer/`logical_links`-inner ordering is not in play for this
  acquisition at all.
- `was_primary`'s gate (a broadcast periodic is only ever started against a link's PRIMARY
  channel_id) applies to both arms of the take unchanged, and is vacuously true whenever
  `tp20_broadcast_periodic` is `Some` in the first place: TP2.0 links never gain a UUDT companion
  channel (UUDT companions are gated to ISO15765-protocol links only).

See `j2534-0404-service/src/service/events.rs`'s `handle_channel_hard_error` for the
implementation and `events_hard_error_broadcast_periodic_tests.rs` for direct-unit coverage,
including a fence-blocking test using the same technique as this file's own
`tp20_broadcast_periodic_api_fence_tests`. The former P3 backlog bullet tracking this residual in
`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog is deleted as closed.

**Amendment (Codex review round 17, P1, PR #101): `ModuleDisconnect` (`rpc_module_disconnect`,
`rpc_module.rs`) is the fifth terminator, and closes this ADR's own audit of every
`tp20_broadcast_periodic` call site with no unfenced terminator remaining.**

*Context.* Round 17's review found that `rpc_module_disconnect` — ISO 22900-2 §9.3.3's
force-cleanup path, run on every `PDUModuleDisconnect` — did nothing with
`tp20_broadcast_periodic` at all before this amendment. It set `link.connected = false` for every
CLL, called `cancel_link_cops` for every CLL (reporting every owned COP, including any broadcast
periodic, terminal), and later `links.clear()` wiped every `LogicalLinkState` — including
`tp20_broadcast_periodic` — with no fence and no leak-tracking of any kind. This is worse than the
pre-fix state of the four paths above: those at least leak-tracked (or, for the suspension path,
correctly deferred) a committed entry; this path dropped one silently.

*Decision.* The same take-then-fence-the-decision idiom the round-16 amendment above established
for the hard-error sweep, applied inside this function's EXISTING `chans`-held window (the block
already acquiring `self.shared_channels` across the `cancel_link_cops` loop, per ADR-139) — no
second lock window added:

- While iterating `links.values_mut()` in the same loop that sets `link.connected = false`, take
  `link.tp20_broadcast_periodic` and match on `periodic.message_id`: `Some(_)` (already committed)
  is a deliberate no-op — see the Consequences discussion above for why; `None` (an in-flight-start
  reservation) sets a per-call `in_flight_start_taken` flag, declared before the `chans`/`links`
  acquisitions, mirroring the round-16 flag's own declaration shape.
- Immediately after that block ends (`links` released, `chans` still held) and strictly before the
  `cancel_link_cops` loop, one batched `if in_flight_start_taken { drop(self.api.lock().await); }`
  — the identical acquire-then-release fence the round-16 amendment uses, for the identical
  ordering-guarantee reason: whichever side reaches `self.api` first wins; a racing start that
  has not yet revalidated finds its reservation gone and self-aborts; a start already inside its
  own bracket finishes it, including its own orphan-stop, before this function reports anything
  terminal via `cancel_link_cops`.
- Lock order: `self.api` nests under the already-held `self.shared_channels`, sanctioned by
  ADR-080. `self.logical_links` is NOT held at this point (released when the block above ended),
  so ADR-110's `api`-outer/`logical_links`-inner ordering is not in play for this acquisition.
  This function's own LATER `self.api.lock()` (for the native `PassThruDisconnect`/`PassThruClose`
  calls, further down the same function) is a separate, subsequent acquisition entirely unrelated
  to this fence.
- Unlike the four prior fixes, the `Some(_)` (committed) arm deliberately does NOT push onto
  `SharedChannel::leaked_periodic_message_ids`. This does NOT rest on `api.disconnect`/`api.close`
  running unconditionally: both are reached only on the branch where a device was actually open
  (`slot.take()` returning `Some`, further down the same function) and are skipped entirely
  otherwise. The reachability argument: `self.device_id` is cleared only by this function's own
  `slot.take()` and by `spawn_shutdown_task` (`service.rs`), and `connect_new_physical_channel`
  (`rpc_link.rs`) can only add a `shared_channels` entry while the caller holds that same
  `self.device_id` guard — so a `None` outcome here implies `shared_channels` was already empty,
  with nothing for a skipped `api.disconnect` to disconnect anyway. On the branch that DOES run
  them, `api.disconnect(sc.channel_id)` (per channel) and then `api.close(id)` (for the whole
  device) run a few lines later in the same function — SAE J2534-1 §7.2.4's `PassThruDisconnect`
  semantics and §7.2.2's `PassThruClose` semantics each independently document that tearing down a
  channel/device clears that channel's (respectively, the whole device's) resident periodic
  messages device-side (also restated together in §7.2.7). **Superseded by the round-18 amendment
  below**, which no longer rests solely on that argument: `api.disconnect`/`api.close` can each
  independently fail, and this round-17 text did not account for that. Round 18 adds an explicit
  best-effort native stop of its own, issued strictly before `api.disconnect`/`api.close`, and
  extends the identical treatment to `repeat_message_ids` (this paragraph's own closing sentence
  originally cited `repeat_message_ids`' lack of a per-message `api.stop_repeat_message` attempt as
  precedent for periodic's then-no-stop behavior — see the round-18 amendment for why that
  precedent is now itself superseded, symmetrically, rather than left stale). See
  `j2534-0404-service/src/service/rpc_module.rs`'s own doc comment at this same collection site, and
  its sibling `debug_assert!`s at the `slot.take()` site, for the current behavior pinned in code.

*Consequences.* This closes the last unfenced terminator this ADR's mechanism has — the audit
performed while implementing this amendment checked: the four already-fixed paths (`CoptCancel`,
TX-suspension termination, `DisconnectComLogicalLink`, `handle_channel_hard_error`); the one other
`links.clear()`-adjacent site in this same function (none — `rpc_module_disconnect` has exactly
one, now fixed here); the one `links.remove()` site in `rpc_destroy_com_logical_link` (needs no
fence of its own — see below); all four `cancel_link_cops` callers in the crate
(`rpc_module_disconnect`, `rpc_destroy_com_logical_link`/`DestroyComLogicalLink`,
`rpc_disconnect_com_logical_link`/`DisconnectComLogicalLink`, and `handle_channel_hard_error` —
`rpc_disconnect_com_logical_link` already fenced by this ADR's base mechanism (one of the "first
three fenced paths" above) and `handle_channel_hard_error` by the round-16 amendment above,
`rpc_destroy_com_logical_link` needing no fence of its own per the paragraph above, and
`rpc_module_disconnect` fenced by this round-17 amendment); every field-level
`tp20_broadcast_periodic.take()` site in the crate; and
confirmed `CoptStopcomm` (`handle_stop_comm`, `events.rs`) deliberately never touches the broadcast
COP at all (tester-present's software-only stop-comm has no periodic interaction to fence). No
sixth unfenced terminator remains.

`rpc_destroy_com_logical_link`'s own correctness was re-verified, not changed: it removes the CLL
from `logical_links` (and therefore takes `tp20_broadcast_periodic` along with it, implicitly, by
dropping the whole entry) before ever acquiring `self.api`, which rests on the invariant
"`channel_id_for_tp: None` here implies `tp20_broadcast_periodic` was already `None`, or was
already taken atomically alongside `channel_id` by `DisconnectComLogicalLink`" — see the comment
pinning this invariant at that function's own `channel_id_for_tp.is_some()` gate in `rpc_link.rs`.
If that atomicity ever decouples, that function needs its own fence.

See `j2534-0404-service/src/service/rpc_module.rs`'s `rpc_module_disconnect` for the
implementation and its own `mod tests` for direct-unit coverage, including a fence-blocking test
using the same technique as `events_hard_error_broadcast_periodic_tests.rs`'s round-16 test.

**Amendment (Codex review round 18, P1, PR #101): `ModuleDisconnect` now attempts an explicit
best-effort native stop for a committed periodic (and, symmetrically, for `repeat_message_ids`)
before its general disconnect/close teardown, rather than relying solely on that teardown
succeeding.**

*Context.* Round 17's fix above dropped a committed broadcast periodic unconditionally, reasoning
that `api.disconnect`/`api.close` (SAE J2534-1 §7.2.4/§7.2.2) would clear it device-side regardless.
A round-18 Codex review found that reasoning incomplete: neither native call's result is checked
before this function proceeds, so a genuine double failure of both left a committed periodic
transmitting device-side with no tracking anywhere and no way to ever retry stopping it. A
design-advisor consult confirmed leak-tracking the failure was a dead end here specifically (not a
general objection to leak-tracking): by the time any such failure could be observed, `slot.take()`
has already unconditionally cleared `self.device_id` (ADR-107 addendum (h)), and the opportunistic
leaked-stop retry mechanisms (`retry_leaked_periodic_message_stops`/
`retry_leaked_repeat_message_stops`, `rpc_misc.rs`) only ever probe a LIVE `SharedChannel` on an
OPEN device — so nothing could ever reach the old device again to retry against, regardless of what
got tracked.

*Decision.* While iterating `links.values_mut()` in the same loop the round-17 fix already added
(the one that takes `tp20_broadcast_periodic` and sets `link.connected = false`), each link now
also resolves its owning `SharedChannel` via `link.channel_key.and_then(|k| chans.get(&k))` (the
`chans`/`shared_channels` guard the round-17 fix already holds across this loop) and skips
collecting anything for that link if the resolved channel is already `dead` — mirroring
`handle_channel_hard_error`'s own precedent of never attempting a native call against an
already-dead channel. For a still-live channel, a committed `Some(message_id)` periodic and every
entry in `repeat_message_ids` are each collected into a `(channel_id, id)` pair, in two separate
`Vec`s. Later in the same function, inside the `if let Some((_, id)) = slot.take() { ... }` block —
under the SAME `self.api` guard that block already acquires for the disconnect/close sequence, and
strictly BEFORE the existing `for (_, sc) in channels { api.disconnect(sc.channel_id); }` loop —
this function now issues `api.stop_periodic_message(channel_id, message_id)` for every collected
periodic pair and `api.stop_repeat_message(channel_id, msg_id)` for every collected repeat pair. A
failure from either is `warn!`-logged and otherwise ignored: not propagated, not leak-tracked (per
the design-advisor reasoning above), matching how `DisconnectComLogicalLink`/
`DestroyComLogicalLink`/`handle_channel_hard_error` already treat a failed stop with nowhere further
to escalate to. The broader `api.disconnect`/`api.close` sequence still runs immediately afterward,
unconditionally, as a backstop for whatever the explicit stop missed, was skipped for (a `dead`
channel), or itself failed to clear.

The round-17 `debug_assert!` pinning "`slot == None` ⇒ `shared_channels` was already empty" is
extended with a second `debug_assert!` pinning its direct corollary: if `shared_channels` was
empty, no link could have resolved a live `SharedChannel` above either, so both collected `Vec`s
must be empty too.

*Consequences.* If the explicit stop AND the later `api.disconnect` AND `api.close` all fail for
the same message, it may keep transmitting device-side with no tracking anywhere — an ACCEPTED
RESIDUAL, not a new or worse gap specific to periodic/repeat messages: every other resource class on
an abandoned device (filters, other repeat slots, everything else) is equally unreachable the moment
`self.device_id` clears, per ADR-107 addendum (h)'s existing deliberate device-abandonment design.
Periodic/repeat messages now merely share that same already-accepted cost instead of being a silent,
undocumented exception to it (periodic) or an unremarked-upon precedent nobody had actually
justified in writing (repeat, per the round-17 bullet above). This amendment adds no new fenced
terminator and does not reopen the "no sixth unfenced terminator remains" audit above — it changes
only what the already-fenced decision inside `ModuleDisconnect`'s existing `self.api` bracket does,
not when or under what lock that bracket is acquired.

See `j2534-0404-service/src/service/rpc_module.rs`'s `rpc_module_disconnect` for the implementation
and its own `mod tests` for direct-unit coverage of: the explicit stop firing for a committed
periodic on a live channel, the overall RPC still succeeding when that stop fails, the stop being
skipped for an already-`dead` channel, and the symmetric `repeat_message_ids` failure-tolerance
behavior.

**`CoptCancel`'s fence is unconditional.** Every `CoptCancel` call now briefly contends for
`self.api`, even for a COP that owns no broadcast periodic at all — a cheaper "does this CLL
track one?" pre-check would itself have to read `logical_links` without the fence, reopening
exactly the unsynchronized read this ADR closes. `self.api` is already shared with the poll
task and every native-call site — including some with a multi-second worst case (e.g.
`ioctl_become_master`'s ADR-189 native call, `five_baud_init`) — so an unlucky `CancelComPrimitive`
can now occasionally queue behind one of those even for a COP that owns no broadcast periodic at
all, not just contend with it whenever it does have a periodic to stop. No cheap safe alternative
exists: a `logical_links`-only pre-check is exactly the unsynchronized read this ADR closes (a
concurrent start can sit between its `primitives` insert and its own reservation write, so such a
pre-check could miss a periodic that's about to exist). Accepted as the cost of closing the race.

**Supersedes ADR-192 Decision item 2's in-flight-reservation/terminator-race mechanism**, not
that Decision item's broadcast-periodic feature or its other terminators' overall shape (their
native-call/tracking/leak-tracking behavior for an already-committed entry is unchanged by this
ADR — only WHEN and under WHAT LOCK a `None`-sentinel entry may be taken and reported
terminal). ADR-192's own body is left describing the pre-fence mechanism as historical
context for why this fence exists; its Status line is annotated to point here.

**Follow-up:** none. The hard-error dead-channel sweep (round 16) and `ModuleDisconnect`'s
force-cleanup (round 17) were this mechanism's last two unfenced terminators; the round-17
amendment's own audit (see above) confirms no sixth remains. The round-18 amendment above is a
refinement of `ModuleDisconnect`'s already-fenced decision (an explicit best-effort native stop
before its general teardown, plus the symmetric `repeat_message_ids` treatment), not a new
terminator or a new fence, and introduces no follow-up of its own beyond the accepted residual its
own Consequences paragraph documents.

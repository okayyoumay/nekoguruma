# ADR-110: `PDU_ERR_EVT_RSC_LOCKED` Error Event Replaces Synchronous Physical-ComParam-Lock Rejection

**Date:** 2026-07-22 (corrected 2026-07-22 after a confirmed regression found
by `edge-case-hunter` in this ADR's first cut, before this branch was
merged — see the Decision and Consequences sections below for what changed
and why; the branch was never merged with the flawed design, so this ADR is
amended in place rather than superseded; amended again 2026-07-22 for two
Codex review findings on PR #116 -- `CP_Parity`'s BUSTYPE-list exclusion and
a `LockResource`/`handle_update_param` TOCTOU race -- see the "BUSTYPE list
is keyed by hardware effect, not ISO label" and "Lock-grant/apply
serialization" subsections below)
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`),
             `j2534-0404-service/src/service/rpc_link.rs` (`rpc_set_com_param`,
             `rpc_lock_resource`),
             `j2534-0404-service/src/service/comparam_support.rs`
             (`apply_bustype_lock`, `strip_bustype_keys`, `BUSTYPE_UNUM32`),
             `j2534-0404-service/src/service/events.rs` (`handle_update_param`,
             `handle_send_recv`, `handle_start_comm`,
             `revert_hardware_to_live_active`, `apply_params_to_hardware`,
             `apply_params_to_hardware_locked`)

## Context

ISO 22900-2:2009 §9.4.16 d) describes what happens when a `PDU_COPT_UPDATEPARAM`
would modify a locked `PDU_PC_BUSTYPE`-class physical ComParam:

The clause (paraphrased) says: when another ComLogicalLink holds a lock on
the resource behind the addressed ComLogicalLink for a `PDU_PC_BUSTYPE` class
ComParam, the D-PDU API must still create the ComPrimitive, apply all the
ComParams that are not locked, and raise a `PDU_ERR_EVT_RSC_LOCKED` error
event. The ComPrimitive must then complete in the ordinary way, ending in
`PDU_COPST_FINISHED`.

This is **not** a rejection model: the COP is always created and always
finishes; the lock conflict surfaces only as a single asynchronous error
event, and only for the specific ComParam(s) actually in conflict — everything
else the caller staged still gets applied and promoted.

Prior to this ADR, this codebase implemented the opposite model in three
places, none of which the spec supports:

1. **`CoptUpdateparam`** (`rpc_primitive.rs`, ADR-044): `rpc_start_com_primitive`
   synchronously rejected with `Status::resource_exhausted` /
   `PduErrRscLockedByOtherCll` if another CLL held
   `LOCK_PHYSICAL_COM_PARAMS` — no COP was ever created. This is the exact
   case §9.4.16 d) describes, and the spec explicitly says the error surfaces
   "after a `PDU_COPT_UPDATEPARAM`," not instead of one.
2. **`temp_param_update` on `CoptSendrecv`/`CoptStartcomm`** (`rpc_primitive.rs`,
   ADR-044/ADR-066): the same synchronous rejection applied to a
   `temp_param_update=1` call sharing the lock-checked resource. ISO
   22900-2:2009 §9.4.16.2.1 c) NOTE 2 (spec line 2129) and d) (line 2133) make
   clear that a `PDU_PC_BUSTYPE`-class ComParam can never be changed via
   `TempParamUpdate` at all — lock or no lock: the temp bracket applies and
   later reverts against this CLL's own bound/live Active set, which has no
   cross-CLL synchronization and can already be stale relative to whatever
   another CLL has actually pushed to the shared physical hardware,
   independent of whether any lock happens to be held at the time. (An
   earlier draft of this ADR reasoned that ADR-067 §E's own
   `PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard made a separate lock check redundant
   here, on the theory that a call reaching this point has "no staged BUSTYPE
   change left to protect against" — `edge-case-hunter` found and confirmed,
   with a repro, that this reasoning does not hold: ADR-067 §E only checks
   that this CLL's own Working equals its own Active, which says nothing
   about whether that CLL's own Active still matches the real, currently
   shared hardware state. See the Decision section's temp-path fix below.)
3. **`SetComParam`** (`rpc_link.rs`): `rpc_set_com_param`'s `Unum32` arm
   synchronously rejected a physical (J2534-config-mapped) ComParam write with
   the same error if another CLL held the lock. `SetComParam` only ever
   writes the in-memory Working set — Table 25 (the RPC's own return-code
   table) has no lock-related return code for it at all, because `SetComParam`
   performs no hardware mutation for a lock to protect. The lock's effect only
   exists once a `PDU_COPT_UPDATEPARAM` attempts to promote Working to Active
   and push it to hardware.

## Decision

### Three synchronous rejections removed

All three call sites above are deleted outright (no replacement synchronous
check):

- `rpc_primitive.rs`'s `writes_physical_com_params` block (the
  `LOCK_PHYSICAL_COM_PARAMS` check gating `CoptUpdateparam` and
  `temp_param_update` `CoptSendrecv`/`CoptStartcomm`).
- `rpc_link.rs`'s `is_physical`/`find_physical_lock_holder` check inside
  `rpc_set_com_param`'s `Unum32` arm.

`CoptUpdateparam` now always creates the COP and enqueues it — nothing is
checked synchronously for this lock. `SetComParam` never had a spec-legal
lock rejection to begin with; its `Unum32` arm is now shaped identically to
the existing Bytefield/Structfield arms (no lock check at all).

### BUSTYPE-only classification, not the broader CONFIG-ID set

The new exclusion logic (`comparam_support::apply_bustype_lock`) governs
**only** the narrow `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` const lists already
defined for ADR-067's `bustype_params_differ` guard — not the broader
`to_j2534_config_id`-mapped set of "physical" ComParams the old
`rpc_link.rs` check used. The spec names `PDU_PC_BUSTYPE` specifically in
§9.4.16 d); ADR-067 §E already established that this same narrow list is the
correct BUSTYPE classification for this codebase (drawn from
`comparam-protocol-support.md`'s "Physical Layer ComParams (BUSTYPE class)"
table). Reusing it here means `LOCK_PHYSICAL_COM_PARAMS` protects exactly the
same class of ComParam in both places it now matters (ADR-067's synchronous
`temp_param_update` guard, and this ADR's execution-time promotion guard) —
one shared classification definition, not two independently-maintained ones.

### Three independently-computed roles, not one shared `effective` set (corrected)

This ADR's first cut computed a single `effective` `ComParamSet` — used for
both the hardware push and the Active promotion — whose BUSTYPE-class
exclusion was gated on whether this CLL's own bound Working (`params`)
differed from this CLL's own live Active (`active`). `edge-case-hunter`
found and confirmed, with a repro, that this is unsound: **a CLL's own
Working-vs-Active agreement is an "did I intend to change this" signal, not
a "is this value safe to push to hardware" signal.**
`LogicalLinkState::active` is per-CLL bookkeeping with no cross-CLL
synchronization, so it can go stale relative to the real, shared hardware
value with absolutely no local signal of that staleness — a non-owning CLL
whose own Working happened to equal its own (already-stale) Active would
skip the exclusion entirely under the old design and silently push its own
stale value to hardware, clobbering whatever the lock holder had actually
set. This is strictly worse than the pre-ADR-110 blanket synchronous
rejection it replaced, which at least caught this case (crudely, by
rejecting the whole call).

The corrected design (`comparam_support::apply_bustype_lock`) splits the old
single `effective` set into three independently-computed pieces, returned as
a `BustypeLockResolution { hw_set, promote_set, rsc_locked }`:

1. **`hw_set`** — the `ComParamSet` passed to `apply_params_to_hardware`.
   Whenever `locked_by_other`, every `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` key is
   removed from `hw_set` **unconditionally** — regardless of whether it
   happens to differ from this CLL's own `active`. (An absent key in the set
   passed to `apply_params_to_hardware` already means "don't touch hardware
   for it at all," per that function's own per-key `filter_map` and its
   `configs.is_empty()` early return — no change needed there.) This
   unconditional exclusion is the actual safety fix: no BUSTYPE key from a
   non-owning CLL ever reaches hardware while locked, full stop — own
   Working-vs-Active agreement can never prove otherwise.
2. **`promote_set`** — the `ComParamSet` assigned to `LogicalLinkState::active`
   (and used for the tester-present re-arm's gating/resolution/`CP_P2Max`
   reads). Whenever `locked_by_other`, each excluded BUSTYPE key is
   substituted with this CLL's own **pre-call** `active` entry (inserted if
   present, removed if absent) instead of `params`'s staged value — this is
   exactly the original design's substitution logic, now scoped to promotion
   bookkeeping only, never to the hardware push. It keeps this CLL's own
   `GetComParam`-visible-after-`CoptRestoreParam` bookkeeping internally
   consistent even though the hardware push for that key never happened.
3. **`rsc_locked`** — `locked_by_other && bustype_params_differ(params, active)`,
   reusing the existing, unchanged `bustype_params_differ` helper. This is
   the CLL's own "did I attempt to modify anything" signal (matching §9.4.16
   d)'s "attempts to modify" wording), independent of what got excluded from
   `hw_set`: a CLL whose own Working equals its own Active on every BUSTYPE
   key attempted nothing, so no event fires even though its
   (self-consistent, but possibly hardware-stale) values are still excluded
   from `hw_set`.

When `locked_by_other` is `false`, `hw_set = promote_set = params` unchanged
and `rsc_locked` is `false` — identical to pre-ADR-110 unlocked behavior.

### Temp-path: structural strip, not lock-conflict logic

The corrected design does **not** add a lock check (or a `PDU_ERR_EVT_RSC_LOCKED`
event) to `temp_param_update`. Per ISO 22900-2:2009 §9.4.16.2.1 c) NOTE 2 /
d): a `PDU_PC_BUSTYPE`-class ComParam can never be changed via
`TempParamUpdate` at all — lock or no lock — because the stale-Active clobber
risk exists even with **no** lock held anywhere (this CLL's own stale Active
gets pushed regardless of any lock's presence). A lock check on this path
would therefore be both insufficient (it does nothing for the unlocked case,
which is equally vulnerable) and unnecessary (ADR-067 §E's unchanged
`PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard already keeps `effective`'s own BUSTYPE
portion equal to this CLL's own Active, from that CLL's own point of view).

Instead, `comparam_support::strip_bustype_keys(params: &ComParamSet) ->
ComParamSet` removes every `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` key from a clone
(leaving every other key, including `structfield`, unchanged), and is applied
at every point a `ParamBinding::Temp` bracket's ComParams reach hardware:

- `handle_send_recv`'s `ParamBinding::Temp { effective, .. }` apply.
- `handle_start_comm`'s `ParamBinding::Temp { effective }` apply.
- `revert_hardware_to_live_active` — the single function backing all 7
  Temp-bracket revert call sites across `handle_send_recv`/`handle_start_comm`
  (confirmed Temp-specific at each call site before centralizing the strip
  here rather than at each of the 7): strips the live `active` snapshot it
  reads before pushing it back to hardware, so the revert can never re-write
  a stale BUSTYPE value either.

Since ADR-067 §E's guard already guarantees `effective`'s BUSTYPE portion
equals this CLL's own Active for any call that reaches execution, this strip
is a pure no-op from that CLL's own point of view — but a real fix against
the cross-CLL stale-Active clobber, since it structurally prevents a BUSTYPE
key from ever being part of what a temp bracket pushes to hardware, at apply
or at revert time, regardless of lock state.

### Live-at-execution-time check, not call-time

The lock conflict is (re-)checked inside `handle_update_param` (`events.rs`),
in the same critical section that already reads the live `hw_protocol_id`
for the ADR-086 staleness check (U1) — not at `StartComPrimitive` call time.
This matches both the spec's own wording ("after a `PDU_COPT_UPDATEPARAM`" —
i.e. at execution, not at the call that enqueues it) and this function's
existing ADR-086 discipline of snapshotting everything the execution path
needs inside one lock acquisition, rechecked at the same points U1/U2/the
post-promotion recheck already exist for. The live Active snapshot
(`l.active.clone()`) and the `find_physical_lock_holder` result
(`LOCK_PHYSICAL_COM_PARAMS`) are captured alongside `hw_protocol_id` in that
one critical section, threaded through the match on `live_ctx` exactly like
the existing `j2534_protocol_id` value was.

### Exactly-one-error-event placement

`send_error_event(..., PduErrorEvent::PduErrEvtRscLocked)` is called at most
once per `handle_update_param` invocation, immediately after
`apply_params_to_hardware`'s attempt for `hw_set` returns — i.e. after
`PduCopstExecuting` has already been sent and after the hardware I/O for
this COP has been attempted, but before the `all_ok` promotion/re-arm
branch runs. This matches the order in which the spec describes the steps (one
`PDU_ERR_EVT_RSC_LOCKED` error event, then the remaining ComParams applied,
then `PDU_COPST_FINISHED` from the CLL; the spec stresses that it is a
single event) — a subscriber never observes
the lock-conflict error while the non-conflicting ComParams are still
unapplied — and is independent of whether the `apply_params_to_hardware`
call itself succeeds or fails (that failure path already has its own,
unrelated `PduErrEvtProtErr` event via the pre-existing `!all_ok` arm): the
lock event fires either way, since it reports on the excluded key, not on
the outcome of applying everything else. (Corrected after a Codex review
round on PR #116 found the initial implementation emitted this event
*before* the hardware-apply attempt, contradicting this same ordering
rationale.)

### BUSTYPE list is keyed by hardware effect, not ISO label (Codex review, PR #116)

A second Codex review round on PR #116 found that `BUSTYPE_UNUM32`
(`comparam_support.rs`) deliberately excluded `CP_Parity`
(`ComParamId(j2534_0404::PARITY)`), reasoning inherited from ADR-067 §E: its
ISO `PDU_PC_BUSTYPE` *label* is ambiguous, since D-PDU folds parity into
`CP_UartConfig` and `CP_Parity` has no distinct D-PDU `CP_*` name of its own.
That reasoning was sound for `bustype_params_differ`'s original purpose (is
this ISO-labeled ComParam BUSTYPE class), but this ADR's own `hw_set`
exclusion (`apply_bustype_lock`) and `strip_bustype_keys` reuse the *same*
list for a materially different purpose: "does this ComParam reach a native
`PassThruIoctl SET_CONFIG` write that another CLL's lock or a temp bracket
must never touch." By that measure, `expand_uart_config` (`comparam_id.rs`)
shows `CP_Parity`'s physical effect is completely unambiguous — an explicit
`PARITY` entry always reaches hardware, unconditionally, and wins precedence
over any `CP_UartConfig`-derived parity value. Excluding it from
`BUSTYPE_UNUM32` therefore left a real hole: a non-owning CLL (or a
`temp_param_update` call) could smuggle a physical UART-parity change past
both `apply_bustype_lock`'s exclusion and `strip_bustype_keys`, bypassing
exactly the protection this ADR exists to provide.

**Decision:** `CP_Parity` is added to `BUSTYPE_UNUM32`. This is one shared
list, re-classified as being keyed by physical hardware effect rather than
by ISO `PDU_PC_BUSTYPE` label membership — not a second, lock-specific list
alongside the original. Reusing one list for both `bustype_params_differ`
(ADR-067 §E's call-time guard) and `apply_bustype_lock`/`strip_bustype_keys`
(this ADR's execution-time guards) means every consumer sees the same
classification; maintaining two independently-curated lists for "is this
BUSTYPE by ISO label" vs. "does this need hardware-write protection" would
be strictly worse and prone to drifting apart again.

**Intended side effect, not a regression:** because `bustype_params_differ`
is unchanged code reading a now-wider list, ADR-067 §E's own
`PDU_ERR_TEMPPARAM_NOT_ALLOWED` synchronous guard now also rejects a
`temp_param_update` call that stages a `CP_Parity` difference from Active.
This is spec-correct per ISO 22900-2 §9.4.16.2.1 c) NOTE 2: a physical
ComParam can never change via `TempParamUpdate`, regardless of which alias
names it. See the amendment note added to ADR-067 §E and
`comparam-protocol-support.md`'s BUSTYPE-class table/note for the same
correction.

### Lock-grant/apply serialization (Codex review, PR #116)

A third Codex review round found a TOCTOU race between `rpc_lock_resource`
(`rpc_link.rs`) granting `LOCK_PHYSICAL_COM_PARAMS`/`LOCK_PHYSICAL_TX_QUEUE`
and `handle_update_param`'s own lock-conflict resolution and hardware push:
`rpc_lock_resource` only ever acquired `logical_links`, never `api`; the
live-at-execution-time check this ADR introduced (above) reads
`locked_by_other` under `logical_links`, releases it, and only later
acquires `api` (the global native-API mutex, contended by every hardware
operation in the service) to actually perform the `SET_CONFIG` push. Between
that read and the `api` acquisition, a concurrent `LockResource` call could
complete and return success to its caller — after which the still-in-flight
`handle_update_param` would resume, holding a now-stale `locked_by_other =
false`, and push its unfiltered `hw_set` to hardware, clobbering exactly
what the just-granted lock was supposed to protect, with zero exclusion and
zero `PDU_ERR_EVT_RSC_LOCKED` event.

**Decision:** serialize the grant and the read-then-push on the `api` mutex
itself.

- **`rpc_lock_resource`** now acquires `self.api.lock().await` FIRST, before
  `self.logical_links.lock().await`, and holds both simultaneously through
  the entire conflict-check-and-grant sequence (unchanged otherwise), only
  releasing them together at the end of the function. `rpc_unlock_resource`
  is unchanged and needs no such serialization: releasing a lock while an
  apply is in flight can only make an already-computed exclusion
  conservative (the apply still excludes based on the read it already took),
  never unsafe.
- **`handle_update_param`** now acquires `ctx.api.lock().await` FIRST, before
  the live-at-execution-time check (`logical_links`, nested and released
  before continuing), and holds `api` continuously through
  `apply_bustype_lock`'s (pure, synchronous) resolution and the hardware
  push itself, releasing it only after the push returns — the `None`/stale
  arms that don't need hardware I/O drop `api` immediately since they have
  nothing to serialize against. `apply_params_to_hardware` is split into a
  thin `Arc<Mutex<J2534Api0404>>`-locking wrapper (unchanged behavior, kept
  for its other call sites: `handle_send_recv`/`handle_start_comm`'s temp
  brackets and `revert_hardware_to_live_active`) and
  `apply_params_to_hardware_locked`, which takes an already-locked
  `&J2534Api0404` directly — `handle_update_param` calls the latter so the
  read-then-push sequence runs inside one continuous critical section.
  `PDU_ERR_EVT_RSC_LOCKED`'s emission moves to after `api` is released (it
  still fires exactly once, still after the hardware-apply attempt, per the
  reorder fix above) because `send_error_event` internally re-locks
  `logical_links`, which must never happen while `api` is held (see the new
  ordering invariant below).

**Why this closes the window:** with `api` acquired first on both sides,
only two orderings are possible for any given `LockResource` call racing a
`CoptUpdateparam`'s execution: either the grant completes (and is visible to
its caller) strictly before `handle_update_param` reaches its own `api`
acquisition — in which case its fresh `find_physical_lock_holder` read
inside that critical section observes the grant and excludes correctly — or
the grant cannot complete until *after* `handle_update_param` releases
`api`, i.e. after the push has already happened, in which case the grant
only ever protects from that point forward, never retroactively. There is no
window where a grant is visible to callers but `api` hasn't yet serialized
against an in-flight `handle_update_param`.

**New lock-ordering invariant:** `api` must be acquired before
`logical_links` whenever both are held together; `logical_links` must never
be held across an `api.lock().await` acquisition. Before this amendment, no
ordering existed between these two mutexes crate-wide (only
`shared_channels`, ADR-080, had a documented outermost position) — the two
were simply never held simultaneously anywhere. A grep sweep of
`j2534-0404-service/src/service/*.rs` for every `logical_links.lock().await`
binding, checked against every `api.lock().await` site in the same function,
confirmed no existing call site holds `logical_links` across an
`api.lock().await` acquisition (every existing site either never holds both
at once, or scopes the `logical_links` guard to a block that closes before
`api` is acquired) — so this amendment's two new call sites are the first to
hold both simultaneously, and both follow the new `api`-then-`logical_links`
order.

**Dependency on the BUSTYPE-list fix above:** temp brackets
(`handle_send_recv`/`handle_start_comm`'s `ParamBinding::Temp` applies and
`revert_hardware_to_live_active`) need no changes for this race — they push
BUSTYPE-free sets by construction via `strip_bustype_keys`, so no lock grant
can be violated by them — but this claim is only true because
`strip_bustype_keys` reads the corrected, `CP_Parity`-inclusive
`BUSTYPE_UNUM32`. Had the BUSTYPE-list fix not landed first, a temp
bracket's `CP_Parity` value could still reach hardware unfiltered,
independent of this race's own fix. The two Codex-review findings this
amendment addresses are therefore not independent; both are required
together.

**Consequences (added to the list below):** `LockResource`'s latency now
includes waiting out any `api` hold in flight from a concurrent
`CoptUpdateparam`'s hardware push — rare (a single `SET_CONFIG` IOCTL is
fast) and correctness-mandated, not a performance concern in practice.
`LOCK_PHYSICAL_TX_QUEUE` has an analogous call-time-check-vs-in-flight-send
window (a `CoptSendrecv`/`CoptStartcomm` transmit reads the TX-queue lock at
dispatch time, not atomically with the actual write) that is explicitly OUT
of scope for this fix — recorded as a backlog item in
`j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
instead of folded into this PR.

**Regression test feasibility:** a fully deterministic test pinning this
race (e.g. holding a test-controlled gate on the mock's `api` mutex while
observing `LockResource`'s call ordering) was attempted and found infeasible
with the existing `grpc_mock` harness — see that test module's own doc
comment for the detailed reasoning (this crate's `#[tokio::test]` default
`current_thread` flavor plus this service's synchronous-FFI-under-a-tokio-
Mutex design means the only existing "hold" hook
(`arm_write_rx_injection`'s `hold_ms`) blocks the single OS thread via
`std::thread::sleep` rather than cooperatively yielding, so it cannot create
a genuine two-task interleaving window without a new test-only production
gate hook, which is out of scope for this fix). The fix is instead verified
by: the full existing `j2534-0404-service` test suite (including
`locks_and_param_classes.rs`/`startcomm_comparam.rs`'s existing ADR-110
regression tests) passing unchanged after this reordering, confirming no
deadlock and no behavior change in every call-time ordering the harness CAN
construct; and the lock-ordering-invariant grep sweep described above.

## Alternatives Considered

1. **Reuse the broader `to_j2534_config_id`-derived "is this physical at
   all" set for exclusion** (the shape the old `rpc_link.rs` check used) —
   Rejected: over-blocks. That set includes every ComParam this service maps
   to a J2534 CONFIG ID, which includes per-CLL logical params (e.g.
   ISO15765-only addressing/timing params) the spec's `PDU_PC_BUSTYPE` lock
   was never meant to govern. Using it here would exclude (and silently
   substitute) far more of a locked CLL's promotion than the spec's own
   `PDU_PC_BUSTYPE` wording calls for.
2. **Omit conflicting keys from `promote_set` instead of substituting
   Active's value** — Rejected: an omitted key in a partial-update model
   means "don't touch this key in the target," which is correct for a
   sparse-patch API but wrong here, because the `link.active = promote_set.
   clone()` write operates on the *whole* `ComParamSet`, not a sparse patch.
   Simply leaving the conflicting key out of `promote_set` (rather than
   setting it to this CLL's own pre-call Active value) would silently
   corrupt the promoted Active set — a `GetComParam`-after-`CoptRestoreParam`
   read would report whatever stale/absent value happened to already be in
   the cloned struct, not this CLL's own real, still-locked-and-unchanged
   bookkeeping. Substituting this CLL's own pre-call Active value keeps
   `promote_set` internally consistent: exactly the keys that could not
   legally change stay at their pre-conflict value; everything else
   promotes as staged.
3. **Keep some form of synchronous rejection** (e.g. only for
   `CoptUpdateparam`, dropping just the `temp_param_update`/`SetComParam`
   cases) — Rejected: the spec is unambiguous that §9.4.16 d)'s COP must be
   created and must reach `PDU_COPST_FINISHED`; a synchronous rejection can
   never satisfy that regardless of which call sites it's narrowed to. The
   lock state that matters is the state *at execution time*, not at
   `StartComPrimitive` call time, which a synchronous call-time check cannot
   observe correctly regardless of narrowing.
4. **Gate `hw_set`'s BUSTYPE exclusion on whether `params` differs from
   `active`** (this ADR's own first-cut design) — Rejected after
   `edge-case-hunter` confirmed, with a repro, that it is unsound: a CLL's
   own Working-vs-Active agreement proves only that this CLL did not intend
   to change the value, never that the value is safe to push to a physical
   resource another CLL is actively protecting. `LogicalLinkState::active`
   has no cross-CLL synchronization, so it can be stale relative to the real
   hardware state with no local signal of that staleness — gating the
   exclusion on this comparison lets a non-owning CLL clobber the lock
   holder's real hardware value whenever its own bookkeeping happens to
   already agree with its own (stale) prior state. The corrected design
   makes `hw_set`'s exclusion unconditional whenever `locked_by_other`, and
   uses the Working-vs-Active comparison only for the (separate, weaker)
   `rsc_locked` event signal, where being merely a signal rather than a
   safety gate is the correct role for it.
5. **Restore a `LOCK_PHYSICAL_COM_PARAMS` check for `temp_param_update`
   calls, instead of (or in addition to) the structural strip** — Rejected:
   this would incorrectly re-reject spec-legal `temp_param_update` calls
   whose only "conflict" is that another CLL merely *holds* the lock, even
   when that other CLL's own Active is fully in sync with the real hardware
   state and the temp call's own BUSTYPE portion (already guaranteed equal
   to this CLL's own Active by ADR-067 §E) poses no actual risk in that
   case. The real risk this ADR fixes is the stale-Active clobber, which
   exists independent of whether any lock is held at all (see NOTE 2 / d) in
   the Context section) — a lock check neither necessary nor sufficient for
   that risk is the wrong tool regardless of how it's scoped.

## Consequences

- `StartComPrimitive(COPT_UPDATEPARAM)` always returns a COP handle now and
  the COP always finishes (`PDU_COPST_FINISHED`), regardless of any other
  CLL's `LOCK_PHYSICAL_COM_PARAMS`. A lock conflict on a BUSTYPE-class key
  never reaches hardware (`hw_set` excludes it unconditionally), and the
  event that surfaces the conflict (`PDU_ERR_EVT_RSC_LOCKED`) fires only when
  this CLL itself attempted a change (its own Working differed from its own
  Active) — an identical-to-its-own-Active staged value is still excluded
  from `hw_set` (it is never provably safe to push), but fires no event,
  since this CLL attempted nothing of its own.
- `temp_param_update` `CoptSendrecv`/`CoptStartcomm` calls are no longer
  gated by `LOCK_PHYSICAL_COM_PARAMS` at all, and never were meant to be:
  `strip_bustype_keys` structurally keeps every BUSTYPE-class key out of both
  the temp apply and the revert, regardless of lock state; ADR-067 §E's own
  `PDU_ERR_TEMPPARAM_NOT_ALLOWED` BUSTYPE-differ guard remains the sole
  call-time physical-ComParam gate for these calls (see the amendment note
  added to ADR-067 §C).
- `SetComParam` never rejects on this lock; it always writes the requested
  value into Working, exactly like every other `ParamData` arm
  (Bytefield/Structfield) already did.
- **Accepted residual — a non-owning CLL's own Active can remain stale on
  BUSTYPE keys indefinitely, with no event ever surfacing that fact to it.**
  Since `rsc_locked`/`PDU_ERR_EVT_RSC_LOCKED` only fires when this CLL's own
  Working differs from its own Active, a CLL whose own bookkeeping already
  (coincidentally) matches its own prior state gets no signal at all that
  its `LogicalLinkState::active` no longer reflects the real, shared
  hardware value — it was already stale before this COP ran, and this ADR's
  fix (correctly) only prevents that staleness from being pushed to
  hardware, not from existing in the first place. This is accepted, not
  fixed: `GetComParam`/`LogicalLinkState::active` bookkeeping is
  per-CLL-by-design in this codebase (see A2-17 in the conformance audit —
  `GetComParam` never reports a cross-CLL-synchronized class/value), and
  surfacing every other CLL's hardware-affecting change to every CLL sharing
  a channel is a materially larger, separate design question this ADR does
  not take on.
- **Accepted residual — non-BUSTYPE physical params on a shared channel are
  no longer masked by an over-broad lock check.** `LOCK_PHYSICAL_COM_PARAMS`'s
  spec scope is `PDU_PC_BUSTYPE` only; a non-owning CLL can now stage and
  promote *non-BUSTYPE* physical ComParams (e.g. per-CLL addressing/timing
  params this service maps to a J2534 CONFIG ID but that are not in
  `BUSTYPE_UNUM32`/`BUSTYPE_BYTES`) on a shared channel while another CLL
  holds this lock — this was previously incidentally blocked by the removed
  `rpc_link.rs` check's broader `to_j2534_config_id`-based classification.
  The pre-existing question of whether/how non-BUSTYPE physical-param
  sharing across CLLs on one channel should be governed is a genuinely
  separate, out-of-scope design question this ADR does not resolve — flagged
  here as a latent gap for a future audit item, not fixed in this change.
- **Accepted residual — mid-`.await` lock-state changes during
  `apply_params_to_hardware` are a single-read-point residual**, the same
  shape as `handle_update_param`'s other documented ADR-086 residuals (U1/U2/
  the post-promotion recheck): the lock-conflict check itself is read once,
  in the same critical section as the `hw_protocol_id`/`active` snapshot,
  before `apply_params_to_hardware`'s hardware `.await`. A concurrent
  `LockResource`/`UnlockResource` call that changes the lock's holder during
  that `.await` is not re-observed before the promotion completes — matching
  this function's existing "snapshot everything needed in one lock
  acquisition" idiom rather than introducing a new, differently-shaped
  recheck for this one field.
- **Explicitly not addressed by this ADR**: A2-1 (async error events,
  including this ADR's own `PDU_ERR_EVT_RSC_LOCKED` event, never carry
  `cop_handle` attribution) is a separate, pre-existing gap in
  `send_error_event`/`TrackedError`/`error.rs` this ADR does not touch.
- **Lock-grant/apply serialization amendment (PR #116)**: `LockResource`'s
  latency now includes waiting out any `api` hold already in flight from a
  concurrent `CoptUpdateparam`'s hardware push — an accepted,
  correctness-mandated residual, not a performance concern in practice (a
  single `SET_CONFIG` IOCTL is fast). `LOCK_PHYSICAL_TX_QUEUE` has an
  analogous call-time-check-vs-in-flight-send window that is explicitly OUT
  of scope for this fix — recorded as a backlog item in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
  instead. `CP_Parity` is added to `BUSTYPE_UNUM32` (superseding ADR-067
  §E's "conservatively excluded" classification) — see the "BUSTYPE list is
  keyed by hardware effect, not ISO label" subsection above.
- Test files updated: `j2534-0404-service/tests/grpc_mock/locks_and_param_classes.rs`
  (the `CoptUpdateparam`-blocked-by-lock test now asserts a COP handle, one
  `PDU_ERR_EVT_RSC_LOCKED` event, and `PDU_COPST_FINISHED`, with the
  conflicting BUSTYPE key excluded from the hardware push and its promotion
  pinned to cll_b's own pre-conflict Active value, and any other staged
  non-BUSTYPE param promoted normally; the `temp_param_update` lock test now
  asserts success) and `j2534-0404-service/tests/grpc_mock/startcomm_comparam.rs`
  (the `CoptStartcomm` `temp_param_update` lock test likewise now asserts
  success). Both files' module-level doc comments describing ADR-044's
  synchronous-rejection behavior are updated to describe this ADR's
  event-based behavior instead.
- **Regression tests pin both edge-case-hunter-confirmed bugs from this
  ADR's first cut**: `locks_and_param_classes.rs`'s
  `start_com_primitive_updateparam_stale_own_active_does_not_clobber_real_
  hardware_and_fires_no_event` (main path: a non-owning CLL's own
  Working-equals-own-stale-Active must still exclude the BUSTYPE key from
  the hardware push, firing no event, without clobbering the real,
  currently-locked hardware value) and `startcomm_comparam.rs`'s
  `start_com_primitive_startcomm_temp_param_update_does_not_clobber_hardware_
  with_stale_own_active` (temp path: the same stale-Active clobber, with
  *no* lock held anywhere, via a `temp_param_update=1` `CoptStartcomm`'s
  apply/revert bracket). Both were confirmed to fail against this ADR's
  original (buggy) implementation before being pinned against the corrected
  one.

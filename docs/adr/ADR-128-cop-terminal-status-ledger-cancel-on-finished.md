# ADR-128: COP Terminal-Status Ledger — CancelComPrimitive Succeeds on an Already-Finished COP

**Date:** 2026-07-24
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`J2534Service::terminal_cops`,
             `TerminalCopsLedger`),
             `j2534-0404-service/src/service/events.rs` (`send_cop_status`,
             `cancel_link_cops`, `cancel_held_tx_items`,
             `should_skip_cancelled_item`, `handle_channel_hard_error`,
             `handle_update_param`, `handle_start_comm`, `handle_stop_comm`),
             `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_cancel_com_primitive`, `rpc_get_status`),
             `j2534-0404-service/src/service/rpc_link.rs`
             (`rpc_create_com_logical_link`, `rpc_destroy_com_logical_link`,
             `rpc_disconnect_com_logical_link`),
             `j2534-0404-service/src/service/rpc_module.rs`
             (`rpc_module_disconnect`),
             `j2534-0404-service/src/service/rpc_misc.rs`

## Context

Conformance-audit finding A2-23
(`j2534-0404-service/docs/iso22900-2-conformance-audit.md`) found that
`CancelComPrimitive` returned `PDU_ERR_INVALID_HANDLE` for a COP that had
already reached a terminal status. ISO 22900-2:2009(E) is explicit on two
points this service did not match:

- **§9.4.18.2 d):** a cancel request against a ComPrimitive that is already
  in `PDU_COPST_FINISHED` succeeds. §9.2.6.6 gives the same do-nothing
  outcome for a COP already in `PDU_COPST_CANCELLED`.
- **§9.2.6.7 (Destruction of ComPrimitives):** the D-PDU API destroys a
  ComPrimitive internally only once its final status item has been read
  from the event queue. From that point on, no further operation may be
  performed on that ComPrimitive, and attempts are answered with
  `PDU_ERR_INVALID_HANDLE`.

Prior to this change, every one of `events.rs`'s ~44 `send_cop_status(...,
PduCopstFinished/PduCopstCancelled)` call sites removed the COP from
`primitives` (this service's live-COP map) in the same step it emitted the
terminal status — i.e. destruction happened at *emission* time, not at
*read* time. `rpc_cancel_com_primitive`'s only source of truth for "does this
handle exist" was `primitives`, so a `CancelComPrimitive` call landing any
time after that emission — including the window before the client has even
seen the terminal status — saw a plain map miss and returned
`PDU_ERR_INVALID_HANDLE`, contradicting §9.4.18.2 d).

**Why "destroy on read" cannot be implemented literally in this
architecture.** This service delivers COP status two ways: a push model
(`SubscribeEvent`, `send_cop_status` writing directly to a live subscriber's
channel) and a pull model (`GetStatus` polling). Neither gives the service a
reliable "the client has now read the terminal status" signal:
`SubscribeEvent`'s channel send succeeding only proves the notification was
queued into the client's local gRPC stream buffer, not that application code
consumed it, and `GetStatus` polling is stateless — every call just
re-reports current state, with no acknowledgment concept at all. Multiple
concurrent readers (a live subscriber and a polling caller) can coexist, so
there is no single "read" event to hang destruction off. Building a real
read-acknowledgment protocol across both delivery paths for this purpose
alone was rejected (see Alternatives).

## Decision

**Redefine when a COP's bookkeeping actually disappears: not at
terminal-emission time, but at CLL-destroy time.** A COP having left
`primitives` is no longer equivalent to a fully "destroyed" COP for
`CancelComPrimitive`/`GetStatus` purposes; it merely means "no longer live
and running." Full destruction (participation in a genuine
`PDU_ERR_INVALID_HANDLE`) is deferred until the owning CLL itself is
destroyed.

**Mechanism — a service-wide terminal-status ledger, populated centrally.**
`J2534Service` gains a new field:

```rust
terminal_cops: Arc<Mutex<HashMap<u32, (u32 /* cll_handle */, PduComPrimitiveStatus)>>>,
```

`events::send_cop_status` — the single function every terminal-status
emission in this codebase already funnels through (confirmed: the only
`EventItemData::CopStatus(...)` construction site in the crate) — now takes
`terminal_cops` as a parameter and, before touching `subscriptions`, records
`terminal_cops.insert(cop_handle, (cll_handle, status))` whenever `status` is
`PduCopstFinished` or `PduCopstCancelled`. Because the recording lives inside
`send_cop_status` itself rather than being duplicated at each of its ~44 call
sites, coverage is **compiler-enforced**: any future terminal-emission call
site is covered automatically, and a call site that forgot to thread
`terminal_cops` through simply fails to compile (wrong argument count).

Consumers:

- **`rpc_cancel_com_primitive`:** on a `primitives` miss, checks
  `terminal_cops` before erroring. A hit returns success immediately
  (`Ok(Self::empty_response())`) with **no further action** — no
  re-emission, no `cancelled_cops`/`logical_links` mutation — matching
  §9.2.6.6's "no further action" wording for an already-terminal COP exactly.
  A miss on *both* maps is a genuinely unknown or already-destroyed handle
  and still returns `PDU_ERR_INVALID_HANDLE`, unchanged from before.
- **`rpc_get_status`:** the pre-existing unconditional-`PduCopstFinished`
  fallback for a `primitives` miss now consults `terminal_cops` first and
  reports the real recorded terminal status (Finished *or* Cancelled) when
  present, falling back to the old unconditional `PduCopstFinished` only when
  `terminal_cops` also has no entry. This incidentally fixes a second,
  related bug the audit's own investigation surfaced: a COP cancelled via the
  deferred (`cancelled_cops`) mark-and-defer path used to visibly flip from
  Cancelled to Finished in `GetStatus` the instant the poll task's deferred
  emission ran and removed the entry from `primitives` — `terminal_cops` now
  keeps reporting Cancelled through that transition too, since §9.2.6.7
  treats FINISHED and CANCELLED identically for destruction purposes.
- **`rpc_destroy_com_logical_link`:** after `cancel_link_cops` runs, purges
  every `terminal_cops` entry belonging to the destroyed CLL
  (`retain(|_, (cll, _)| *cll != handle)`) — the actual destruction point
  this ADR defines.

**Eviction policy: destroy-only, no timer, no read-tracking.** An entry
lives from its COP's terminal emission until its owning CLL is destroyed,
however long that is — not purged on disconnect (a disconnected-but-not-yet-destroyed
CLL's terminal COPs must remain queryable/cancellable-as-no-op), not
time-bounded. `next_primitive_handle` (`service.rs`) allocates COP handles
from a wrapping `u32` counter that only skips values currently live in
`primitives`; it does not also check `terminal_cops`, so a handle can in
principle wrap around and be reissued while a stale `terminal_cops` entry for
it still exists. This is harmless: both consumers check `primitives` first
and only fall back to `terminal_cops` on a miss, so a reissued handle's live
`primitives` entry always wins; the stale `terminal_cops` entry is simply
overwritten the next time *that* COP itself reaches a terminal status. See
Consequences for the residual this leaves.

**Lock discipline — strict leaf: never acquire another lock while holding
`terminal_cops`, though other locks are routinely held while acquiring
it.** `send_cop_status` acquires and fully releases `terminal_cops` before
acquiring `subscriptions`; the two are never held simultaneously. This
introduces two nesting edges elsewhere in the crate, both one-way *into*
`terminal_cops`, never out of it: `primitives -> terminal_cops` at sites
that already hold `primitives` across their `send_cop_status` call (the
pre-existing ADR-118 `primitives -> subscriptions` edge extends
transparently to `primitives -> terminal_cops -> subscriptions`, still a
straight line, not a cycle), and `shared_channels -> terminal_cops` in
`rpc_destroy_com_logical_link`, which holds `shared_channels` continuously
across both `cancel_link_cops` (acquires `terminal_cops` inside
`send_cop_status`) and this function's own explicit `terminal_cops` purge.
Neither is a deadlock risk — `terminal_cops` is never held while acquiring
anything else, so it cannot participate in a cycle — but it does mean the
field's own doc comment must not claim `terminal_cops` is *never* held
alongside another lock; only the "never acquire another lock while holding
it" direction is the actual invariant. `rpc_cancel_com_primitive` and
`rpc_get_status` acquire `primitives` and `terminal_cops` as two
independent, non-overlapping temporary locks (the `primitives` guard is a
temporary, dropped at the end of its lookup expression, before
`terminal_cops` is ever touched).

**Mechanism (round 2 correction) — a terminal emission's `primitives`
removal and its `terminal_cops` record are ONE atomic critical section, not
two separately-timed operations.** The original mechanism above described
`send_cop_status` recording into `terminal_cops` as a side effect, called
from sites of the shape `if primitives.lock().await.remove(&cop_handle)
.is_some() { send_cop_status(...).await; }`. On this crate's actual runtime
— `#[tokio::main]`'s default MULTI-THREADED flavor, genuinely concurrent OS
threads rather than single-thread cooperative interleaving — Rust drops
that `if` condition's temporary `MutexGuard` at the end of the condition
expression, BEFORE the `if` body runs, so `primitives` is released well
before `send_cop_status` ever acquires `terminal_cops`. Any two separately-timed
lock acquisitions are racy on a multi-threaded runtime regardless of
whether an explicit `.await` sits between them (a mistake repeated three
times while writing the original version of this ADR — see the "Finding 1"
bullet below): a concurrent thread can interleave at any point, not only at
`.await` boundaries. A `CancelComPrimitive`/`GetStatus` call landing in
that gap saw a miss on BOTH `primitives` and `terminal_cops` and concluded
the handle never existed — reproducing the exact A2-23 bug this ledger
exists to fix, not a merely-theoretical residual (the original text below
called it "sub-microsecond" and accepted it; that framing is withdrawn).

Fixed by making every terminal emission hold ONE `primitives` `MutexGuard`
continuously from the removal through the entire `send_cop_status` call
(which itself acquires `terminal_cops` then `subscriptions`) — most call
sites via a new shared helper, `events::emit_terminal_if_live` (see its own
doc comment for the full reasoning and deadlock argument). Since
`rpc_cancel_com_primitive`/`rpc_get_status` both acquire `primitives`
FIRST, before ever consulting `terminal_cops`, a reader attempting
`primitives.lock().await` while a writer still holds that SAME mutex
across its whole critical section blocks until the writer's `terminal_cops`
record has already landed — the reader can only ever observe "still live in
`primitives`" or "gone from `primitives`, but `terminal_cops` definitely has
the record," never the in-between "gone from both" state. Deadlock-free:
this extends the `primitives -> terminal_cops -> subscriptions` edge from
momentary to call-spanning, and no site in this crate acquires `primitives`
while already holding `subscriptions` or `terminal_cops` (verified
crate-wide, not merely locally). Two sites could not use the shared helper
as-is and have their own hand-written equivalent instead: `cancel_link_cops`
(batch removal across many COPs for one CLL — holds `primitives` across the
whole remove-then-emit loop, not just one cop, and drops it before
acquiring `logical_links` afterward, preserving the documented
`logical_links -> primitives` order) and `dispatch_tx_item`'s WAITING/
CANCELLED tail (pre-existing, already correct — holds `logical_links` and
`primitives` together for an unrelated invariant, ADR-118 round 6).

## Alternatives Considered

1. **Unconditional success for any unknown `cop_handle`.** Rejected: this
   would also swallow a truly bogus, never-allocated, or long-since-destroyed
   handle, which §9.2.6.7 explicitly requires `PDU_ERR_INVALID_HANDLE` for —
   trading one conformance defect for its mirror image.
2. **A per-emission-site `finished_cops`-style set, mirroring the existing
   `cancelled_cops` mark-and-defer pattern.** Rejected: `cancelled_cops`
   works because cancellation is *client-initiated* (the RPC that inserts the
   mark is the same call whose caller needs the answer). There is no
   equivalent trigger for "the client is about to ask about a COP that
   finished on its own" — the insertion would have to happen at every one of
   the ~12-15 `PduCopstFinished`-emitting removal sites (a scattered,
   convention-enforced set, not compiler-checked), and would still leave the
   CANCELLED case — which needs the identical fallback — uncovered by a
   FINISHED-only set. A centralized ledger inside `send_cop_status` covers
   both statuses and every current and future emission site by construction.
3. **A per-CLL field on `LogicalLinkState` instead of a service-wide
   `J2534Service` field.** Rejected: `send_cop_status` does not have a
   `LogicalLinkState` in scope at most call sites (only `cll_handle`, a raw
   `u32`), and adding one would require acquiring `logical_links` at every
   terminal emission — a real lock-ordering expansion this crate's
   extensively-documented `logical_links`-hierarchy (see that field's own doc
   comment in `service.rs`) does not need for a leaf-only piece of
   bookkeeping. A service-wide field keyed by `cop_handle` (already
   service-wide-unique) needs no `LogicalLinkState` access at insertion time
   at all.
4. **True read-acknowledgment tracking across both delivery paths.**
   Rejected as infeasible without a protocol change: `SubscribeEvent`'s
   stream-send success is not a "the client processed this" signal, and
   `GetStatus` polling is inherently stateless. Implementing this would mean
   adding an explicit ack RPC or reworking `GetStatus` into a consuming read,
   neither of which any real client of this service expects or needs — no
   client-visible benefit over the destroy-on-CLL-destroy approximation.
5. **Time-bound eviction of `terminal_cops` entries.** Rejected: introduces a
   nondeterministic point after which `CancelComPrimitive` starts returning
   `PDU_ERR_INVALID_HANDLE` again for a COP the client may still be
   legitimately about to query, which the spec's read-triggered model does
   not describe at all. Destroy-only is simpler and strictly more permissive
   (never a *false* invalid-handle) than any timer could be.

## Consequences

- `CancelComPrimitive` on a COP that already reached `PDU_COPST_FINISHED` or
  `PDU_COPST_CANCELLED`, whose CLL is still alive, now returns success
  (no-op) instead of `PDU_ERR_INVALID_HANDLE` — the A2-23 fix, and a
  client-visible behavior change any existing test/integration asserting the
  old error code on this specific path needs to update.
- `GetStatus(cop_handle)` now correctly keeps reporting `PduCopstCancelled`
  (instead of prematurely flipping to `PduCopstFinished`) once the deferred
  mark-and-defer cancellation path's poll-task emission has run and removed
  the entry from `primitives` — a related, previously-unflagged bug fixed as
  a byproduct of the same mechanism.
- **`GetStatus` on a truly never-existed `cop_handle` still unconditionally
  reports `PduCopstFinished`** rather than a more accurate "no such handle"
  signal. Pre-existing, out of scope for this ADR (this fallback's behavior
  is unchanged for the case where `terminal_cops` also has no entry) —
  flagged as a separate, standing gap.
- **Withdrawn — the "sub-microsecond remove-vs-record window" was
  originally accepted here as a residual; Codex review round 2 correctly
  identified it as a real, unaccepted bug (the exact A2-23 failure this ADR
  exists to fix), not a bounded curiosity.** See the round-2 "Mechanism"
  paragraph in Decision above for the fix (`emit_terminal_if_live`, holding
  `primitives` across the whole remove-then-record-then-emit sequence) and
  the corrected reasoning: on this crate's actual multi-threaded runtime,
  any two separately-timed lock acquisitions are racy regardless of
  `.await` placement, so "no intervening `.await`" is not evidence of
  safety — the same mistaken reasoning that under-scoped the emission-guard
  fix below on its first pass.
- **Correction, round 1 (incomplete) then round 2 (complete) —
  terminal-status emission was unconditional at 8 sites, not the 3 first
  found, letting a straggling emission after a concurrent CLL destroy
  resurrect a stale `terminal_cops` entry.** Round 1 found and fixed
  `handle_update_param`, `handle_start_comm`, and `handle_stop_comm`
  (`events.rs`): each had a final `send_cop_status(..., Finished)` call with
  no `primitives.remove(&cop_handle).is_some()` guard. Round 1's own
  verification pass then WRONGLY cleared three more sites —
  `handle_send_recv`'s `remaining == 0` branch, `handle_delay`'s success
  tail, `handle_restore_param`'s success tail — reasoning that each
  "computes its staleness check via `is_some_and`/an explicit `None =>
  false` match arm... and has zero intervening `.await` between that check
  and its Finished emission, so there is no window." That reasoning is
  false on a multi-threaded runtime (see the withdrawn-residual bullet
  above): a concurrent OS thread can interleave between any two separately
  timed lock acquisitions, `.await` or not. Codex review round 2 caught one
  of these three (`handle_send_recv`); design-advisor's full-surface sweep,
  commissioned once the same mistake recurred, found the other two plus
  three MORE previously entirely unaudited sites — `handle_send_recv`'s
  `!write_ok` TX-failure exit, its `ReRequestTxFailed` receive-phase exit,
  and `handle_start_comm`'s `ProtErr`/`InitError`/`TxFailure` tails (three
  separate sites within that one handler, not one) — for a corrected total
  of 8. `handle_update_param` additionally never removed its own
  `primitives` entry on the ordinary success path at all (an independent,
  pre-existing bug this ADR's diff also happens to fix, invisible before
  this ADR since nothing durable used to depend on `primitives` membership
  surviving past the event emission — a generic post-dispatch cleanup in
  `dispatch_tx_item` removed it afterward regardless).

  Fixed uniformly across all 8 (plus the reader-vs-writer gap above) by the
  shared `emit_terminal_if_live` helper: on the normal path the removal
  always wins (nothing else races an executing, not-yet-terminal COP), so
  behavior is unchanged; on the destroy-race path, `cancel_link_cops`
  already won, so the straggler loses and correctly no-ops.
  `handle_delay`/`handle_restore_param`'s two-branch structure (a separate
  Finished-if-live / Cancelled-if-stale pair) was additionally collapsed
  into `handle_update_param`'s existing "compute the status, then one
  guarded emission" shape, so there is exactly one `primitives` removal per
  call deciding both the first-wins race and which status is correct — two
  independent, separately-timed removals for the same COP was itself a
  latent instance of the same race class. This finding-and-fix pattern
  (second consecutive round hitting the same class → stop patching
  per-instance, commission a full-surface audit) is this codebase's own
  documented playbook (`.claude/skills/codex-pr-review-loop/reference/finding-routing.md`),
  applied here after round 2 recurred.
- **Accepted residual — `terminal_cops` handle-reuse shadowing, bounded by
  `u32` wraparound.** As described in Decision, a wrapped-around, reissued
  COP handle can transiently coexist with a stale `terminal_cops` entry from
  its previous life; harmless because `primitives` is always checked first
  by both consumers. Requires roughly 2^32 COPs started on one CLL without
  that CLL ever being destroyed to become observable at all.
- **Accepted residual — unbounded `terminal_cops` growth across a long-lived
  CLL's COP churn, AND unbounded `TerminalCopsLedger::destroyed_clls`
  growth across the module's whole lifetime.** `entries` are purged only at
  CLL-destroy time, so a CLL that runs for a very long time and starts many
  COPs accumulates one 12-16-byte entry per COP until it is destroyed;
  `destroyed_clls` (added by the Codex-review-round-1 amendment below) never
  shrinks at all — every CLL this service EVER destroys leaves a permanent
  4-byte marker. Not fixed here; a backlog note is recorded in
  `j2534-0404-service/docs/implementation-notes.md` (a bounded per-CLL cap for
  `entries`, and/or an epoch/generation scheme replacing the permanent
  `destroyed_clls` set, if either ever proves to matter in practice).
- Two new lock-ordering edges, both one-way *into* `terminal_cops` (never
  out of it, so neither is a deadlock risk): `primitives -> terminal_cops`
  (already covered by the pre-existing `primitives -> subscriptions` edge's
  reasoning) and `shared_channels -> terminal_cops`
  (`rpc_destroy_com_logical_link` holds `shared_channels` across both
  `cancel_link_cops` and this map's own purge) — documented on
  `J2534Service::terminal_cops`'s own doc comment alongside the existing
  `logical_links` hierarchy notes.
- **Test coverage for the three-handler emission-guard correction:** no
  dedicated regression test exercises `DestroyComLogicalLink` racing an
  in-flight `handle_update_param`/`handle_start_comm`/`handle_stop_comm`
  mid-`.await` — the full existing test suite (310 unit + 439 `grpc_mock`
  integration tests) was confirmed to still pass unchanged with the fix in
  place, matching this crate's established, repeatedly-documented precedent
  (ADR-086 rounds 11/13, among others) that this class of in-handler race
  has no natural preemption point to land on deterministically under the
  `grpc_mock` suite's single-threaded (`current_thread`) test runtime; see
  `j2534-0404-service/docs/implementation-notes.md`. Verified by code
  inspection and the "first-wins through `primitives`" argument only, the
  same standard this crate's own prior in-handler guards are held to.

## Amendment (Codex review round 1 on the ADR-128 PR): record-vs-purge must be atomic

Codex's first review pass on this ADR's PR found two real, independent gaps
in the destruction model above — both now closed by restructuring
`terminal_cops`'s backing store into `TerminalCopsLedger`
(`j2534-0404-service/src/service.rs`), which pairs the entries map with a
`destroyed_clls: HashSet<u32>` marker set so that recording a terminal
status and purging/marking a CLL destroyed are each a single atomic
operation under `terminal_cops`'s ONE lock acquisition, rather than two
independently-timed operations on a bare `HashMap`.

**Finding 1 — resurrection race between a straggling `record` and a
concurrent purge.** The original design's "first-wins through `primitives`"
guard (added to `handle_update_param`/`handle_start_comm`/
`handle_stop_comm` earlier in this same PR, see the correction above) closes
the case where a straggler's OWN `primitives.remove()` loses the race — it
correctly skips its emission entirely. But it does not close the OPPOSITE
ordering: a straggler's `primitives.remove()` can still WIN (nothing else
has touched this cop_handle in `primitives` yet), after which — before that
same straggler's subsequent `send_cop_status` call actually reaches its
`terminal_cops.insert` — a concurrent `DestroyComLogicalLink` can run to
completion: `cancel_link_cops`'s own `primitives` scan finds nothing (the
straggler already removed it), and the destroy's `terminal_cops` purge
(the original bare `.retain(...)`) therefore finds nothing to remove either.
The straggler's delayed insert then lands AFTER the purge, resurrecting a
permanently-unpurgeable entry for an already-fully-destroyed CLL — worse
than the already-documented "sub-microsecond remove-vs-record window"
residual, which is self-correcting for a *reader*; this is a *permanent*
leak, because the destroyed `cll_handle` will never be the target of another
`DestroyComLogicalLink` call. Reachable in production (`main.rs` runs the
default multi-threaded `#[tokio::main]` runtime, not the `grpc_mock` test
suite's single-threaded one), unlike this crate's usual "test-harness-
infeasible" class of in-handler race.

Closed by `TerminalCopsLedger::record`/`purge`/`purge_many`: `purge` (used
by `rpc_destroy_com_logical_link`) and `purge_many` (used by
`rpc_module_disconnect`, see Finding 2) both remove matching entries AND
insert into `destroyed_clls`, atomically. `record` (used by
`send_cop_status`) checks `destroyed_clls` before inserting, under the SAME
lock acquisition. Whichever operation's lock acquisition happens first now
determines the outcome deterministically: if the purge runs first, the
later straggling `record` sees the destroyed marker and no-ops; if `record`
runs first, the entry is inserted normally and a later purge removes it as
before. No interleaving can resurrect an entry.

**Handle-reuse correction.** Since `destroyed_clls` markers are permanent
(never cleared automatically), `rpc_create_com_logical_link` now calls
`TerminalCopsLedger::unmark_destroyed` for its freshly-allocated
`cll_handle` right after inserting the new `LogicalLinkState` — without
this, a `cll_handle` number reissued by `next_logical_link_handle`'s
wrapping allocator (the same astronomically-rare, `u32`-wraparound-scale
event already accepted as a residual for the `entries` side of this
struct) would have every terminal status for its brand-new CLL silently
dropped by `record`'s guard forever, mistaking "this number belonged to a
destroyed CLL in a past life" for "the CLL currently holding this number is
destroyed" — a correctness bug on that path, not merely a residual, since
unlike the `entries`-reuse case, `primitives` gives a live COP no way to
"win" against an incorrectly-set `destroyed_clls` marker.

**Finding 2 — `ModuleDisconnect`'s force-cleanup never purged
`terminal_cops` at all.** `rpc_module_disconnect` (`rpc_module.rs`) calls
`cancel_link_cops` per CLL (correctly recording each one's terminal COPs),
then tears down every `LogicalLinkState` at once via
`self.logical_links.lock().await.clear()` — but the original code never
purged `terminal_cops` for any of those CLLs, unlike
`rpc_destroy_com_logical_link`'s single-CLL path. Every terminal COP that
existed at `ModuleDisconnect` time stayed permanently resolvable
(`CancelComPrimitive` succeeding as a no-op, `GetStatus` reporting its
stale recorded status) even after the module — and every one of its CLLs —
was completely gone, including across a later reconnect/new session. Closed
by calling `TerminalCopsLedger::purge_many` with the full list of
`cll_handles` torn down, right after `self.logical_links.lock().await
.clear()`, mirroring `rpc_destroy_com_logical_link`'s single-CLL `purge`
call exactly.

**Verification.** Four new unit tests directly exercise
`TerminalCopsLedger`'s atomicity contract (`purge` blocking a later
`record` for the same `cll_handle`; the ordinary record-before-purge case
still working; `purge_many`'s batch form; `unmark_destroyed` re-enabling a
reused handle) — deterministic, since they test the struct's logic
directly rather than trying to force the actual async interleaving. One new
`grpc_mock` integration test,
`module_disconnect_purges_terminal_cops_for_every_torn_down_cll`
(`tests/grpc_mock/modules.rs`), deterministically confirms Finding 2's fix
end-to-end: a COP that reaches `Finished`, then a `ModuleDisconnect`, then a
`CancelComPrimitive` on that same handle correctly fails with
`PDU_ERR_INVALID_HANDLE` (it would have wrongly succeeded before this
amendment). Finding 1's actual race remains structural-argument-only, for
the same reason the three-handler emission-guard fix above is (no
preemption point in the single-threaded test runtime to force the
interleaving on) — the full test suite (314 unit + 440 `grpc_mock`
integration tests after this amendment) was confirmed to still pass
unchanged.

## Amendment (Codex review round 2 on the ADR-128 PR): hold `primitives` across the whole terminal emission

Full detail is woven into the Decision and Consequences sections above
(rewritten in place per this repo's convention for an unmerged PR's own
ADR — see `.claude/skills/codex-pr-review-loop/reference/finding-routing.md`)
rather than appended separately; this section is a pointer for anyone
scanning the amendment history.

Codex round 2 found two real gaps, both consequences of the same
underlying mistake: "no intervening `.await` between a staleness check and
a terminal emission" was treated as evidence of safety in round 1's own
verification pass, which does not hold on this service's actual
multi-threaded `#[tokio::main]` runtime.

1. A third emission site (`handle_send_recv`'s `remaining == 0` branch) had
   no `primitives.remove(&cop).is_some()` guard, the identical shape round
   1 fixed at three other sites. A design-advisor-commissioned full-surface
   sweep (triggered by the same finding class recurring a second
   consecutive round) found five more: two more in `handle_send_recv`, three
   in `handle_start_comm`. All 8 are now fixed uniformly via a new shared
   helper, `events::emit_terminal_if_live`.
2. Even a correctly-guarded site had a residual gap: the guard's
   `MutexGuard` was a temporary, dropped before `send_cop_status` ran,
   leaving a window where a concurrent `CancelComPrimitive`/`GetStatus`
   could see a miss on both `primitives` and `terminal_cops` at once —
   reproducing A2-23 itself. Originally accepted in this ADR as a
   "sub-microsecond residual"; that framing is withdrawn. Fixed by holding
   the SAME `primitives` guard continuously across the removal and the
   entire `send_cop_status` call, at every site (`emit_terminal_if_live`
   for ~40 sites; `cancel_link_cops` and `dispatch_tx_item`'s WAITING gate
   have their own hand-written equivalents for batch/composite reasons).

Verified deadlock-free (no site acquires `primitives` while holding
`subscriptions` or `terminal_cops`, crate-wide) by design-advisor, who
traced every relevant lock-acquisition site directly rather than trusting
the pre-existing doc comments' claims. Full test suite (314 unit + 440
`grpc_mock` integration tests) confirmed unchanged after the fix; both
findings' actual races remain structural-argument-only, per this crate's
established precedent for this class of in-handler/cross-task race
(ADR-086 rounds 11/13 and others) — no deterministic reproduction is
feasible in the `grpc_mock` suite's harness.

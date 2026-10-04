# ADR-123: LockResource/UnlockResource §9.4.13-9.4.14 Conformance — Lock-Driven TX-Queue Suspension, Unlock Error Codes, Strict Mask Validation

**Date:** 2026-07-23
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`LogicalLinkState`, `find_physical_lock_holder`,
             `same_physical_resource`, `recompute_lock_tx_suspensions`),
             `j2534-0404-service/src/service/rpc_link.rs` (`rpc_lock_resource`,
             `rpc_unlock_resource`, `rpc_connect_com_logical_link`, `finalize_connected_link`,
             `rpc_disconnect_com_logical_link`, `rpc_destroy_com_logical_link`,
             `autodetect_sae_j1850_flavor`),
             `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`),
             `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_suspend_tx_queue`,
             `ioctl_resume_tx_queue`, `ioctl_clear_tx_queue`),
             `j2534-0404-service/src/service/events.rs` (`cancel_held_tx_items`,
             `dispatch_tx_item`, `drain_tx_held_backlog`, `handle_channel_hard_error`,
             `poll_rx_inner`)

## Context

Conformance-audit finding A2-15 (`j2534-0404-service/docs/iso22900-2-conformance-audit.md`)
identified that `LockResource`/`UnlockResource` diverged from ISO 22900-2:2009(E) §9.4.13/
§9.4.14 in four ways, none previously covered by a decision record:

1. **`UnlockResource` never returned `PDU_ERR_RSC_NOT_LOCKED`/`PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`.**
   Table 22 (§9.4.14.5, spec lines 1877-1889) defines both codes; the implementation
   unconditionally cleared bits from `held_lock_mask`, silently no-opping when nothing
   changed.
2. **`LockResource` never checked whether a transmission was in progress on the resource**
   (§9.4.13.2 b), spec line 1804) before granting a lock.
3. **`LockResource`'s TX-queue-lock use case rejected other CLLs' new COPs outright**
   instead of the spec's `SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` semantics. §9.4.13.3 use case 1
   (spec line 1810) is explicit: taking the transmit-queue lock
   forces every other ComLogicalLink sharing the physical resource into `SUSPEND_TX_QUEUE`,
   and a ComLogicalLink created afterwards is treated as starting out with its ComPrimitive
   queue already suspended. Releasing the lock sends `RESUME_TX_QUEUE` to all ComLogicalLinks
   on that resource. The prior implementation instead synchronously rejected
   `CoptSendrecv`/`CoptStartcomm`/non-empty-data `CoptStopcomm` with
   `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` (`rpc_primitive.rs` — this specific check predates any
   ADR of its own; ADR-044 covers a separate, `LOCK_PHYSICAL_COM_PARAMS`-only check added
   alongside it) — a client-visibly-different, stricter behavior than the spec describes.
4. **`LockResource` silently masked off undefined lock-mask bits** instead of validating
   them. Table D.2 (spec lines 6365-6370) defines only bit 0 (`LOCK_PHYSICAL_COM_PARAMS`) and
   bit 1 (`LOCK_PHYSICAL_TX_QUEUE`) — no other bits are defined.

Items 1 and 4 are spec-underspecified: the spec text does not describe what happens when an
`UnlockResource` mask spans bits in different states (held-by-self / held-by-other /
not-held), nor what a `LockResource`/`UnlockResource` mask containing undefined bits should
do. Item 3 is a concurrency-model and protocol-interpretation choice that touches two
existing lock-ordering invariants (ADR-080's `shared_channels`-outermost hierarchy, ADR-110's
`api`-before-`logical_links` amendment) and an existing suspend/resume subsystem
(`LogicalLinkState::tx_suspended`/`tx_held`, originally built only for the client-driven
`PDU_IOCTL_SUSPEND_TX_QUEUE`/`PDU_IOCTL_RESUME_TX_QUEUE` IOCTLs). Per CLAUDE.md's ADR rules,
all three qualify: non-obvious concurrency/state-machine choices and spec interpretation
calls that would surprise a future reader.

## Decision

### 1. `UnlockResource`: atomic, conflict-first validation

`rpc_unlock_resource` now computes, per call, `held_by_self = held_lock_mask & lock_mask` and
`leftover = lock_mask & !held_by_self`. If `leftover == 0`, the whole mask is held by this CLL
and the call proceeds (clear the bits, run the resume sweep below). If `leftover != 0`, the
call is rejected as a whole — **no partial unlock** — with:

- `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` (`Code::ResourceExhausted`) if
  `find_physical_lock_holder` finds another CLL holding any bit in `leftover`, or
- `PDU_ERR_RSC_NOT_LOCKED` (`Code::FailedPrecondition`) otherwise (no one holds that bit).

**Mixed-mask interpretation (spec-silent, decided here):** all-or-nothing rejection, mirroring
`rpc_lock_resource`'s pre-existing atomic treatment of a multi-bit mask (it already rejects
the whole grant if *any* requested bit conflicts). `RSC_LOCKED_BY_OTHER_CLL` takes priority
over `RSC_NOT_LOCKED` when checking the combined `leftover` mask, but in practice the two
outcomes are mutually exclusive per bit: a bit is held by at most one CLL at a time.

### 2. `LockResource`: active-transmission check, TX-queue bit only

When `lock_mask & LOCK_PHYSICAL_TX_QUEUE != 0`, `rpc_lock_resource` now also checks, for
every `SharedChannel` whose physical resource matches this CLL's own (`same_physical_resource`,
shared with `find_physical_lock_holder` below — this covers the pre-connect case, where the
requesting CLL has no `channel_key` yet and can only be compared by `hw_protocol_id`), that
channel's `executing_cop`. If it is `Some(cop)` and that COP's `CopEntry::transmits` is `true`
(ADR-123 Codex-review Fix D — a merely-executing, non-transmitting COP such as `CoptDelay` or
`CoptUpdateparam` does not block the grant), the grant is rejected with `PDU_ERR_FCT_FAILED`
(`Code::FailedPrecondition`) — **regardless of which CLL owns `cop`, including the requesting CLL
itself (ADR-123 Codex-review round 5 Finding J — see the correction note in Consequences below);
the check is resource-scoped, not CLL-scoped.** Table 21 (§9.4.13.6) offers only
`PDU_ERR_RSC_LOCKED` or `PDU_ERR_FCT_FAILED` for this function; `RSC_LOCKED`'s description
(the resource is already locked) is factually wrong for a busy-but-unlocked resource, so
`FCT_FAILED` is used.

This check is **not** applied when only `LOCK_PHYSICAL_COM_PARAMS` is requested: §9.4.13.3 use
case 2 (spec line 1814) states that taking a ComParam lock does not cut off transmissions that are already running.
A queued-but-not-yet-dispatched `TxItem` does **not** count as an active transmission — only a
live, actually-transmitting `executing_cop` blocks the grant; queued items are exactly what
TX-queue-lock suspension (below) exists to hold.

**Fix C (Codex-review round 2 on this ADR's PR): runs inside the grant's own critical
section, not a separate peek.** The check originally ran as a short-lived `logical_links` peek,
resolved and dropped *before* `api` was acquired, using only the requesting CLL's own
`channel_key` to find the physical channel — which is unconditionally absent pre-connect, so a
pre-connect `LockResource(LOCK_PHYSICAL_TX_QUEUE)` skipped the busy check entirely (Finding C).
The check now runs after both `api` and `logical_links` are held, alongside the existing
`find_physical_lock_holder` conflict check, scanning every channel in `shared_channels` and
filtering by `same_physical_resource` rather than a single `channel_key` lookup — so a
pre-connect grant still finds an already-connected sibling sharing the same `hw_protocol_id`.
Running inside the grant's own critical section (rather than a separate, earlier one) is also
what closes Finding A below: see Fix A's correctness argument.

**Fix D (same Codex-review round): `CopEntry.transmits`, not "any executing COP", gates the
rejection.** The original check rejected on any resolved, different-CLL `executing_cop`,
regardless of what that COP actually does — over-blocking a grant against a sibling's merely
*executing* but non-transmitting COP (`CoptDelay`, `CoptUpdateparam`, `CoptRestoreParam`, or an
empty-data `CoptStopcomm`), none of which needs suspending under §9.4.13.3 use case 1's
"transmit queue" framing. `CopEntry.transmits` is set once, at `StartComPrimitive` call time
(`rpc_start_com_primitive`), from the same per-`cop_type`/`cop_data`/`cop_ctrl_data`
classification `TxItem::transmits()` applies at dispatch time (`SendRecv` transmits iff
`NumSendCycles != 0` — see Fix H below; `StartComm` always transmits; `StopComm` iff `cop_data`
was non-empty; everything else never does) — see §3's `TxItem::transmits()` discussion for why
the two classifications live in two places and how they're kept from drifting apart.

**Fix H (Codex review round 4): receive-only `CoptSendrecv` reclassified as
non-transmitting.** Both `TxItem::transmits()` and `CopEntry.transmits` classified
`Self::SendRecv { .. }`/`CoptSendrecv` as *unconditionally* transmitting, ignoring
`NumSendCycles` (`send_cycles_remaining` on the `TxItem` side). ADR-059 defines
`NumSendCycles == 0` as receive-only — a single non-repeating monitoring pass with no
corresponding bus write at all (`handle_send_recv`'s `should_transmit = send_cycles_remaining
!= 0` gate, which skips `PassThruWriteMsgs` entirely for this case, already relied on the same
semantics). The unconditional classification meant a receive-only monitor queued by a
non-holding CLL was needlessly parked behind a sibling's held `LOCK_PHYSICAL_TX_QUEUE`
(dispatch-time siphon, §3), and an already-executing receive-only monitor could wrongly block a
sibling's `LockResource(LOCK_PHYSICAL_TX_QUEUE)` grant (this section's busy check) — both wrong,
since monitoring is explicitly unaffected by this lock per §9.4.13.3 use case 1's "transmit
queue" framing.

Fixed at both classification sites, precomputed rather than patched after the fact:
`TxItem::transmits()`'s `SendRecv` arm now reads `*send_cycles_remaining != 0` (`!=`, not `>` —
`-1`, infinite cyclic send, must still classify as transmitting; only exactly `0` is
receive-only). `CopEntry.transmits` (`rpc_start_com_primitive`) now precomputes
`num_send_cycles` from `cop_ctrl_data` *before* the classification `match`, so the `CoptSendrecv`
arm reads `num_send_cycles != 0` — and the pre-existing later re-parse of the same field, inside
the `CoptSendrecv`-specific construction block further down the function, was deleted and
replaced with the same outer binding, so the two reads of `NumSendCycles` for one call can never
independently drift to different values. The `debug_assert_eq!` cross-check in
`dispatch_tx_item` (added for Fix D) continues to hold: a `send_cycles_remaining == 0` `SendRecv`
never produces a `CycleContinuation`, so `CopEntry.transmits`'s call-time value and every
dispatch's freshly-computed `TxItem::transmits()` value stay in agreement across the COP's whole
lifetime, not just its first cycle.

**`StartComm` is deliberately NOT given the same conditional treatment, considered and rejected
here rather than left as an oversight.** A no-init `CoptStartcomm` (no five-baud/fast-init
handshake) with an empty `cop_data` performs no bus write of its own, and could in principle be
reclassified the same way `SendRecv` was. But a no-init `CoptStartcomm` with a *configured*
tester-present would then execute mid-lock and start NEW periodic bus traffic via
`StartPeriodicMsg` — a case this ADR's existing TesterPresent accepted residual (Consequences,
below) does not cover: that residual is scoped to an *already-running* tester-present bypassing
suspension (ADR-081, pre-existing), not one starting fresh during a sibling's held
`LOCK_PHYSICAL_TX_QUEUE`. `StartComm` stays unconditionally transmitting, kept conservative on
purpose.

As part of this fix, the pre-existing "another CLL already holds a requested bit" rejection in
`rpc_lock_resource` changes from `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL` to `PDU_ERR_RSC_LOCKED`:
Table 21 does not list `RSC_LOCKED_BY_OTHER_CLL` as a legal `PDULockResource` return value —
that code is legal only for `PDUUnlockResource` (Table 22).

### 3. TX-queue lock: reuse the existing suspend/resume machinery instead of rejecting

The codebase already had a per-CLL TX-suspend mechanism built for the client-driven
`PDU_IOCTL_SUSPEND_TX_QUEUE`/`PDU_IOCTL_RESUME_TX_QUEUE` IOCTLs: `LogicalLinkState.tx_held`
(a FIFO `VecDeque<TxItem>`) and a `tx_suspended: bool` flag checked by
`events::dispatch_tx_item` for every item belonging to a CLL (including cyclic/periodic
follow-up cycles), with `drain_tx_held_backlog` flushing `tx_held` in FIFO order on resume via
a content-free `TxItem::ResumeWake`. This machinery is structurally exactly what §9.4.13.3 use
case 1 asks for, so `LockResource`'s TX-queue lock now reuses it instead of rejecting:

- **Two independent suspension sources.** `tx_suspended: bool` is replaced by two flags,
  `tx_suspended_by_ioctl` and `tx_suspended_by_lock`, plus an accessor
  `LogicalLinkState::tx_suspended() -> bool` (the OR of both). Without this split, a client's
  own `PDU_IOCTL_RESUME_TX_QUEUE` would incorrectly pierce another CLL's held TX-queue lock,
  and `UnlockResource` would incorrectly clear a client's own explicit
  `PDU_IOCTL_SUSPEND_TX_QUEUE`. `ioctl_suspend_tx_queue`/`ioctl_resume_tx_queue` touch only
  `tx_suspended_by_ioctl`; `tx_suspended_by_lock` is owned exclusively by
  `recompute_lock_tx_suspensions` (below) and never set or cleared anywhere else.
  `events::cancel_held_tx_items`'s `reset_suspended` parameter (used by
  disconnect/destroy/`PDU_IOCTL_RESET`/`PDU_IOCTL_CLEAR_TX_QUEUE`) now clears only
  `tx_suspended_by_ioctl` — critically, `PDU_IOCTL_RESET`/`PDU_IOCTL_CLEAR_TX_QUEUE` must never
  let a client bypass another CLL's held physical-resource lock, and disconnect/destroy's own
  `tx_suspended_by_lock` correction happens separately, via the recompute sweep, once
  `held_lock_mask` is actually cleared.
- **A recompute-from-scratch sweep, not incremental set/clear.**
  `service::recompute_lock_tx_suspensions(links: &mut HashMap<u32, LogicalLinkState>) ->
  Vec<u32>` recomputes every CLL's `tx_suspended_by_lock` from the current `held_lock_mask`
  state of every CLL sharing its physical resource (via a `same_physical_resource` predicate
  factored out of, and shared with, the pre-existing `find_physical_lock_holder`), returning
  the handles of every CLL whose *effective* suspension (`tx_suspended()`) transitioned
  true→false. Recompute-from-scratch was chosen over incremental updates because
  `find_physical_lock_holder`'s resource-scope comparison — `channel_key` equality once
  connected, `hw_protocol_id` equality as a pre-connect fallback — can change which CLLs count
  as "the same resource" as a CLL connects after a lock was already granted against it via the
  fallback. An incremental update computed once at grant time cannot self-correct when that
  happens; recomputing on every lock-state-affecting transition can. It is called (and its
  returned handles sent a `TxItem::ResumeWake` each) from the seven sites in the table below.
- **Lock ordering.** `rpc_lock_resource` and `rpc_unlock_resource` now acquire
  `shared_channels` as the outermost guard (ADR-080), since both the active-transmission check
  (§2 above) and the resume-wake sends read a `SharedChannel`. `rpc_lock_resource` preserves
  ADR-110's `api`-before-`logical_links` sub-order underneath it
  (`shared_channels` → `api` → `logical_links`); `rpc_unlock_resource` still does not acquire
  `api` at all (`shared_channels` → `logical_links`), unchanged from ADR-110's original
  reasoning that releasing a lock while an apply is in flight can only make an
  already-computed exclusion conservative, never unsafe. `finalize_connected_link` (the
  connect-time site, see the invariant below) follows the same
  `shared_channels` → `logical_links` order for the identical reason: its resume-wake sends
  also need a `SharedChannel` lookup per resumed sibling.
- **`rpc_primitive.rs`'s hard-reject is deleted outright.** `CoptSendrecv`/`CoptStartcomm`/
  non-empty-data `CoptStopcomm` now always enqueue; `dispatch_tx_item`'s siphon (below) holds
  them in `tx_held` for as long as `tx_suspended_by_lock` is set, and the resume sweep above
  flushes them once the lock releases.
- **The governing invariant (restructured from the separately-appended Fix A/E/F notes across two Codex-review rounds
  into a single statement — every subsequent finding in this area has been a new instance of
  the same shape, not a new problem).** Any state change that affects a CLL's
  `tx_suspended_by_lock` inputs, or a CLL's visibility to the dispatcher, must run
  `recompute_lock_tx_suspensions` inside the *same* `logical_links` critical section that makes
  the triggering change, under `shared_channels`; and lock-only suspension gates only
  transmitting traffic, symmetrically at siphon time and drain time, with the FIFO no-overtake
  clause applying only at fresh-siphon time (an item with something genuinely ahead of it in the
  backlog), never at drain-re-entry time (where everything remaining is behind, not ahead). Eight
  sites are instances of this invariant:

  | Site | Trigger | Found | History |
  |---|---|---|---|
  | `rpc_lock_resource` | grant | Round 1 (original decision); the busy-check race, Codex-review round 2 Finding A | siphon-check-then-mark (`dispatch_tx_item`) and busy-check-then-grant unified under one `logical_links` critical section (Fix A/C) |
  | `rpc_unlock_resource` | release | Round 1 (original decision) | recompute sweep runs under `shared_channels` → `logical_links`, same critical section that clears `held_lock_mask` |
  | `rpc_disconnect_com_logical_link` | disconnect | Round 1 (original decision) | recompute sweep runs after this CLL's `held_lock_mask` is cleared, same critical section |
  | `rpc_destroy_com_logical_link` | destroy | Round 1 (original decision) | recompute sweep runs after this CLL's state is removed, same critical section |
  | `finalize_connected_link` | connect publication | Codex-review round 3 Finding E | sweep moved from a separate, later `rpc_connect_com_logical_link` block into the same critical section that publishes `connected`/`channel_key` |
  | `dispatch_tx_item` siphon / `drain_tx_held_backlog` pop gate | dispatch-time gating (not a state-change site, but the invariant's *consumer*) | Codex-review round 2 Finding B (transmits-only gating), round 3 Finding F (drain-re-entry livelock) | FIFO no-overtake clause gated to fresh-siphon only via `is_backlog_drain`; the naive per-item fix that gated only the pop condition, without also suppressing the siphon's FIFO clause on re-entry, produced a deterministic livelock (see below) |
  | `autodetect_sae_j1850_flavor` | pre-connect `hw_protocol_id` re-resolution | Codex review round 3, Finding G | sweep moved into the same critical section as the write |
  | `handle_channel_hard_error` | channel/module hard error taking a CLL offline | Codex-review round 5, Finding I | clears `held_lock_mask` and runs the recompute sweep inside the same `shared_channels` → `logical_links` critical section that takes each affected CLL offline, mirroring the established pattern; its one call site (`poll_rx_inner`) was restructured so `api`'s guard drops before this function is invoked, preserving ADR-080's `shared_channels`-outermost ordering (confirmed non-sites, per design-advisor: `rpc_module_disconnect`'s `links.clear()` — no CLL survives to hold a stale lock — and `release_uudt_companion_channel`, UUDT-only/RX-only, already covered by the existing accepted-residual bullet below) |

  A dead CLL taken offline by a hard error can never call `UnlockResource` itself (its channel is
  gone) and its client may never even learn the link died, so `handle_channel_hard_error` is
  treated here as the `SUSPEND_TX_QUEUE`-lock analog of §9.4.13.3 use case 3's automatic-unlock
  (the spec names only Destroy/Disconnect explicitly; extending it to forced-offline is a
  protocol-interpretation call made here, justified because a dead CLL's held lock protects
  nothing and would otherwise permanently starve a sibling with no recovery path).

  **Deliberately-excluded non-sites.** `CreateComLogicalLink`'s `LogicalLinkState` map-insert
  (`rpc_link.rs`, ~line 949) is not a site: a freshly-created CLL starts with
  `channel_key: None`, `held_lock_mask: 0`, `tx_suspended_by_lock: false`, so no sibling's
  `same_physical_resource`/`held_lock_mask` inputs change, and the new CLL cannot enqueue TX
  before `channel_key` is set anyway (only via `finalize_connected_link`, already covered
  above). `tx_suspended_by_ioctl` writes (`ioctl_suspend_tx_queue`/`ioctl_resume_tx_queue`) are
  also not a site for this invariant: `tx_suspended()`'s `resumed` computation reads it, but
  writes to it never change any CLL's `tx_suspended_by_lock`, so there is nothing for a sweep
  to recompute there.

  **Correctness argument, `rpc_lock_resource` vs. the siphon (Finding A):** either the sibling
  poll task takes `logical_links` first — `dispatch_tx_item` marks `executing_cop` before
  dropping the guard, so the grant's busy check (under the same `logical_links` guard) observes
  `Some(cop)` and rejects — or the grant takes `logical_links` first — `tx_suspended_by_lock` is
  set via `recompute_lock_tx_suspensions` (under the same guard) before the sibling's siphon
  check can run, so the sibling's item gets siphoned. No interleaving lets a transmission slip
  through ungated.

  **Correctness argument, `finalize_connected_link` vs. a concurrent grant (Finding E):** both
  connect's sweep and a concurrent `LockResource` grant's sweep serialize on `shared_channels` →
  `logical_links`. Whichever runs first, the other's `recompute_lock_tx_suspensions` call (which
  reads the full current `links` state) sees the final, consistent picture: if the grant runs
  first, connect's sweep (running after) sees the already-granted `held_lock_mask`; if connect
  runs first, the grant's sweep (running after, since `rpc_lock_resource` already holds
  `shared_channels` throughout its own critical section per Fix A) sees the real `channel_key`.
  No interleaving leaves a newly-connected CLL's suspension stale.
- **Lock-only suspension gates only transmitting traffic, symmetrically at siphon time and
  drain time (Fix B, extended by Fix F).** `dispatch_tx_item`'s siphon does not simply check
  `tx_suspended()` (the OR of both sources) unconditionally; the two sources have different
  scopes. `tx_suspended_by_ioctl` (the client's own explicit `PDU_IOCTL_SUSPEND_TX_QUEUE`) holds
  every item unconditionally — the client asked to suspend its whole queue, and both
  transmitting and non-transmitting items are part of "the queue." `tx_suspended_by_lock`
  (another CLL's held `LOCK_PHYSICAL_TX_QUEUE`) holds an item only when
  `item.transmits() || (!is_backlog_drain && !link.tx_held.is_empty())`:
  `TxItem::transmits()` classifies `SendRecv` as transmitting iff `send_cycles_remaining != 0`
  (`NumSendCycles == 0` is a receive-only monitoring pass with no bus write at all, ADR-059 —
  see §2's Fix H), `StartComm` as always transmitting (deliberately not given the same
  conditional treatment — see §2's Fix H rationale), `StopComm` iff
  its `tx` is `Some` (mirroring ADR-085's empty-data exemption), and
  `Delay`/`UpdateParam`/`RestoreParam`/`ResumeWake` as never transmitting — only actual bus
  traffic needs to wait on a lock scoped to "the transmit queue" (§9.4.13.3 use case 1's own
  wording). The `!link.tx_held.is_empty()` clause is **not** a transmission check; it is
  FIFO-preservation for the case where a transmitting item is already parked ahead of a
  non-transmitting one on the same CLL's queue: a non-transmitting item must never overtake an
  already-held transmitting item on its own CLL's queue, or `CopEntry`/`TxItem` order and the
  ComPrimitive queue's actual execution order would diverge — e.g. a held `CoptStartcomm`
  followed by a passing empty `CoptStopcomm` executing out of order would leave comm started
  when the client had already asked, in FIFO order, for it to be stopped. **This clause applies
  only at fresh-siphon time (`is_backlog_drain == false`), never at drain-re-entry time (Fix F,
  Codex-review round 3 Finding F).** `drain_tx_held_backlog` pops strictly from the *front* of `tx_held` and
  re-dispatches through this same siphon with `is_backlog_drain == true`; everything the FIFO
  invariant guarantees is still in `tx_held` at that point is strictly *behind* the popped item,
  not ahead of it, so the no-overtake clause is vacuously satisfied there and must not re-fire.
  The naive fix Codex review round 3 rejected — gating only `drain_tx_held_backlog`'s own pop
  condition on `!item.transmits()`, without also suppressing this siphon clause on the
  drain-re-entry path — produces a deterministic livelock: the drain loop pops the front
  non-transmitting item, `dispatch_tx_item` re-checks the (ungated) FIFO clause, finds `tx_held`
  still non-empty (whatever was behind the popped item), re-siphons it right back via
  `push_front`, and the drain loop pops the identical front item again next iteration —
  forever, no race required to trigger it. `drain_tx_held_backlog`'s own pop gate mirrors this
  symmetry: `!link.tx_suspended_by_ioctl && (!link.tx_suspended_by_lock ||
  link.tx_held.front().is_some_and(|item| !item.transmits()))` — it may pop a non-transmitting
  front item under lock-only suspension, but never under ioctl suspension (unconditional, as
  above), and never a *transmitting* front item under lock-only suspension either way.
- **`ioctl_clear_tx_queue` keeps its own independent reject, unchanged.** It is not one of
  §9.4.13/§9.4.14's named functions, use case 1 only prescribes suspend/resume semantics for
  the ComPrimitive queue path, and — unlike enqueuing a COP, which has nothing to undo while
  waiting — `PDU_IOCTL_CLEAR_TX_QUEUE` is destructive/irreversible, so "just wait for the lock"
  is not an available option for it. Its rejection stays a synchronous
  `PDU_ERR_RSC_LOCKED_BY_OTHER_CLL`, side-effect-free and conservative.

### 4. Strict lock-mask validation, both directions

Both `rpc_lock_resource` and `rpc_unlock_resource` now reject, at the top of the function
before any other logic, a `lock_mask` that is zero or contains any bit outside
`LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE` — even when a defined bit is also set —
with `PDU_ERR_INVALID_PARAMETERS` (`Code::InvalidArgument`). This is stricter than "reject
only an entirely-undefined mask" (one plausible reading of the audit finding's wording),
decided deliberately: §9.4.13.2 a)/§9.4.14.2 a) both say "validate all input parameters,"
Table D.2 defines only two bits, and silently narrowing a mixed mask (the prior behavior)
would let a client believe an undefined bit was locked/unlocked when it was not — a false
success is worse than a loud rejection here, and is also the more easily recoverable failure
mode if a future spec edition defines additional bits (no 2022-edition text is available in
this workspace to confirm either way — see this repo's `CLAUDE.md` "Spec References" section).

## Alternatives Considered

1. **Keep the hard-reject for COPs under a foreign TX-queue lock.** Rejected: contradicts
   §9.4.13.3 use case 1's explicit SUSPEND_TX_QUEUE/RESUME_TX_QUEUE wording; a client-visibly
   different, stricter behavior than the spec describes.
2. **A single shared `tx_suspended` flag, reused as-is for both the IOCTL and the lock
   source.** Rejected: `UnlockResource` would then also clear a client's own explicit
   `PDU_IOCTL_SUSPEND_TX_QUEUE`, and a client's own `PDU_IOCTL_RESUME_TX_QUEUE` would pierce a
   sibling's held `LOCK_PHYSICAL_TX_QUEUE` lock. Two independent sources need two independent
   flags.
3. **Incremental suspend/resume of sibling CLLs at grant/release time**, instead of a
   recompute-from-scratch sweep. Rejected: leaks a stale suspension under the pre-connect
   `hw_protocol_id`-fallback resource-scope comparison — e.g. a lock granted before the holder
   connects, followed by the holder later connecting to a real `channel_key` that changes which
   CLLs match "the same resource," would never be corrected by a one-time incremental update.
   The recompute sweep is self-healing by construction.
4. **Count queued/parked `TxItem`s as "active transmissions"** for §9.4.13.2 b)'s check.
   Rejected: queued items are exactly what TX-queue-lock suspension exists to hold; treating
   them as blocking would make the lock nearly ungrantable on any channel with pending traffic.
5. **Silently mask off undefined lock-mask bits** (the audit finding's minimal reading).
   Rejected: grants/releases an undefined "privilege" the service cannot actually honor and
   gives the client no signal that part of its request was dropped, violating both functions'
   explicit "validate all input parameters" behavior step.

## Consequences

- `LockResource`'s and `UnlockResource`'s critical sections now include `shared_channels`
  (ADR-080), serializing them against connect/disconnect/destroy and TX-queue lookups for
  their duration — correctness-mandated, not a performance concern (the sections are short).
- `UnlockResource` can now fail (`RSC_NOT_LOCKED`/`RSC_LOCKED_BY_OTHER_CLL`) where it previously
  always succeeded; existing and new client integrations must handle these two return codes.
- `LockResource`'s conflicting-holder rejection changes from `RSC_LOCKED_BY_OTHER_CLL` to
  `RSC_LOCKED`; any client or test asserting the old code on this specific path needs updating
  (done in this change's own regression tests).
- A `CoptSendrecv`/`CoptStartcomm`/non-empty-data `CoptStopcomm` issued by a non-holding CLL
  while another CLL holds `LOCK_PHYSICAL_TX_QUEUE` now returns a COP handle and queues
  (`PDU_COPST_IDLE`, since `CopEntry::dispatched` stays `false` while siphoned) instead of
  failing synchronously; it executes once the lock releases. This is a client-visible behavior
  change from the pre-ADR-123 hard-reject, intentionally, to match spec conformance.
- **Accepted residual — TesterPresent bypasses `tx_suspended`/`tx_held` entirely (pre-existing,
  ADR-081), so a TX-queue lock does not suspend it.** Spec NOTE at line 1812 says suspension
  should also stop tester-present messages; this codebase's tester-present handling
  deliberately runs outside the TX-suspend/`tx_held` path for both suspension sources (the
  IOCTL and, now, the lock) — not newly introduced or widened by this change.
- **The call-time-check-vs-in-flight-send window on a `LOCK_PHYSICAL_TX_QUEUE` grant is CLOSED
  for every currently-known state-change site, not merely narrowed — but "currently known" is
  doing real work in that sentence, and the invariant in §3 is what would need re-verifying if a
  future PR adds a new site.** This ADR's first cut left a real race here (ADR-110 had already
  flagged the TX-queue-lock version of this window as an explicit out-of-scope backlog item,
  `j2534-0404-service/docs/implementation-notes.md`, before this ADR existed): the grant's busy
  check and `dispatch_tx_item`'s siphon-check-then-mark ran under separate, non-overlapping
  critical sections, so a transmission could slip between a "not suspended yet" siphon read and
  a "not executing yet" grant read. Codex-review round 2's Finding A closed this specific
  instance structurally, by giving both a shared serialization point — the same `logical_links`
  critical section — rather than shrinking it further (see §3's invariant and Finding A's
  correctness argument). That closure claim, made in Codex-review round 2, turned out to be
  scoped more narrowly than its original wording implied: Finding E (Codex-review round 3, a
  later pass over the same PR) found a *second*, structurally identical door through the same
  wall — connect-time publication (`finalize_connected_link`) running its
  `recompute_lock_tx_suspensions` sweep in a critical section separate from the one that
  published `connected`/`channel_key` — closed the same way, by moving the sweep into the
  publishing critical section (§3's invariant table). Finding G (Codex-review round 3, the same
  pass as Finding E/F) found a *third* instance of the identical door — `autodetect_sae_j1850_flavor`
  writing `hw_protocol_id` (one of `same_physical_resource`'s two pre-connect-fallback comparison
  fields) in a `logical_links`-only critical section with no recompute — closed the same way, by
  moving the sweep into the write's own critical section (§3's invariant table). Finding I
  (Codex-review round 5) found a *fourth* instance, of a different shape than the first three:
  `handle_channel_hard_error` (a forced-offline site, not a state-*change*-vs-*consumer* race)
  never cleared `held_lock_mask` at all, so a dead CLL's held `LOCK_PHYSICAL_TX_QUEUE` lock stayed
  "held" from `recompute_lock_tx_suspensions`'s perspective forever, permanently starving a
  sibling — closed the same way, by clearing `held_lock_mask` and running the recompute sweep
  inside the same critical section that takes the CLL offline (§3's invariant table); closing it
  also required hoisting the function's one call site out from under `api`, since the function
  newly acquires `shared_channels` and ADR-080 requires `shared_channels` to stay outermost. The
  pattern across all four rounds is: this class of issue is closed at every site anyone has found
  so far, and each time a new site turns up it is closed the same way (recompute inside the same
  `logical_links` critical section as the triggering state change, under `shared_channels`) rather
  than requiring a new mechanism — but nothing in the code enforces the invariant structurally for
  a *hypothetical future* site; a future PR that adds a ninth way to change
  `held_lock_mask`-adjacent state or a CLL's dispatcher visibility must independently apply it.
  Design-advisor's round-3 review enumerated this class exhaustively by two independent methods
  (every `recompute_lock_tx_suspensions` call site, and every write site of each field
  `same_physical_resource`/`held_lock_mask` scans depend on) and found Finding G to be the last
  remaining instance of *that* enumeration's scope (state-change sites already inside a
  `logical_links` critical section) — but see the immediately preceding caveat: that confirmed
  only that no known site remained *within that scope*, not that no future PR (or, as round 5
  showed, no differently-shaped existing site outside that scope) could add one. What remains
  specific to the `rpc_lock_resource`-grant-vs-siphon interaction (Finding A), all narrower and
  accepted:
  - **Untracked-COP window.** `executing_cop` and `primitives` are independently locked and
    updated non-atomically by the poll task: a COP removed from `primitives`
    (cancellation/staleness paths in `events.rs`) slightly before `dispatch_tx_item` clears
    `executing_cop` looks, for that instant, like "nothing to block on" to the grant's busy check
    (§2's Fix C, `owner == None` case) even though the poll task may still be mid-handler for it.
    Bounded, not open-ended: every hardware write the poll task could still be performing in that
    window serializes on the same `api` mutex the grant also holds, so the grant cannot complete
    and return "safe to assume the resource is free" while that write is still in flight underneath
    it — only the bookkeeping read is stale, not the hardware serialization.
  - **TesterPresent bypass (pre-existing, ADR-081, unchanged by this ADR).** See the bullet
    above — restated here because it is the same "narrower residual" class as this one, not a new
    finding.
  - **Transient false-positive rejection.** A COP that just finished, whose `executing_cop` clear
    happens outside the `logical_links` guard (in the `dispatch_tx_item` handler path, not the
    siphon-check-then-mark block Fix A serializes), can transiently still read as "executing" to a
    concurrent grant's busy check and cause a spurious rejection. Retryable and harmless — strictly
    more conservative than the bug this ADR fixes, not a new safety gap.
- **`drain_tx_held_backlog`'s own pop gate now correctly drains a leading non-transmitting item
  under lock-only suspension (Finding F, Codex-review round 3); only a non-*front* non-transmitting item
  stays held, and that is correct FIFO behavior, not a residual bug.** An earlier version of
  this bullet claimed the pre-Finding-F pop gate ("refuse to pop anything at all while
  `tx_suspended_by_lock` alone is set") was "conservative by design," reasoning that "a
  non-transmitting item can only reach `tx_held` via the Fix B `!link.tx_held.is_empty()` clause
  \[i.e. something transmitting is already ahead of it\]." That premise was wrong: it ignored
  the ioctl-suspend-then-resume-under-lock path (Finding F's own scenario) — a non-transmitting
  item can also reach `tx_held` via `tx_suspended_by_ioctl`'s unconditional siphon while sitting
  at the *front*, with nothing transmitting ahead of it at all, and there is no correctness
  reason to keep such an item held once the client's own `PDU_IOCTL_RESUME_TX_QUEUE` clears
  `tx_suspended_by_ioctl`. §3's invariant now states the corrected rule: a front-of-backlog
  non-transmitting item drains under lock-only suspension (nothing is ahead of it to reorder
  against); an item stays held only while something transmitting is still in front of it in
  `tx_held` — which *is* the correct FIFO behavior the original bullet was trying to describe,
  just misattributed to the wrong condition (every leading non-transmitting item drains one at a
  time as it reaches the front, in FIFO order, rather than only the CLL's queue resuming as a
  whole).
- **§2's Fix D busy check is resource-scoped, not CLL-scoped — it does not exempt the requesting
  CLL's own executing, transmitting COP (Finding J, Codex-review round 5).** The check as
  originally written (and as Fix D itself, which only changed which COPs the check counts, left
  unchanged) additionally required the executing COP's owner to be a *different* CLL than the
  requester (`other != handle`) before rejecting. That premise was wrong: ISO 22900-2:2009(E)
  §9.4.13.2 b) directs an implementation to look at two things on the resource before granting:
  locks held by others, and transmissions currently in progress. Grammatically, the word "other"
  attaches only to the *locks* item (a CLL cannot lock-conflict with a lock it already holds
  itself); the in-progress-transmissions item carries no such qualifier and applies to the
  resource as a whole. The `other != handle`
  conjunct let a CLL successfully lock `LOCK_PHYSICAL_TX_QUEUE` while its own COP was actively
  transmitting on the same resource — harmless in the sense that a grant never suspends its own
  holder's traffic, but a spec-nonconformant grant nonetheless, since the check as specified has
  no self-exemption. Fixed by dropping the `other != handle` conjunct; the `owner == None`
  (untracked-COP) exemption is unrelated and unchanged. Design-advisor confirmed every pre-existing
  regression test for this check used requester ≠ owner, so this correction could not flip any
  existing test; a new regression test,
  `lock_resource_rejects_active_transmission_owned_by_the_requesting_cll_itself`
  (`tests/grpc_mock/locks_and_param_classes.rs`), covers the requester-owns-the-COP case directly.
- **The relocated busy check (§2's Fix C) widens the grant's `shared_channels` → `api` →
  `logical_links` critical section by one `executing_cop` read per physical-resource-matching
  channel.** Bounded by live channel count (typically one or a small handful sharing a physical
  bus), and correctness-mandated — the check must run inside the same critical section Fix A's
  interleaving argument depends on — not a performance concern, matching this ADR's existing
  framing for the `shared_channels`-inclusion consequence above.
- **Accepted residual — §9.4.13.3 use case 4 / §9.4.14.2 c)'s `PDU_IT_INFO` lock-status-change
  callback remains unimplemented.** Not part of A2-15's original finding list; a separate,
  future audit item.
- **Accepted, out-of-scope-because-inapplicable observation — a CLL's UUDT companion channel
  (`LogicalLinkState.uudt_channel_id`/`uudt_channel_key`, `ensure_uudt_companion_channel` in
  `rpc_link.rs`) is RX-only and never carries TX traffic, so it is unaffected by this ADR's
  suspend/resume machinery.** Every `TxItem` enqueue site (`rpc_primitive.rs`) resolves its
  `tx_queue` exclusively from `link.channel_key`'s `SharedChannel`, never
  `uudt_channel_key`'s; `same_physical_resource` (used by both `find_physical_lock_holder` and
  `recompute_lock_tx_suspensions`) likewise compares only `channel_key`, never
  `uudt_channel_key`. Confirmed by code inspection during the round-3 review, not by a new
  regression test — there is no TX-side behavior to test.
- `ioctl_clear_tx_queue`'s own `LOCK_PHYSICAL_TX_QUEUE` rejection is unchanged — still a
  synchronous, side-effect-free reject, now documented as an intentionally-retained divergence
  from the COP-queuing behavior above rather than as "mirroring" a check that no longer exists.
- Test files updated: `j2534-0404-service/tests/grpc_mock/stopcomm_data_tx.rs` and
  `tests/grpc_mock/pdu_ioctl.rs` (two pre-existing tests asserting the old hard-reject,
  rewritten to assert queue-then-dispatch-after-unlock); six tests added to
  `tests/grpc_mock/locks_and_param_classes.rs` covering each of the four original fixes above,
  including a newly-connecting CLL inheriting a sibling's TX-queue-lock suspension. Codex-review
  round 2 (Fixes A/B/C/D) added three more regression tests to the same file:
  `non_transmitting_item_from_a_non_holding_cll_is_not_held_by_a_siblings_tx_queue_lock` (Fix B),
  `lock_resource_pre_connect_rejects_active_transmission_on_same_hw_protocol_id` (Fix C), and
  `lock_resource_is_not_blocked_by_a_siblings_non_transmitting_executing_cop` (Fix D). Finding
  A's race is closed by the code-structure/lock-ordering argument in §3's invariant, not by a
  regression test — the existing mock harness has no deterministic way to force the exact
  poll-task/RPC interleaving the race depended on (same regression-test-feasibility precedent as
  ADR-110's amendment); Finding E is likewise structural-argument-only, for the same reason.
  Codex-review round 3 added Finding E
  (`finalize_connected_link`'s sweep publication race) and Finding F
  (`drain_tx_held_backlog`'s pop gate ignoring `transmits()`, and the deterministic livelock in
  the naive per-item fix — see §3's invariant bullet). Unlike Finding E, Finding F IS
  deterministically testable and has a dedicated regression test:
  `ioctl_resume_drains_leading_non_transmitting_backlog_item_under_lock_only_suspension`
  (`tests/grpc_mock/locks_and_param_classes.rs`) — verified fail-without/pass-with: reverting the
  `is_backlog_drain` guard in `dispatch_tx_item`'s siphon while keeping the corrected
  `drain_tx_held_backlog` pop gate reproduces the naive fix exactly, and the test fails cleanly
  (does not hang the test process — see the test's own doc comment for why: the client-side
  `wait_for_event` helper has its own bounded internal timeout, so the client-observable failure
  surfaces at that deadline regardless of whether the server-side poll task is genuinely spinning
  forever underneath it; independently confirmed by re-running with a 15s internal timeout and
  observing the failure land at exactly 15s rather than sooner, consistent with the underlying
  task never making progress on its own). Finding G
  (`autodetect_sae_j1850_flavor`'s `hw_protocol_id` write) is, like Finding A and Finding E,
  structural-argument-only — the code-structure/lock-ordering argument in §3's invariant table
  and the "Correctness argument" precedent above apply directly (the write and any concurrent
  grant's sweep both now serialize on `shared_channels` → `logical_links`), and there is no
  deterministic way in this mock harness to pause execution between the write and a concurrent
  grant's read without a test-only production gate hook this codebase does not have; forcing one
  would only produce a flaky test, which is not acceptable regression-test practice here.
  Codex review round 4 added Finding H (receive-only `CoptSendrecv` misclassified as
  transmitting, §2's Fix H) — unlike Findings A/E/G, this one IS deterministically testable, and
  has two dedicated regression tests in `tests/grpc_mock/locks_and_param_classes.rs`, one per
  independent classification site:
  `receive_only_sendrecv_from_a_non_holding_cll_is_not_held_by_a_siblings_tx_queue_lock`
  (`TxItem::transmits()`'s siphon-time classification) and
  `lock_resource_is_not_blocked_by_a_siblings_receive_only_executing_sendrecv`
  (`CopEntry.transmits`'s busy-check classification) — verified fail-without/pass-with
  independently for each: reverting only the `TxItem::transmits()` `SendRecv` arm reproduces the
  first test's bug (the receive-only item stays parked in `tx_held` and `wait_for_cop_finished`
  times out cleanly at its own 2s bound, not a hang); reverting only `CopEntry.transmits`'s
  `CoptSendrecv` arm reproduces the second test's bug by tripping `dispatch_tx_item`'s
  `debug_assert_eq!` cross-check (added for Fix D) the moment the receive-only COP dispatches,
  which fails the test cleanly before the `LockResource` call under test is even reached.
  Codex review round 5 added Finding I (`handle_channel_hard_error` never released a dead CLL's
  held TX-queue lock, §3's 8th invariant-table row) and Finding J (the active-transmission busy
  check wrongly exempted the requesting CLL's own transmission, §2's Fix D correction below).
  Finding J has a dedicated regression test,
  `lock_resource_rejects_active_transmission_owned_by_the_requesting_cll_itself`
  (`tests/grpc_mock/locks_and_param_classes.rs`) — verified fail-without/pass-with: temporarily
  restoring the removed `other != handle` conjunct reproduces the bug (the self-owned grant
  wrongly succeeds instead of being rejected), confirmed to fail cleanly (no hang) before
  restoring the fix. Design-advisor confirmed every pre-existing `LOCK_PHYSICAL_TX_QUEUE`
  busy-check test uses requester ≠ owner, so removing this conjunct cannot flip any of them; the
  full-suite run for this round confirms no regression. **Finding I is structural-argument-only,
  for a different reason than Findings A/E/G's "cannot force the interleaving" — Finding I's bug
  was a stable END STATE (once `handle_channel_hard_error` ran, the effect was permanent, not a
  narrow race window), so a deterministic test is possible in principle; the reason no test was
  added this round is a mock-harness capability gap, not an unforceable interleaving: the mock
  (`j2534-0404-mock/src/lib.rs`) has no fault-injection hook for a `PassThruReadMsgs` failure (the
  precedent hooks, `__mock_set_fast_init_error`/`__mock_set_stop_filter_error`, cover
  `IOCTL_FAST_INIT`/`PassThruStopMsgFilter` only) — adding one (e.g.
  `__mock_set_read_msgs_error(channel_id, code)`) plus a full dual-channel test was judged too
  large a scope addition for this round and deferred.**

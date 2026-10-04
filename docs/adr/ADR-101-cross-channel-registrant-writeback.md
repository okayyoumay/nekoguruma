# ADR-101: Cross-Channel Registrant Bookkeeping Uses Delta/Monotone Merge, Not Absolute Overwrite

**Date:** 2026-07-18
**Status:** Accepted (amends ADR-100)
**Affects:** `j2534-0404-service` events

## Context

ADR-100 made `LogicalLinkState.registrants: Vec<CopRegistrant>` per-CLL, shared state,
snapshotted into `CllRxEntry` once per poll pass by `build_cll_rx_entries` and merged back into
the live registrant after `bind_frame` processes that pass's frame batch (`events.rs:1609-1634`).
This assumed a single writer per registrant per poll cycle — true for a CLL on a single physical
channel, but not for a CLL with a UUDT companion channel (ADR-046, software-isotp functional
addressing): `LogicalLinkState` carries both `channel_id` (primary) and `uudt_channel_id`
(companion), two genuinely separate physical channels, each with its own independently-spawned
`poll_channel_events` task (`rpc_link.rs:1836`, `events.rs:1793`/`1800`). Both channels'
`build_cll_rx_entries` calls pick up the SAME CLL (filtered by
`l.channel_id == Some(channel_id) || l.uudt_channel_id == Some(channel_id)`, `events.rs:297`),
so the same registrant can be snapshotted, mutated, and merged back by two concurrently-running
poll tasks with no synchronization between the two full snapshot→process→writeback sequences.

A Codex review of PR #103 (ADR-100's implementation) caught this: the writeback merge writes
absolute snapshot state back to the live registrant
(`live.matches_got = snap.matches_got; live.pending_rc = snap.pending_rc; live.cyclic_deadline =
snap.cyclic_deadline;`), so a later pass's writeback — cloned from live state *before* an earlier
pass's writeback landed — silently discards the earlier pass's contribution. Concretely: a
`NumReceiveCycles = 2` COP receiving one matching response on the primary channel and one on the
companion channel can have both frames correctly delivered with the COP's handle, yet the live
`matches_got` counter sticks at 1 and the COP times out waiting for a second match it already
received.

This is a genuinely new gap, not one ADR-100 inherited: the `MatchProbe` mechanism it replaced
was constructed fresh per call, entirely on the stack of the one `wait_for_expected_response`
call driving it, and was only ever passed into `poll_rx_inner` for the PRIMARY channel's own
`ctx` (`wait_for_expected_response` is only ever invoked from `handle_send_recv`, itself only
ever dispatched by the primary channel's own `TxItem` queue). A response arriving on the
companion channel's own physical connection was therefore never visible to the old probe at all
— it could not be attributed to the COP by that mechanism, full stop. ADR-100's per-CLL shared
registrant is a genuine improvement here (a companion-channel response can now be correctly
attributed at all), but the writeback step was never built to expect two concurrent writers, and
the same gap independently affects `poll_rx_and_check_match`'s own before/after diffing (see
Decision, item B): even with a correct merge, its private "matches at the start of this call"
baseline absorbs any companion-channel increment that lands between two of the primary channel's
own calls, so the wait loop's locally-tracked match count never observes it either.

## Decision

Two coupled changes are both required — the merge fix alone does not close Codex's reported
scenario, since `poll_rx_and_check_match`'s own diffing has the identical baseline problem one
layer up.

### A. Delta/monotone merge at writeback (`events.rs:1609-1634`)

`build_cll_rx_entries` (`events.rs:747-759`, the same lock acquisition that already clones
`registrants` into `CllRxEntry`) additionally captures a per-registrant baseline:
`registrant_baselines: Vec<RegistrantBaseline { cop_handle: u32, matches_got: u32, pending_rc:
Option<u8> }>`, index-aligned with (or matched by `cop_handle` against) the registrant clone.
Kept as a separate field on `CllRxEntry` rather than added to `CopRegistrant` itself, so the
baseline — meaningful only for the writeback merge — doesn't pollute the live registrant struct
or its ~10 existing test constructors.

The merge, inside the existing `live.connect_generation == snap.connect_generation` guard:

- **`matches_got`** (a per-pass-only-incrementing counter): `live.matches_got =
  live.matches_got.saturating_add(snap.matches_got - base.matches_got)` — a pass only ever
  increments its own clone, so `snap.matches_got >= base.matches_got` always holds
  (`debug_assert!` it). This sums each pass's own contribution instead of overwriting with
  whichever pass's absolute count landed last.
- **`pending_rc`** (consumer-reset by `poll_rx_and_check_match` before its own poll pass, set at
  most once per registrant lifetime — `bind_registrant`'s `if r.pending_rc.is_none()` guard, first
  code wins): `if base.pending_rc.is_none() && let Some(code) = snap.pending_rc &&
  live.pending_rc.is_none() { live.pending_rc = Some(code); }` — merges only a transition made
  *by this specific pass* (baseline `None` → snapshot `Some`), and never overwrites a live `Some`
  a different pass (or a consumer reset racing this merge) already established. A simpler
  "only overwrite None→Some" rule without the baseline is insufficient: a snapshot taken *before*
  a consumer reset, itself unchanged at `Some(x)`, would otherwise resurrect a stale `x` over a
  live `None` the reset legitimately produced.
- **`cyclic_deadline`** (restarted forward-only on each accepted match, `cyclic_timeout_ms` frozen
  at registration and never live-updated, cleared only by removing the registrant entirely — never
  by any pass): `live.cyclic_deadline = live.cyclic_deadline.max(snap.cyclic_deadline)` — no
  baseline needed; taking whichever deadline is further in the future is correct because a
  deadline only ever moves forward or the registrant stops existing. `Option<Instant>`'s derived
  `Ord` (`None < Some`) gives the right behavior for a registrant with no cyclic deadline
  configured at all (ADR-100 Decision §4's scope) for free.
- **`tier`** (added by ADR-100's round-9 Finding-2 correction; one-way transition,
  `ActiveSendReceive → ReceiveOnly`, never the reverse, applied inline in `bind_registrant`'s own
  match-acceptance step by `migrate_on_first_match`-flagged registrants): `if snap.tier ==
  RegistrantTier::ReceiveOnly { live.tier = RegistrantTier::ReceiveOnly; }` — a one-way latch, no
  baseline needed for the same reason `cyclic_deadline`'s merge needs none: a migration only ever
  moves forward (tier-1 → tier-2) or the registrant stops existing, never the reverse, so "adopt
  the more-migrated of the two" is unconditionally correct regardless of which pass's snapshot is
  newer. This SUPERSEDES this section's own prior "accepted transient" bullet below, which claimed
  the merge never writes `tier` at all — see that bullet's own correction.

**Correction (Codex review of PR #103, round 9, Finding 2):** this section previously accepted, as
a transient not worth fixing, that `migrate_registrant_to_receive_only` (ADR-100 S5) flipped a
registrant's `tier` on the live struct directly, invisible to this merge (which "never writes
`tier`"), so an in-flight companion-channel snapshot taken just before that flip would carry the
old tier for the remainder of its own pass. That framing undersold the actual exposure: the same
staleness applies WITHIN a single poll task's own batch, not just across channels — a registrant
migrating mid-batch (its first accepted match at frame index N, with the same
`PassThruReadMsgs` batch also containing frames after index N) left every later frame in that same
batch scanned against the stale tier-1 snapshot, since neither `bind_registrant` nor this merge
touched `tier` at all — capable of the SAME capture-bug shape this whole ADR-100/101 pair exists
to close, re-opened for one batch's worth of frames at the transition moment. Fixed by ADR-100's
round-9 Finding-2 correction (the in-batch `migrate_on_first_match` flip) plus the new `tier` merge
bullet above — the intra-batch (same-poll-task) case is now fully closed.

**Accepted residual (Codex review of PR #103, round 10) — one overlapping SIBLING-channel pass,
narrower than the intra-batch case just closed, structurally unclosable at this ADR's own already-
established cost:** the in-batch flip mutates only the accepting task's own pass-local clone; a
SIBLING channel's poll task (either direction — the first positive response can arrive on either
the primary or the companion, so this is not "companion-only" staleness) that already cloned
`registrants` before that flip's own end-of-pass writeback lands has no way to observe it — its
clone is an independent copy, not a shared reference, and the one-way `tier` latch above only
prevents that sibling's eventually-stale writeback from REVERTING an already-migrated live
registrant; it does nothing for frames the sibling's OWN pass already bound/delivered during the
one overlapping pass itself. Investigated for a cheaper, narrower fix than the general
snapshot→process→writeback serialization this ADR's own Alternatives Considered section already
rejected (which would hold a lock across `process_frame_for_entry`'s real hardware I/O under the
separate `api` lock) — an eager tier write-through mirroring Decision §D's `cyclic_deadline`
mechanism was considered and rejected: unlike §D's race, which has a DESTRUCTIVE consumer (a reap
deletes the registrant, permanently losing an in-time response), this race's consumer is
NON-DESTRUCTIVE — the frame is still delivered, correctly attributed to a COP that legitimately
matches it, merely at a stale (tier-1) rank for one pass. Eager write-through only helps a sibling
snapshot cloned in the sub-millisecond sliver between acceptance and the accepting task's own
writeback (negligible narrowing); actually closing the finding would require re-checking live tier
per-frame mid-pass on the sibling and re-binding at corrected rank — a mini-serialization that
collides with this ADR's own established "per-pass binding decisions are snapshot-consistent and
final, never re-offered" rule (Alternatives Considered), and still could not reach the true floor:
a sibling frame processed before the first match is accepted ANYWHERE has no cross-channel
ordering oracle at all (no host-correlated, cross-channel-comparable frame-arrival timestamp exists
in the J2534 API). Bounded blast radius: scoped to `migrate_on_first_match` (true IS-CYCLIC)
registrants on a dual-channel CLL only; at most one `PassThruReadMsgs` batch
(`MAX_POLL_MESSAGES` = 8 frames) on the sibling channel, self-healing at that sibling's very next
clone via the latch above; per-frame consequence is a frame binding at tier-1 rank instead of
tier-2 (can outrank a tester-present signature, delivering `ResultData` instead of a discard, or
outrank an older tier-2 monitor with equal claim) — narrower still in practice, since the
stale-ranked bind additionally requires that registrant's own descriptors to admit the sibling
channel's frames at all (`unique_resp_ids` empty, or containing the sibling's routed CAN IDs).
Same architectural class as ADR-100's Consequences "registrant snapshot vs. frame-drain ordering"
residual and this ADR's own Decision §D "drained but not yet bound" residual — see the
Snapshot-visibility principle in Consequences below for why all three are one class, not three
coincidences.

### B. `poll_rx_and_check_match` diffs against the caller's cumulative count, not its own private baseline

`poll_rx_and_check_match` (`events.rs:7527-7566`, line numbers as of this ADR's original
Decision — both the function's location and `PollMatchResult::Matched`'s own shape have since
moved on; `Matched` is now a 2-tuple, `ADR-148` Amendment 8) currently captures `matches_got_before` from
the live registrant at the start of its own call and diffs against it after `poll_rx_inner`
returns, to report a `PollMatchResult::Matched(count)` for *this pass only*. This has the same
shape of bug one layer up: even with (A) correctly summing cross-channel contributions into
`live.matches_got`, a companion-channel increment landing between two of the primary channel's
own `poll_rx_and_check_match` calls is invisible to this diff — it becomes part of the *next*
call's baseline instead of being reported to `wait_for_expected_response_inner`'s own local
`matches_got` counter (`events.rs:7058`, `7091`), which never observes it and keeps waiting for
a match count the live registrant already reached.

Fix: thread the wait loop's own local, cumulative `matches_got` in as the baseline for this call,
rather than re-reading the registrant fresh each time. `check_match_against_baseline`'s signature
and its own unit tests are unaffected by (C) below.

### C. `pending_rc`: atomic observe-and-consume, replacing the pre-poll reset

An implementation review (edge-case-hunter, then design-advisor) caught that `pending_rc`'s
existing pre-poll reset (`registrant.pending_rc = None`, run BEFORE this call's own
`poll_rx_inner` pass) was left unchanged by the first draft of this ADR — incorrectly, since it
has the identical failure shape as (B) and is not merely "one poll interval late" (an earlier
draft of this Consequences section said so; that was wrong). The companion channel's own writeback
(A) can merge a detected `pending_rc` into live state at any point relative to the primary
channel's calls, including during the wait loop's inter-iteration sleep; the NEXT call's
unconditional pre-poll reset — which runs before it has looked at, let alone reported, the current
live value — silently and permanently destroys that companion-detected occurrence. Nothing ever
reports it: the wait loop's RC78/21/23 deadline-extension/retransmit logic never fires for an RC
the ECU genuinely sent.

Fix: delete the pre-poll reset and its associated early-return critical section entirely. Replace
it with a single post-poll critical section that reads `pending_rc` and resets it to `None` in the
SAME lock acquisition (no separate compare-and-reset needed — the invariant that makes this sound
is that every writer of `pending_rc`, both `bind_registrant`'s intra-pass detection and (A)'s
cross-channel merge, is set-only-if-currently-`None`, so any `Some` observed after this call's own
poll pass is an unreported occurrence regardless of which channel set it or when):

```rust
let mut links = ctx.logical_links.lock().await;
let Some(registrant) = links.get_mut(&target_cll).and_then(|l| {
    l.registrants.iter_mut().find(|r| r.cop_handle == cop_handle)
}) else {
    return PollMatchResult::NoMatch;
};
let result = check_match_against_baseline(
    registrant.matches_got,
    registrant.pending_rc,
    caller_cumulative_matches_got,
);
registrant.pending_rc = None; // consumed, atomically with the read above
result
```

The reset is unconditional, not only-when-reported: when the result is `Matched` (which
outranks `PendingRc` in `check_match_against_baseline`'s priority), a coexisting `pending_rc` is
cleared unreported rather than deferred to the next call. This is deliberate, not an oversight —
leaving it set would report a stale RC one call after the match that superseded it, and for
0x21/0x23 that arm retransmits the original request, so a late report would put a spurious
duplicate request on the wire. This is exact behavior parity with the pre-ADR-101 design (a
Matched-priority RC was always silently wiped at the next call's top there too, single-channel
included) — a pre-existing, now-merely-documented residual, not a new loss; see Consequences.

### D. Reap-vs-companion-writeback: eager bind-confirm write-through for `cyclic_deadline`

Codex review of PR #103, round 4, found that this ADR's own "One-poll-interval-late observation,
accepted" Consequences bullet (below) was wrong in the same way Decision §C's first draft was
wrong about `pending_rc`: it framed the exposure as mere *latency*, when the actual failure mode
is *loss*. `reap_expired_cyclic_registrants` (ADR-100 Decision §4, `events.rs:4049-4101`) is the
sole mechanism that ever finishes a created-receive-only `NumReceiveCycles = -1` registrant with a
nonzero `CP_CyclicRespTimeout`; it reads the LIVE `cyclic_deadline` directly and deletes the
registrant from `link.registrants` when expired. Verified: `reap_expired_cyclic_registrants` and
its sibling `reap_cancelled_detached_registrants` gate on `link.channel_id == Some(ctx.channel_id)`
only, never `link.uudt_channel_id` (contrast `build_cll_rx_entries`, which checks both,
`events.rs:614/657`) — so exactly one poll task (the primary channel's) ever reaps a given CLL's
registrants; the companion channel's own reap calls for that CLL are always a no-op. The race is
therefore only ever primary-reap vs. companion-writeback, not reap vs. reap.

Sequence: the companion channel's poll task drains a matching frame, `bind_registrant` restarts
`cyclic_deadline` on that pass's owned clone only (Decision A's snapshot-clone shape), and the
frame is delivered to the client immediately (`push_rx_frame`/subscription notify,
`events.rs:1595-1640`) — well before that pass's own end-of-batch writeback (Decision A) re-locks
`logical_links` and merges the restart into live state. If the primary channel's
`reap_expired_cyclic_registrants` acquires `logical_links` and reads the still-stale live deadline
in that window, it deletes the registrant and emits `PduCopstFinished` for what the client just
received as an ongoing response — and the companion's later writeback then silently skips the
merge (`merge_registrant_writeback`'s call site looks up the live registrant by `cop_handle` and
finds nothing), so the extension is lost, not merely delayed.

**Fix:** an eager confirm-and-write-through step, inserted in `poll_rx_inner` between `bind_frame`
returning a `FrameBinding::Registrant` outcome and that frame's delivery (`events.rs:1593`/`:1595`
today), scoped to exactly the registrant shape that can have `cyclic_timeout_ms` set at all
(`tier == RegistrantTier::ReceiveOnly && cyclic_timeout_ms.filter(|&ms| ms > 0).is_some()` on the
just-mutated snapshot registrant — every other registrant shape leaves `cyclic_timeout_ms` unset
and is unaffected): re-acquire `logical_links`, find the live registrant by `cop_handle` +
`connect_generation` (the same pair Decision A's writeback loop already keys on), and

- if found: `live.cyclic_deadline = live.cyclic_deadline.max(snap.cyclic_deadline)` — the snapshot
  already carries this frame's fresh restart, so the later end-of-pass writeback's own monotone-max
  merge becomes a harmless no-op;
- if not found (already reaped, or a reconnect changed `connect_generation`): discard this frame —
  `continue` past its delivery, same "unbound → discarded" precedent as ADR-100 Decision §5/§6,
  rather than delivering `ResultData` under a `cop_handle` that no longer exists live.

This linearizes the "extend vs. finish" decision on the `logical_links` lock: whichever of
{primary's reap, companion's confirm} acquires the lock first determines a self-consistent outcome
— reap-first deletes the registrant and the confirm step then discards the frame instead of
delivering it under a dead handle; confirm-first extends the live deadline so the reap's own
`now >= dl` check fails and the COP survives. Either order is now consistent from the client's
point of view; the previous bug delivered the frame AND finished the COP in the same window. No
change to `matches_got` here — that field stays exclusively delta-merged at end-of-pass (Decision
A); eagerly applying it here would double-count against the writeback's own delta.

`matches_got` is intentionally NOT given the same eager-confirm treatment — only `cyclic_deadline`
has a deletion consumer (`reap_expired_cyclic_registrants`) racing its writeback; nothing reads
`matches_got` from live state destructively between passes.

**Residual, honestly bounded (unlike the bullet it corrects, below):** a frame the companion
channel drained *before* binding it — i.e. before `logical_links` is even acquired for the confirm
step — can still lose to a reap that linearizes first, discarded alongside a legitimate
`PduCopstFinished`. This window is one lock acquisition wide, has no oracle (no host-correlated
frame-arrival timestamp from the J2534 API, the same reasoning ADR-100's own
registrant-snapshot-vs-frame-drain residual already establishes,
`docs/adr/ADR-100-cop-registry-two-tier-binding.md`'s Consequences section), and is inherent to any
host-side timeout check racing wire arrival. Scope: only `NumSendCycles == 0`,
`NumReceiveCycles == -1`, nonzero-`CP_CyclicRespTimeout` registrants on a CLL with a UUDT companion
channel — no other registrant shape ever has `cyclic_timeout_ms` set, and a single-channel CLL's
own reap calls are sequential with its own `poll_rx_inner` passes (no companion, no race).

**Correction (edge-case-hunter review of this fix):** an earlier draft of this bullet claimed the
confirm-and-write-through step opens "the identical" window every other registrant-removal path
already had. That overstated it: before this fix, the span between `bind_frame` accepting a match
and that frame's delivery was purely synchronous (zero `.await` points) for every registrant, so
`reap_cancelled_detached_registrants`/`cancel_link_cops`/hard-error teardown could only race
delivery at other, pre-existing yield points, never at that exact span — this fix's own added
`logical_links.lock().await` is a genuinely new yield point there, for the scoped (tier-2,
`cyclic_timeout_ms > 0`) registrant shape only. Closed directly, at no extra cost, since
`confirm_cyclic_deadline_writeback` already holds the same `logical_links` lock it needs to check:
it also rejects (`false`, frame discarded) when `link.cancelled_cops` already contains the
registrant's `cop_handle` — a `CancelComPrimitive` marked but not yet reaped by
`reap_cancelled_detached_registrants` — so a cancel racing into this fix's own new window can no
longer slip through and get its frame delivered/deadline extended moments before removal.

**Flagged, not fixed (accepted as-is, narrower than the above):** `cancel_link_cops` and hard-error
teardown remove the registrant from `registrants` outright rather than merely marking
`cancelled_cops`, so they were already covered by the existing "registrant not found → discard"
branch and needed no new check. What remains genuinely unfixed is the pre-existing, coarser-grained
deliver-after-removal window at every OTHER yield point in a poll pass (frame-batch iteration,
`process_frame_for_entry`'s own hardware I/O, etc.) for every tier-2 registrant shape, not just the
cyclic-timeout-bearing one this Decision scopes to. Closing that fully would mean re-checking
`cancelled_cops`/live existence around every yield point in the frame-processing pipeline for every
tier-2 binding — not warranted for a benign case (the client asked for cancellation, or the link is
gone) versus this ADR's case (a genuine in-time response racing a spurious timeout). A frame the
confirm gate discards is not re-offered to a lower-precedence registrant in the same pass — the
binding decision was already made against that pass's snapshot, consistent with the spec's
first-match-wins rule this ADR's sibling, ADR-100 Decision §3, already establishes.

### E. Per-channel drain watermarks: the cyclic reap must be sound against BOTH channels of a dual-channel CLL

Codex review of PR #103, round 7. Round 6 (Decision §D's neighbor in ADR-095's own Amendment
section) taught `reap_expired_cyclic_registrants` to gate on `rx_drained` — the CALLING poll task's
own physical channel having been EXHAUSTIVELY drained (not merely polled) this pass. That closed the
single-queue backlog hole, but `reap_expired_cyclic_registrants` only ever runs from the PRIMARY
channel's poll task (it filters CLLs by `link.channel_id`, never `link.uudt_channel_id` — the
companion channel's own reap calls are always a no-op for a CLL they don't "own" this way). For a
CLL with a UUDT companion channel (ADR-046), the companion is an entirely independent poll task with
its own adapter queue that `rx_drained` has zero visibility into: the primary's queue can be
genuinely, exhaustively empty while a matching response for the SAME registrant is still sitting
un-polled in the companion's own queue, because the companion poll task simply hasn't reached its
next tick yet, or is itself mid-hold in some other long wait (the same STmin/RC21-23/etc. holds that
motivate this whole mechanism, just on the other channel). The primary's reap, now "correctly" gated
per round 6, still deletes the registrant — "drained" was only ever proven for one of the two
channels serving it.

Decision §D (the eager `confirm_cyclic_deadline_writeback` write-through) does not cover this: it
linearizes extend-vs-finish only once the companion has actually READ AND BOUND a frame — a frame
still sitting un-polled in the companion's adapter queue has never touched `logical_links` at all,
so there is nothing for §D to linearize against. §D's own "registrant not found → discard" branch
firing later, once the companion eventually does poll, is the LOSS event this round's finding
describes ("the companion task then drops the response because the registrant is gone") — §D is
behaving correctly given what it can see; the reap deleted the registrant before §D ever got a
chance to contribute.

**Decision.** Replace the round-5/6 boolean gating (`rx_freshly_drained`/`rx_drained`, threaded
per-call-site) entirely with a **per-channel drain watermark**, stating the actual invariant
directly instead of approximating it site-by-site: a cyclic registrant may be finished only when
EVERY channel that can deliver it a match has completed an exhaustive drain-and-merge pass whose
read began at or after the registrant's `cyclic_deadline`.

- **Stamp.** `poll_rx_inner` captures `read_started_at` immediately before its `PassThruReadMsgs`
  call. When that pass's outcome is exhaustive (buffer-empty, or a batch shorter than
  `MAX_POLL_MESSAGES`), it writes `watermarks[ctx.channel_id] = read_started_at` — at the END of the
  pass, after that pass's own writeback/merge has landed, not immediately after the read. Timestamping
  from read-START is conservative for a frame that arrives mid-read; writing the watermark only after
  the merge closes the window where a watermark could be observed before the bindings it certifies
  have actually reached live state.
- **Store.** A new module-level shared field, `Arc<Mutex<HashMap<ChannelId, tokio::time::Instant>>>`,
  constructed once alongside `logical_links` (`service.rs`/`rpc_primitive.rs`'s module-construction
  site) and `Arc::clone`d into every `ChannelPollCtx` the same way `logical_links` already is (NOT
  the `last_bus_activity`-style pattern of one fresh instance per physical channel — this map must be
  the SAME instance across every channel poll task on a module, so the primary can read the
  companion's own watermark). Read it in the reap BEFORE acquiring `logical_links` — a stale read
  only ever defers reaping, never wrongly permits it, so no new lock-ordering hazard against ADR-080's
  hierarchy.
- **Predicate.** A registrant is reapable iff `is_cyclic_deadline_expired(r, now)` AND
  `watermarks[link.channel_id] >= r.cyclic_deadline` AND (`link.uudt_channel_id` is `None` OR
  `watermarks[link.uudt_channel_id] >= r.cyclic_deadline`). Extracted as a pure function alongside
  `is_cyclic_deadline_expired` for direct unit testing.
- **Simplification dividend.** This subsumes rounds 5 and 6 outright: `run_detached_registrant_
  maintenance`'s `rx_freshly_drained`/`rx_drained` parameter is deleted, every injection site
  (including the STmin loop, the RC21/23 chunked sleep, and `handle_delay`) calls it unconditionally
  again, and `run_due_tick_duties`'s own special-cased gate on its direct reap call is deleted too —
  the reap is now self-gating purely from the watermark map, regardless of which site triggered it or
  whether that site's own loop polls RX at all. A no-drain hold (STmin, RC21/23) simply means that
  channel's watermark predates a mid-hold expiry, so the reap defers there exactly as it did under the
  round-6 boolean scheme — but a registrant whose deadline expired BEFORE such a hold began, with a
  watermark already past it, now reaps promptly and soundly, which the boolean scheme could not
  express. This also retires round 6's own documented micro-granularity residual (a deadline falling
  in the gap between an exhaustive drain and the reap's own check) — the watermark's read-then-merge
  ordering means that gap can no longer exist for a `true`-gated site the way it could before.
- **Companion staleness is bounded, not unbounded.** A companion's own last-recorded watermark can
  only be as stale as whatever no-drain hold the companion poll task is itself inside — the same
  bounded classes (STmin block, RC21/23 completion ceiling, `CoptDelay`) already accepted for the
  primary side. A hard error on the companion clears `uudt_channel_id` and wholesale-cancels the
  CLL's COPs; disconnect/destroy routes through `cancel_link_cops` — either way the reap's own
  `uudt_channel_id` read (freshly taken under the same `logical_links` lock acquisition as the
  registrant scan itself) reflects reality, so a dead or torn-down companion can never permanently
  stall this gate. A stale watermark surviving a channel-ID reuse (a torn-down channel's slot reused
  for a new connection) is always further in the past than any new registrant's own `cyclic_deadline`
  could be, so it can only ever cause an extra defer, never a wrongful permit — the map entry is
  additionally cleared at channel teardown as hygiene, not as a correctness requirement.
- **Refinement considered, not required for correctness:** a registrant whose descriptors resolve to
  `unique_resp_ids` that can never route to the companion's own CAN ID could in principle skip the
  companion conjunct entirely (it can never receive a companion-delivered match). Left as a future
  latency optimization, not implemented now — the unconditional AND-both-channels predicate above is
  already sound; this would only narrow the bounded-deferral residual for a subset of registrants.

**Extends to the tier-1 inline wait (closed in the same commit as this Decision, not deferred as a
residual).** The identical shape exists one tier up: `wait_for_expected_response_inner`'s
finite-count receive-phase loop ends a wait on the primary channel's own deadline check
(`!no_deadline && now >= deadline`) with no companion-freshness check at all — an in-time UUDT
response sitting un-polled in the companion's queue (companion merely one tick behind, or itself
mid-hold) yields a spurious `PduErrEvtRxTimeout`/cycle-complete outcome while the frame is discarded
moments later as unbound (ADR-100 Decision §5) once the companion does eventually poll. Same
watermark, same remedy: before breaking on deadline for a CLL with a UUDT companion, additionally
require `watermarks[uudt_channel_id] >= deadline`; if not yet satisfied, take one more pass instead
of breaking (bounded grace, the same "one grace poll" shape this ADR's sibling ADR-095 already
established for a dispatch that overruns its own deadline). This is a strict widening of an existing,
already-accepted grace mechanism, not a new client-visible timing change in kind — only in the
narrow dual-channel case it now also covers.

**Correction (Codex review of PR #103, round 8): "watermark caught up" alone does not finish the
syllogism.** `watermarks[uudt_channel_id] >= deadline` proves the companion's own writeback merge
has landed in LIVE registrant state (the watermark is stamped only after that merge, per this
Decision's own stamping rule) — but the wait loop's own local `matches_got` counter was sampled
earlier in the SAME iteration, via `poll_rx_and_check_match`'s top-of-loop call, at a point that can
predate the companion's merge. Three separate lock acquisitions (the loop's own top-of-loop sample,
the cancellation/staleness check, and the watermark read) give the companion's merge a window to land
strictly between the first and the third with nothing to stop it. Breaking straight to the timeout
path (which judges completion on that same stale local counter) can therefore report
`PduErrEvtRxTimeout` for a response the companion has ALREADY delivered to the client at bind time
(ADR-101 Decision §D's confirm step already passed, or the frame would have been discarded, not
bound) — worse than a merely wrong status, since the client now sees both the delivered `ResultData`
and a spurious timeout for the same completed exchange.

Fixed by a read-only recheck immediately before the break: re-acquire `logical_links`, look up the
live registrant, and check whether `matches_got` now exceeds the loop's own local baseline, or
`pending_rc` is set (via the existing pure `check_match_against_baseline`, reused rather than
duplicated). If so, do NOT break — set `poll_immediately = true` (skipping the sleep) and let the
loop's own next top-of-loop `poll_rx_and_check_match` pass observe and process the already-merged
delta through its EXISTING match/RC-handling arms, unchanged. This recheck must be a PURE read,
never consuming `pending_rc` itself (unlike `observe_and_consume_pending_rc_outcome`'s own
consume-on-observe contract, Decision §C) — Decision §C's soundness rests on exactly one consumer of
a given `pending_rc` occurrence; consuming it here, at a site that does not itself act on the result
(falls through to the next pass instead of processing inline), would silently drop it. No new
match/RC-handling logic is written at the break site itself — only the decision of whether to still
break.

The recheck runs unconditionally, for both single- and dual-channel CLLs — it is NOT gated on
`uudt_channel_id.is_some()` (an earlier draft of this paragraph claimed such a gate; the shipped
code has no separate branch for the two cases, and none is needed). For a single-channel CLL this is
harmless by construction rather than by an explicit skip: nothing else can have written to
`matches_got`/`pending_rc` between the loop's own top-of-loop sample and this recheck (no second
poll task, and nothing between the two touches RX), so `check_match_against_baseline` always
resolves `NoMatch` there and the loop still breaks on the very first check — functionally identical
to an explicit `uudt_channel_id.is_some()` gate, just without the extra branch.

**No livelock:** each deferral requires either a strictly larger live `matches_got` — capped at
`matches_needed` by `bind_registrant`'s own acceptance gate (`r.matches_got < needed`), so a
finite-count wait cannot defer past its own completion — or a `pending_rc` occurrence, which is
set-once and consumed by the very next pass. For IS-MULTIPLE (`matches_needed = None`), each
deferral still requires one genuinely new, already-delivered frame; the deadline itself stays capped
at `match_reset_ceiling` regardless of how many times it defers, so sustained deferral requires (and
correctly reflects) a sustained stream of genuine matches, not an unbounded loop.

**Residual, honestly bounded (same class as Decision §D's own):** a companion merge landing AFTER
this recheck but before the timeout status is actually sent is still lost to the timeout verdict —
one lock acquisition wide, no oracle to close it, the same "drained but not yet observed" shape
Decision §D already documents and accepts. Chasing this further would mean re-checking live state
after every subsequent `.await` in the timeout path indefinitely; this recheck is the principled
stopping line, not an oversight.

**Test-coverage residual (edge-case-hunter review of this correction):** the recheck's OWN call-site
wiring — argument order into `check_match_against_baseline`, which registrant field feeds which
parameter, and that the closure captures the loop's own local `matches_got` rather than something
else — has no regression test that would catch a mistake there independently of
`check_match_against_baseline`'s own pre-existing unit tests (which cover the pure function's three
input shapes, not this call site's wiring). The integration test added alongside this fix
(`tier1_wait_recheck_survives_many_deadline_vs_companion_watermark_races_in_one_wait`) does not
distinguish fixed-vs-reverted for this specific defect — reverting to the old unconditional `break`
still passes it, since this crate's `current_thread` `#[tokio::test]` runtime with real timers
cannot land the target race (an uncontended `logical_links` lock acquisition with no genuine yield
point) by chance. Same structurally-infeasible-without-a-dedicated-pause/gate-hook class already
documented for comparable narrow windows in `j2534-0404-service/docs/implementation-notes.md`'s
round-4/5/6/8 ADR-086 notes; verified correct by manual code trace instead (argument order, field
selection, and closure capture all confirmed against the actual diff).

**Rejected: cross-task polling of the companion's own adapter queue from the primary task.** The
shared `api` mutex makes the underlying FFI call itself safe to invoke from either task, but doing so
would make the primary a second concurrent reader/processor of a queue whose entire pipeline
(snapshot→bind→merge ordering, software-ISO-TP reassembly state, hard-error attribution keyed to
`ctx.channel_id`) assumes exactly one poll task per physical channel — this ADR's whole existence is
because even two-tasks-one-CLL was hazardous; two-tasks-one-QUEUE is a strictly worse version of the
same problem. A signal-the-companion-and-wait-for-its-report variant degenerates into the watermark
scheme anyway (the posted report IS the watermark, just synchronous instead of async), with none of
its benefit and a new blocking dependency between two poll tasks that otherwise never wait on each
other. **Rejected: removing the independent reap sweep entirely, relying only on Decision §D's
confirm-step to detect cyclic timeout.** A registrant that never receives ANY frame (the actual
timeout case NOTE 1 describes) has nothing to trigger §D's confirm step at all — timeout detection
fundamentally needs an active sweep, not a passive bind-time check.

**Consequences.** The round-6 "sustained full-batch backlog" residual survives, now per-channel: EITHER
channel's watermark can stall indefinitely while every read on that channel returns a full
`MAX_POLL_MESSAGES` batch, converging at that channel's own first non-full read. The bounded-deferral
residual class gains the companion's own hold durations as an additional (already-bounded, already
accepted-elsewhere) source of latency. Decision §D's own one-lock-wide "drained but not yet bound"
residual is unchanged and continues to be documented where it already is, above.

## Alternatives Considered

**Serialize the two channels' registrant bookkeeping** (a per-CLL lock held across
snapshot→process→writeback) — rejected. `process_frame_for_entry`'s own frame processing performs
real hardware I/O (FC writes, `events.rs:839+`) under the separate `api` lock; holding a per-CLL
lock across that would create a new lock spanning `logical_links` against `api`-touching I/O, on
top of ADR-080's already-audited three-mutex hierarchy (`shared_channels` outermost), plus a
lock-ordering hazard whenever two channel tasks hold overlapping multi-CLL entry sets at once.
Binding directly against live registrants per frame instead of a snapshot clone was also
considered — viable in principle, but strictly larger in scope (touches `bind_frame`/
`bind_registrant` and roughly 15 existing pure unit tests built around the clone-and-merge shape)
and, decisively, does not fix (B) on its own: the wait loop's local match counter would still
need to learn about a companion-channel increment somehow. Since (B) is required regardless of
how (A) is solved, the delta-merge approach is the smaller, complete fix.

**Clamp `matches_got` to `matches_needed`** at merge time, to suppress the over-acceptance
residual below — rejected as dishonest bookkeeping; see Consequences.

## Consequences

**Snapshot-visibility principle (Codex review of PR #103, round 10; stated once here rather than
re-derived per field):** a field mutated on a per-pass registrant clone is invisible to a sibling
channel's already-cloned, in-flight pass until that sibling's own NEXT clone. Merge semantics
(delta for counters, set-once for `pending_rc`, monotone max for `cyclic_deadline`, one-way latch
for `tier`) guarantee *convergence* of live state; none of them can retroact on a binding or
delivery decision a sibling pass already made against its own now-stale clone. Eager write-through
(Decision §D) is warranted only when the racing consumer is DESTRUCTIVE — a reap that deletes the
registrant, permanently losing an in-time response, as `cyclic_deadline`'s race did. For a
NON-DESTRUCTIVE consumer — a precedence/attribution rank, as `tier`'s race is — one overlapping
pass of skew is the accepted cost of not serializing snapshot→process→writeback across real
hardware I/O (Alternatives Considered, above). Every mutable `CopRegistrant` field is now
accounted for under this principle: `matches_got` (over-acceptance, bounded residual below),
`pending_rc` (Decision §C, atomic observe-and-consume), `cyclic_deadline` (Decision §D, eager-closed
because its consumer is destructive), and `tier` (Decision §A, residual above, non-destructive
consumer). Every OTHER `CopRegistrant` field (`expected`, `rc_cfg`, `request_sid`, `matches_needed`,
`registration_seq`, `connect_generation`, `cyclic_timeout_ms`, `migrate_on_first_match`) is
immutable post-registration and so has no snapshot-visibility question to answer at all. Any future
mutable `CopRegistrant` field must declare, at introduction: its merge rule, its skew consumer, and
whether that consumer is destructive — that classification is what decides whether it needs
Decision §D's eager treatment or Decision §A's plain convergent merge.

- Companion-channel responses can now correctly complete a finite-count (`NumReceiveCycles > 0`)
  or IS-MULTIPLE (`-2`) `CoptSendrecv`/`CoptStopcomm` COP instead of silently losing the
  contribution and timing out.
- **Accepted residual: bounded cross-channel over-acceptance.** Each channel's pass still gates
  its own `matches_needed` check against its own snapshot clone's `matches_got`
  (`bind_registrant`, unaffected by this ADR), so a registrant with `matches_needed = Some(2)`
  whose snapshot on BOTH channels happened to be taken at `matches_got = 0` can accept up to 2
  matches on EACH channel within one overlapping poll-batch window, merging to a live count of 4,
  with all four frames genuinely delivered under the COP's handle before either merge runs (no
  merge can retract an already-delivered frame). This requires more genuine, distinct matching
  responses than requested arriving within a single overlapping poll-batch window on top of each
  other — an ECU answering once per physical wire per request cannot trigger it; Codex's reported
  1+1 split for `NumReceiveCycles = 2` is handled exactly by this fix. `matches_got` is
  deliberately left truthful (not clamped to `matches_needed`) rather than silently discarding a
  real, delivered match count — the wait loop's own `>= needed` completion check
  (`wait_for_expected_response_inner`) behaves identically whether `got` equals or exceeds
  `needed`.
- **Correction (Decision §D):** this bullet previously read "one-poll-interval-late observation,
  accepted" for a companion-channel-detected `cyclic_deadline` restart, framing it as mere latency
  before the writeback lands. That was wrong in the same way an earlier draft of this bullet was
  wrong about `pending_rc` (see the parenthetical this replaces): `reap_expired_cyclic_registrants`
  does not just observe a stale deadline late, it can DELETE the registrant on the strength of that
  stale read before the writeback lands — a permanent loss (of the extension, and of the
  registrant itself), not latency. Closed by Decision §D's eager confirm-and-write-through, with
  its own narrower, honestly-bounded residual documented there.
- **Accepted residuals from Decision §C's `pending_rc` consume:** (a) a coexisting unreported RC is
  discarded, unconditionally, whenever the same observation reports `Matched` — pre-existing
  behavior (the pre-ADR-101 design did this too, single-channel), now also applying to a
  companion-detected RC; narrow in practice, since a deadline restart from the match makes the
  discarded RC largely moot. (b) `bind_registrant`'s intra-pass first-code-wins rule and (A)'s
  merge gate both mean a second, distinct pending-RC occurrence arriving while an earlier one is
  still unconsumed is dropped in favor of the first — unchanged from pre-ADR-101 behavior. (c)
  RC78/21/23 ceiling anchoring (ADR-057, `rc78_ceiling.get_or_insert_with`-style, `events.rs:7249`
  and `7263-7275`) is keyed on wall-clock time of first REPORT, not wire detection — a
  companion-detected RC anchors up to one wait-loop iteration later than it was actually seen on
  the wire, tens of ms against multi-second `CP_RC{78,21,23}CompletionTimeout` defaults, always in
  the permissive (later) direction.
- `CllRxEntry` gains a `registrant_baselines` field; `build_cll_rx_entries`'s registrant-snapshot
  site captures it under the same lock the clone itself already uses — no new lock acquisition.
- Follow-up work: regression tests for the merge formulas (two-pass `matches_got` summation;
  `pending_rc` baseline-gated None→Some vs. stale-snapshot-does-not-resurrect; `cyclic_deadline`
  monotone max) as pure unit tests against the extracted merge logic; a
  `poll_rx_and_check_match`/wait-loop-level test proving a companion-channel increment between two
  primary-channel calls is not lost; and pure unit tests for Decision §C's observe-and-consume
  (extracted as a small sync helper for testability, per the same constraint that motivated
  `check_match_against_baseline`'s own extraction): a pre-set companion-merged `Some` with no match
  delta reports `PendingRc` and leaves the field consumed; a match delta with a coexisting `Some`
  reports `Matched` and still clears the field (residual (a) above); no delta and no `pending_rc`
  reports `NoMatch`; and two successive calls with the field re-set between them each independently
  report (the chattering-ECU case).

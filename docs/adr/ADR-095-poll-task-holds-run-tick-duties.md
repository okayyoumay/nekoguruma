# ADR-095: Every Poll-Task Hold Runs Tick Duties, or Documents Why Not

**Date:** 2026-07-16
**Status:** Accepted (amended by ADR-100 and by this file's own Amendment below)
**Affects:** `j2534-0404-service` events (`isotp_send`, `wait_for_expected_response`,
`poll_channel_events`, `drain_tx_held_backlog`, `dispatch_due_tester_present`) — extends ADR-083's
tester-present dispatch hooks and generalizes ADR-094's tick mechanism

## Context

ADR-094 fixed one instance of a bug class: a long-running hold inside the poll task that doesn't
give `dispatch_due_tester_present` (both `CP_TesterPresentSendType` modes as of ADR-093) a chance to
run, so a mode-0 tester-present keep-alive — now software-dispatched, previously hardware-autonomous
and immune to this — can silently stop firing for the hold's entire duration.

A fourth Codex review finding on this same PR (#97) reported a second instance: `isotp_send`'s
software ISO-TP FlowControl-wait (N_Bs) and inter-frame STmin pacing loops only call `poll_rx_inner`
per iteration, never `dispatch_due_tester_present`, unlike `handle_delay`/`wait_for_expected_response`
(ADR-083) or the outer poll loop (ADR-094). A software-ISO-TP `CoptSendrecv` that waits for
FlowControl or paces many consecutive frames for longer than `CP_TesterPresentTime` suppresses mode-0
keep-alives until the transfer returns.

Per this repo's own established convention (`.claude/skills/codex-pr-review-loop/reference/
finding-routing.md`: "when the same CLASS of finding lands in a second consecutive round, stop
routing per-instance and commission a full-surface enumeration audit instead"), this ADR is that
audit — enumerating every poll-task hold in `events.rs`, not just fixing the one Codex reported.

## Decision

### Full enumeration

| # | Hold site | Verdict |
|---|---|---|
| 1 | Outer poll loop tick (ADR-094's `next_tick`/`run_due_tick_duties`) | Pass |
| 2 | `handle_delay` (`CoptDelay`) | Pass (ADR-083) |
| 3 | `wait_for_expected_response`'s main receive-phase loop, incl. unbounded IS-CYCLIC | Pass (ADR-083) |
| 4 | `isotp_send`'s FlowControl-wait (N_Bs) loop | **Fixed** (the reported finding) |
| 5 | `isotp_send`'s STmin/ConsecutiveFrame-block pacing loop | **Fixed** (same class, found by the audit) |
| 6 | `wait_for_expected_response`'s RC21/RC23 `request_time_ms` chunked retry sleep | **Fixed** (found by the audit) |
| 7 | The parked-cyclic-follow-up drain loop and `drain_tx_held_backlog` (both feed `dispatch_tx_item` calls back-to-back inside one outer-loop iteration, ahead of ADR-094's own post-`select!` check) | **Fixed** (found by the audit — this compounds ADR-094's exact bug through a path its fix didn't cover) |
| 8 | `wait_for_p3_gap`'s own wait loop | Deliberately excluded, kept (see below) |
| 9 | `run_protocol_init` (K-line 5-baud/fast-init) | Accepted residual, sharpened by a same-PR follow-up (see below) |
| 10 | Software ISO-TP RX reassembly | Pass — a per-frame state machine fed by each poll batch; never blocks across ticks, nothing to fix |

### Fixes

**`isotp_send`'s two waits (#4, #5).** Both insert `Box::pin(dispatch_due_tester_present(true,
Some(cll_handle), ctx)).await` per iteration. Boxing is required, not stylistic: without it,
`isotp_send → dispatch_due_tester_present → send_tester_present_once → transmit_request →
transmit_request_inner → isotp_send` is a closed `async fn` recursion cycle Rust rejects outright
(an infinitely-sized future) unless one edge is boxed. Runtime recursion depth stays bounded at
exactly one level: `send_tester_present_once` always passes `isotp_tx: None` to whatever it calls,
so a tester-present send triggered from inside `isotp_send` can never itself recurse back into a
nested `isotp_send` call — load-bearing, and commented at the call site. `defer_gap_wait = true`
(matching `wait_for_expected_response`'s own existing call) avoids `dispatch_due_tester_present`'s
own `wait_for_p3_gap` entering an *unprobed* `poll_rx` mid-wait — an FC frame arriving during that
unprobed poll would be silently consumed as unattributed background RX and lost, causing a spurious
N_Bs timeout on the very transfer this dispatch call runs inside of.

**`exclude_cll: Option<u32>` on `dispatch_due_tester_present`.** A same-CAN-ID single-frame
tester-present spliced into the middle of a CLL's own FirstFrame/ConsecutiveFrame sequence looks
like an unexpected SF to the receiving ECU mid-transfer (ISO 15765-2 unexpected-SF handling),
terminating its in-progress reception and corrupting the very transfer `isotp_send` is running.
Mode 1 already avoids this incidentally via its own `last_bus_activity`-driven self-exclusion from
firing during recent activity; mode 0 has no such protection, so `isotp_send`'s two call sites pass
`Some(cll_handle)` to explicitly exclude the in-flight CLL's own tester-present for the duration of
its own transfer. Every pre-existing call site (`handle_delay`, `wait_for_expected_response`'s
top-level call, the new `run_due_tick_duties` helper below) passes `None` — unaffected.

**RC21/RC23 retry wait (#6).** `wait_for_expected_response`'s chunked `request_time_ms` sleep
(already chunked to drain `cancelled_cops` per chunk, PR #92) gains an unboxed
`dispatch_due_tester_present(true, None, ctx).await` per chunk, alongside the existing cancellation
drain — no recursion risk here (this loop is not in `dispatch_due_tester_present`'s own call graph).

**Parked-drain and `tx_held` drain (#7).** ADR-094's post-`select!` tick-deadline block is extracted
into a shared `run_due_tick_duties(next_tick: &mut Instant, interval: Duration, ctx: &ChannelPollCtx)
-> bool` helper (`false` signals a hard RX error, mirroring the existing break). Called from three
places now instead of one: the original outer-loop post-`select!` site, once per item inside the
parked-cyclic-follow-up drain loop (bailing the *entire* outer loop, not just the inner drain, on
`false`), and once per item inside `drain_tx_held_backlog` (which gains a `next_tick: &mut Instant`
parameter and a `bool` return, propagated to its own callers). Without this, a suspended CLL's queued
backlog draining in a tight loop reproduces ADR-094's exact starvation through a path its fix didn't
touch — N items dispatched back-to-back inside one iteration, with the tick check only reachable
after the whole drain completes.

### `wait_for_p3_gap` — exclusion kept, rationale sharpened (#8)

The prior comment ("bounded by a P3-family gap, necessarily much shorter than a keepalive interval")
overstated the case — `CP_P3Func`/`CP_P3Phys` are client-configurable to arbitrary magnitudes, not
inherently short. The decisive facts that still justify the exclusion, regardless of configured
magnitude: (1) the wait has a fixed, one-shot deadline anchored at the last TX instant — it never
self-renews, so the worst-case tester-present delay from this exclusion is bounded to exactly one
configured gap, once, not unbounded; (2) for a tester-present send in the *same* addressing bucket as
the gap-waiting send, dispatching from inside would gain nothing — the tester-present send would
immediately re-enter the same gap wait itself; (3) adding dispatch here would create a
`dispatch_due_tester_present → wait_for_p3_gap → dispatch_due_tester_present` re-entrancy needing its
own guard, not just boxing, for no proportionate benefit given (1) and (2). Comment rewritten to
state this explicitly; no functional change.

### `run_protocol_init` — accepted residual (#9), sharpened after a same-PR Codex round

Originally recorded here as: "a small number of synchronous FFI calls under the shared `api` lock,
bounded by SAE J2534-1's own init timing (on the order of a few seconds worst case for 5-baud
init) — not a loop, and not chunkable without restructuring the native init sequence itself." This
framing only considered the initializing CLL's own arming being delayed by its own init. A follow-up
Codex round on this same PR caught the real, uncounted cost: the poll task is per-*physical-channel*,
so one CLL's `run_protocol_init` blocks `dispatch_due_tester_present` for every CLL sharing that
channel — including an already-armed, unrelated **sibling** CLL's mode-0 keepalive, which has nothing
to do with the CLL currently initializing. Worst case without mitigation: the sibling's ECU-visible
inter-keepalive gap is `CP_TesterPresentTime + init_duration`, not just `init_duration`.

Confirmed (design-advisor, re-verdict on this exact scenario) that neither chunking `run_protocol_init`
itself nor moving tick duties to a separate tokio task closes this: the init is genuinely one opaque
blocking `PassThruIoctl` call with no internal yield points, executed under the same device-global
`api` mutex a tester-present send itself needs — a separate task would simply park on that mutex for
the init's full duration with no benefit, and the K-line physically cannot carry a sibling's frame
during the init handshake regardless of dispatch architecture.

**Fix: a pre-init tester-present top-up.** Immediately before `run_protocol_init` runs,
`handle_start_comm` force-fires every currently-`Armed`, **mode-0-only** sibling CLL's tester-present
once (`dispatch_due_tester_present`'s due-check interval predicate bypassed for `send_type == 0`
only, `exclude_cll` set to the initializing CLL, every other guard — channel match, `comm_started`,
`Armed` state — left fully intact). This is the cheap part that WAS avoidable: it converts the
worst-case sibling gap from `CP_TesterPresentTime + init_duration` down to
`max(CP_TesterPresentTime, init_duration)` — the theoretical best any dispatch scheme, including the
old hardware-autonomous one, could achieve — since the forced send resets that sibling's own
due-clock right before the wire goes dark for init.

**Mode 1 is deliberately NOT force-fired.** It's already correctly handled: `run_protocol_init`'s own
wire traffic stamps `last_bus_activity`, and a mode-1 sibling being deferred by genuine external bus
activity is intentional per ADR-083 — forcing it here would fight that existing, correct semantics
for no benefit.

**Residual bound remaining after this fix (accepted, physical floor, not further closable):** during
a sibling CLL's K-line init, mode-0 tester-present on the shared channel still pauses for exactly the
init's own bus-dark duration (fast init ≲ 0.5s; 5-baud roughly 2–5s per ISO 9141-2's W1–W4 timing) —
this part is a genuine physical/API floor (single wire, device-global `api` mutex held across one
blocking ioctl), not a scheduling artifact this service controls. With the top-up, the ECU-visible
gap is bounded to `max(CP_TesterPresentTime, init_duration)` plus at most one poll tick, self-healing
at the first post-init due-check. If a sibling ECU's own session timeout is shorter than the init's
bus-dark duration, that session can drop under *any* dispatch scheme, hardware periodics included —
an inherent hazard of running a slow K-line init on an already-in-session shared channel, not a
defect in this service's dispatch mechanism.

## Consequences

- Mode-0 tester-present now survives every long-running hold in the poll task this audit could find,
  closing the class ADR-094 only partially closed.
- `isotp_send`'s own CLL is excluded from firing its own tester-present mid-transfer — a new,
  necessary carve-out (`exclude_cll`) that did not exist before this ADR, since no prior call site
  needed to protect a specific CLL's own in-flight transfer from its own keep-alive.
- `dispatch_due_tester_present`'s signature changed (`exclude_cll: Option<u32>` added) — every call
  site updated; no observable behavior change for existing (non-`isotp_send`) call sites, since they
  all pass `None`.
- `drain_tx_held_backlog` gained a `next_tick: &mut Instant` parameter and `bool` return — both
  existing call sites updated to bail on `false`, matching the outer loop's own existing hard-error
  handling.
- Two accepted, bounded residuals remain by design, not oversight: `wait_for_p3_gap`'s one-shot,
  self-limiting exclusion (#8), and `run_protocol_init`'s short, unchunkable, spec-timed handshake
  (#9) — both documented here rather than left implicit.
- New regression tests (`tester_present_send_type.rs`): FC-wait dispatch for a sibling CLL, own-CLL
  exclusion during an in-flight transfer, STmin-loop interleaving, `tx_held`-drain non-starvation, and
  an RC21-retry-wait dispatch test. Each of the audit's three previously-unreported gaps (isotp_send
  STmin, tx_held drain, and the exclusion mechanism itself) has an independent fail-without/pass-with
  proof, not a single combined revert.
- ADR-083's tester-present dispatch hook inventory and ADR-094's tick mechanism are both extended by
  this ADR, not superseded — the underlying principle ("every long-running per-tick poll loop must
  give tester-present dispatch a chance to run, or explicitly document why not") is unchanged; this
  ADR only completes its enumeration and generalizes ADR-094's tick-deadline block into a shared
  helper reusable from more call sites.

## Amendment — 2026-07-16: Deadline Check Moved Before Dispatch (PR #97 8th Codex Review Round)

**Context.** The fixes above give `isotp_send`'s two waits (#4, #5) and `wait_for_expected_response`
(#3) a `dispatch_due_tester_present` call per iteration, but did not account for the fact that
`dispatch_due_tester_present`'s send (`send_tester_present_once`) is real hardware write I/O and so
consumes real wall-clock time. In any loop that both polls for a specific expected frame (an FC in
`isotp_send`'s N_Bs wait; an expected-response frame in `wait_for_expected_response`) and dispatches
due tester-present for a sibling CLL, the pre-amendment ordering — poll → capture-check →
cancellation-check → dispatch → deadline-check → sleep — had two defects: (i) a dispatch could run on
an iteration whose deadline had already passed, sending needless tester-present traffic on a wait that
was about to time out regardless; (ii) an expected frame arriving on the wire *during* the dispatch's
write could sit unread in the adapter's buffer while the loop broke out on timeout on that same pass,
without the loop ever polling again to see it — a spurious timeout despite the real response having
already arrived. An 8th Codex review round on PR #97 caught this in `isotp_send`'s FC-wait loop;
investigating for the same shape elsewhere found an identical, pre-existing instance in
`wait_for_expected_response`'s own top-level dispatch call, present since the original ADR-083 hook and
not introduced by this PR's `isotp_send` work.

**Decision.** In any loop that both polls for a specific expected frame and dispatches tester-present,
the order is now poll → capture-check → deadline-break → dispatch → deadline-saturating sleep: the
deadline check moves to immediately before the dispatch call, so no dispatch ever runs on an
already-expired wait, and the post-dispatch sleep uses `deadline.saturating_duration_since(now)` (not
`deadline - now`, which panics/underflows once an overrunning dispatch pushes `now` past `deadline`)
so it never blocks the very next poll pass. Both sites were fixed identically: `isotp_send`'s FC-wait
(N_Bs) loop, and `wait_for_expected_response`'s own top-level dispatch call (its `no_deadline`/
`poll_immediately` IS-CYCLIC and IS-MULTIPLE branches are unaffected — the deadline check is simply
skipped for `no_deadline`, which has no window to expire). `isotp_send`'s STmin/ConsecutiveFrame-block
pacing loop (#5) needed no change: no RX poll or deadline decision follows its dispatch call in the
same loop iteration, so it cannot exhibit this hazard shape. The RC21/RC23 retry wait (#6),
`handle_start_comm`'s pre-init top-up, and `run_due_tick_duties` were likewise reviewed and confirmed
not to have this hazard shape.

**Consequences.** Because each loop's own capture-check runs at the top of the loop on every
iteration, this reordering yields one grace poll after a dispatch that overruns the deadline: a
dispatch's write can still push wall-clock time past the nominal deadline, but the very next
`poll_rx_inner`/`poll_rx_and_check_match` pass — which now always runs before the (moved-earlier)
deadline check can end the wait — gets one more chance to capture whatever arrived during that write.
This is a deliberate, bounded leniency (one poll interval, not unbounded), not a relaxation of the
N_Bs/response-wait timeout itself. A regression test
(`tester_present_send_type::isotp_fc_wait_catches_fc_landing_during_a_dispatched_tester_present_write`)
uses a new mock backdoor (`__mock_arm_write_rx_injection`) to land an FC exactly during a dispatched
tester-present write, with the write's simulated hold deliberately longer than the remaining N_Bs
budget, and confirms the fix's grace poll still catches it (fail-without/pass-with verified by
temporarily reverting the reordering).

## Amendment — 2026-07-18: Detached-Registrant Reap Duties Need the Same Injection (ADR-100/101 follow-up)

**Revision note.** This section's own mechanism went through several corrections within the same
PR (#103, review rounds 5–7) before any of it reached `main` — none of it was merged, relied-upon
history at any point along the way. Rather than stack a growing chain of same-day amendments
describing a mechanism that only ever existed in this PR's own working tree, this section presents
the FINAL injection-site enumeration directly. The cyclic reap's actual SOUNDNESS condition (not
just where it's invoked from) turned out to depend on cross-channel state this ADR does not own —
that half of the mechanism now lives in ADR-101 Decision §E, referenced below rather than
duplicated here. This does not set a new precedent for this ADR's own append-only discipline: the
base decision above and any amendment that has already reached `main` remain append-only, corrected
only by a fresh dated section the normal way.

**Context.** ADR-100 (S5/S6) added `reap_cancelled_detached_registrants` and
`reap_expired_cyclic_registrants` — per-tick sweeps that notify `PduCopstCancelled`/
`PduCopstFinished` for a detached tier-2 registrant nothing else is actively polling. Both were
wired only into `run_due_tick_duties` (this ADR's own tick-duty mechanism), not into the other
poll-task holds this ADR's Full Enumeration already identified. A Codex review of PR #103 caught
the resulting starvation: if the channel's poll task later enters another long hold — a second
`wait_for_expected_response` receive phase, the RC21/23 retry sleep, `isotp_send`'s FC-wait/STmin
loops — a cancelled or cyclic-timed-out detached registrant's notification is delayed for that
hold's entire duration, unboundedly if the hold is itself another IS-CYCLIC COP's own unbounded
pre-first-match wait. This is the identical starvation shape this ADR's Full Enumeration was
written to close for `dispatch_due_tester_present` — just not extended to a mechanism ADR-100 added
four months later.

**Decision.** Extract a small helper, `run_detached_registrant_maintenance(ctx)`, calling both reap
functions, and inject it alongside every non-tick-duty `dispatch_due_tester_present` hook this
ADR's Full Enumeration already established:

- `isotp_send`'s FC-wait loop.
- `isotp_send`'s STmin (ConsecutiveFrame pacing) loop.
- `wait_for_expected_response_inner`'s RC21/23 chunked retry sleep.
- `wait_for_expected_response_inner`'s own receive-phase loop bottom.
- `handle_delay` — a fifth site an earlier draft's four-site enumeration missed entirely.
  `CoptDelay` polls RX every tick and dispatches tester-present, but had no maintenance call of any
  kind, starving both reap duties for its whole configured duration.

Unlike `dispatch_due_tester_present`, neither reap duty needs boxing or a recursion-cycle guard:
neither function is anywhere on `dispatch_due_tester_present`'s or `wait_for_expected_response`'s
own call graph.

**Correction (Codex review of PR #103, round 9): a reap injected inside a hold can never reap the
registrant servicing that same hold — enforced directly, not assumed from tier.** This section
originally argued that fact followed for free from "both reap predicates require
`RegistrantTier::ReceiveOnly`, and a COP inline-waiting inside one of these holds is tier-1 for
the whole of that wait" (ADR-100 Decision §2/§4). ADR-100's own round-9 Finding-1 correction
falsified that premise: a created-receive-only COP with a finite `NumReceiveCycles`/`-2` is, once
correctly classified, tier-2 from the START of its wait — yet, per ADR-100 Decision §4/Out-of-scope's
Stage 3 boundary, it still executes inline/blocking in `wait_for_expected_response_inner`'s own
loop, on the same poll task, for the whole of that wait. So a registrant CAN now be both tier-2
and the COP a hold's own reap-injection sites are running from — exactly the case this section's
original argument had implicitly ruled out. A `CancelComPrimitive` landing between that wait
loop's own top-of-loop cancellation drain and a later injected `run_detached_registrant_
maintenance` call in the SAME iteration would previously have been caught, first-wins, by the
loop's own check; post-round-9-Finding-1 it can instead be caught by
`reap_cancelled_detached_registrants`, removing the registrant and `primitives` entry and emitting
`PduCopstCancelled` out from under the still-running loop — which then runs to its own deadline
and emits a SPURIOUS `PduErrEvtRxTimeout` afterward, an observable protocol-level contradiction
(the client already has `PduCopstCancelled` for this COP), not merely wasted poll-task cycles.

Fixed by enforcing the invariant directly rather than relying on tier alone: both reap functions
read `ctx.executing_cop` (`ChannelPollCtx`'s existing slot for "the COP this poll task is
currently dispatching," already set immediately before dispatch and cleared after the handler
returns — covering the entire span of any inline wait) once at entry and skip any registrant
whose `cop_handle` matches it, in addition to their existing tier filter. This is sound regardless
of tier: `executing_cop` is set by the SAME poll task whose `ctx` a reap call receives (a
companion channel's own reap calls are already no-ops for a CLL they don't "own," per the
`link.channel_id` filter both reaps already apply — see ADR-101 Decision §E), so reading it inside
a reap is exactly "is this the COP this exact call stack is executing." A cancel landing in the
gap is now skipped by the reap and consumed by the wait loop's own NEXT top-of-loop check instead
— `Terminal` + `PduCopstCancelled` from the loop itself, as originally designed; the registrant
becomes reapable again only once `executing_cop` clears (dispatch returns, or the COP detaches to
tier-2 via `DetachedToTier2` and stops being "currently executing" at all).

Applied to BOTH reap predicates, not only the one shown to be reachable today
(`reap_cancelled_detached_registrants`; `reap_expired_cyclic_registrants` cannot currently collide
since `cyclic_deadline` is never set for an inline-waited finite-`N`/`-2` created-receive-only COP,
per ADR-100 Decision §4's `-1`-only scope) — the uniform "never reap the currently-executing COP"
invariant is one extra comparison per reap and won't silently rot if that `-1`-only scope is ever
deliberately widened (ADR-100 Decision §4's own "Resolved (scope, per (e))" already flags this as
a possible, separately-gated future extension).

**Test-coverage residual:** the `executing_cop` exclusion in `reap_cancelled_detached_registrants`
has no deterministic fail-without/pass-with proof — the regression test constructed for it
(`cop_ctrl_cycles::receive_only_finite_n_cancel_during_inline_wait_yields_single_cancelled_status`)
is a best-effort/end-to-end regression rather than one that isolates this specific race, because
the micro-window the fix closes (between the wait loop's own top-of-loop cancellation drain and a
later injected `run_detached_registrant_maintenance` call in the same iteration) has no genuine
executor-yield point to preempt under this crate's single-threaded `current_thread` test harness —
the same structurally-infeasible-without-a-dedicated-pause/gate-hook class already documented
across several rounds in `j2534-0404-service/docs/implementation-notes.md` (round-4/5/6/8 ADR-086
notes, and round 8's own analogous residual on ADR-101 Decision §E's tier-1 recheck). Correctness
instead rests on the code-level argument above; the test protects the end-to-end observable
invariant (exactly one `PduCopstCancelled`, never followed by a spurious `PduErrEvtRxTimeout`) and
was empirically confirmed to still pass without the fix across repeated runs, consistent with the
window being unreachable by chance under this runtime rather than the test being wrong.

The reap duties are NOT folded into `dispatch_due_tester_present` itself — that function's
signature already carries tester-present-specific filters (`exclude_cll`, `exclude_isotp_target`)
that have no reap analog, and blurring the two responsibilities would obscure both. Intra-hook
ordering at each injection site keeps `run_due_tick_duties`'s own existing order (reap before
tester-present dispatch). `wait_for_p3_gap`'s own wait loop (row 8) and `run_protocol_init` (row 9)
remain excluded from this addition for the same reasons this ADR already gives for excluding them
from `dispatch_due_tester_present`.

`reap_cancelled_detached_registrants` needs no further gating beyond simply being called from these
sites — a client-initiated cancellation's semantics never depend on queued RX. `reap_expired_
cyclic_registrants` is a different story: two earlier drafts of this section tried to express its
correctness condition as a per-call-site boolean (first "no gating needed at all," then "gate on
whether a same-iteration poll ran," then "gate on whether that poll was exhaustive") and each was
found insufficient by a subsequent Codex round — the boolean approximation kept being too coarse
for what turned out to be a genuinely cross-channel, timestamp-relative invariant. **That invariant,
and the mechanism that implements it (a per-channel drain watermark, replacing every `rx_freshly_
drained`/`rx_drained` parameter this section previously threaded through each call site), now lives
in [ADR-101 Decision §E](./ADR-101-cross-channel-registrant-writeback.md#e-per-channel-drain-watermarks-the-cyclic-reap-must-be-sound-against-both-channels-of-a-dual-channel-cll)
— ADR-101 already owns cross-poll-task registrant bookkeeping (Decisions A–D), and this invariant
turned out to be exactly that, not a property of WHERE the reap is invoked from.** Every site listed
above calls `run_detached_registrant_maintenance(ctx)` unconditionally; the function itself, and
`run_due_tick_duties`'s own direct call to `reap_expired_cyclic_registrants`, resolve their own
soundness by consulting the watermark map ADR-101 §E describes, not by anything this ADR's
injection sites need to compute or pass in.

One interaction with ADR-101 worth noting, already handled without further change: a reap firing
mid-pass of another COP's hold removes the live registrant between that pass's own snapshot and its
writeback merge — `merge_registrant_writeback`'s existing "registrant removed mid-pass, nothing to
merge" guard (ADR-101 Decision §A) already covers this cleanly.

**Consequences.** `reap_cancelled_detached_registrants`'s and `reap_expired_cyclic_registrants`'s
own doc comments previously claimed detachment-adjacent promptness ("within about one
`POLL_INTERVAL_MS` tick") that was only true when no other long hold intervened — corrected to
describe latency bounded by the nearest injected hook (this ADR's own concern) and, for the cyclic
reap specifically, by the drain-watermark soundness condition ADR-101 Decision §E owns and
documents its own residuals for. This ADR's own residual set is unchanged from the base mechanism:
`dispatch_due_tester_present`'s existing hold-bounded notification latency, now shared by both reap
duties at every site above.

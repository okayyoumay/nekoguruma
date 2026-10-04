# ADR-147: `CP_SuspendQueueOnError` — a Third, Content-Triggered TX-Suspend Source

**Date:** 2026-07-29
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (`LogicalLinkState`, `RcHandlingConfig`,
             `ComParamSet::suspend_queue_on_error`), `j2534-0404-service/src/service/events.rs`
             (`dispatch_tx_item`, `drain_tx_held_backlog`, `bind_registrant`, `bind_frame`,
             `poll_rx_inner`, `wait_for_expected_response_inner`, `cancel_held_tx_items`,
             `handle_channel_hard_error`, `handle_update_param`),
             `j2534-0404-service/src/service/rpc_misc.rs` (`ioctl_resume_tx_queue`)

## Context

`CP_SuspendQueueOnError` (ISO 22900-2 Annex D ComParam table, the D.2-area entry for this
parameter) was declared, name-resolved, allowlisted for the CAN/KWP/J1850 protocol families,
and defaulted to `0`, but never actually read at runtime — setting it had no observable effect.

ADR-123 already built a TX-queue suspend/resume mechanism with two independent, OR'd sources:
`tx_suspended_by_ioctl` (the client's own `PDU_IOCTL_SUSPEND_TX_QUEUE`) and
`tx_suspended_by_lock` (a sibling CLL's held `LOCK_PHYSICAL_TX_QUEUE`, recomputed from scratch
by `recompute_lock_tx_suspensions`). Neither fits `CP_SuspendQueueOnError`'s shape: it is
**content-triggered** (an unhandled negative response or an RX timeout observed on the wire),
not call-triggered the way both existing sources are. Reusing `tx_suspended_by_ioctl` would let
a stray, unrelated positive response on the CLL silently clear a client's own explicit
`PDU_IOCTL_SUSPEND_TX_QUEUE` the moment this ADR's own "positive response always clears"
resume rule fired — a client-visible correctness regression the ioctl source's own contract
(explicit-call-scoped, ADR-123) never had to guard against before. Reusing
`tx_suspended_by_lock` would let `recompute_lock_tx_suspensions`'s from-scratch sweep (which
recomputes solely from `held_lock_mask` across every CLL sharing a physical resource) silently
clobber this CLL's own error-suspend state on any unrelated sibling lock transition, since that
sweep has no way to distinguish "should stay suspended, error still unresolved" from "recompute
found no lock holder, clear it." A third, independent flag is therefore needed.

## Decision

### A third flag, lock-class gating

`LogicalLinkState` gains `tx_suspended_by_error: bool`, OR'd into `tx_suspended()` alongside the
existing two flags. Siphon gating (`dispatch_tx_item`/`drain_tx_held_backlog`) treats it exactly
like `tx_suspended_by_lock` — transmitting-items-only, with the same FIFO no-overtake carve-out
at fresh-siphon time — rather than like `tx_suspended_by_ioctl`'s unconditional gating. This is
deliberate: a non-transmitting recovery COP (most notably a `CoptUpdateparam` that itself clears
`CP_SuspendQueueOnError` back to `0`) must still be able to run while error-suspended, the same
way a non-transmitting item is never held up by a sibling's `LOCK_PHYSICAL_TX_QUEUE` alone.

`tx_suspended_by_error` has **no cross-CLL inputs** (it depends only on this CLL's own traffic
and its own live `CP_SuspendQueueOnError` value) — unlike `tx_suspended_by_lock`, it is never
touched by `recompute_lock_tx_suspensions`'s from-scratch sweep, and is not added as a ninth site
to that mechanism's invariant table (ADR-123 §3). It is scoped and cleared entirely within its
own CLL.

### Two trigger hooks

1. **Timeout (`wait_for_expected_response_inner`).** Inside the existing generation-gated
   critical section that already precedes a `PduErrEvtRxTimeout` emission (the same
   `still_on_this_channel` freshness check that guards the event itself), if the link is still
   fresh and live Active `CP_SuspendQueueOnError` is `1`, set `tx_suspended_by_error = true`. No
   second freshness check is introduced; the existing one is reused for both purposes.
2. **Unhandled-0x7F classification, at bind time (`bind_registrant`/`bind_frame`, not in the
   wait loop).** `bind_registrant`'s tier-1 (non-vacuous) scan is the only place every content
   frame meets every registrant's frozen `rc_cfg` — it already runs
   `RcHandlingConfig::detect_pending_rc` there, gated by the same ECU-scope
   (`rc_unique_resp_ok`) filter that gate uses. A new sibling method,
   `RcHandlingConfig::is_unhandled_negative`, classifies the same frame's NRC as *unhandled*
   when it is `0x7F`-led, passes the identical SID-echo/shape gate `detect_pending_rc` uses, and
   the NRC at `rc_byte_offset` is NOT claimable by `detect_pending_rc` — either it isn't one of
   the RC21/RC23/RC78-mapped codes at all, or it is but the matching `CP_RCxxHandling` is
   currently `0`. It deliberately does its own shape/NRC check rather than delegating to
   `detect_pending_rc`, which early-returns via `any_enabled()` whenever the whole RC engine is
   disabled — exactly the case this method exists to still catch. Like `detect_pending_rc`, it
   only classifies for the standard UDS/KWP shape (`rc_byte_offset >= 2`); `rc_byte_offset < 2`
   framings decline to classify at all (accepted residual, matching `detect_pending_rc`'s own
   ADR-100 residual).

   A small `enum QueueErrorClass { Suspend, Positive }` is folded per-frame across a whole
   `poll_rx_inner` pass into a per-CLL `Option<QueueErrorClass>`
   (`CllRxEntry::queue_error_class`), with **last frame in the pass wins** — matching "held until
   a positive response" temporal semantics, where a later positive response in the same batch
   overrides an earlier suspend-worthy frame. A pending-RC frame (handled by the RC engine) has
   no queue effect. A bound match whose payload is unhandled per the method above, or an
   otherwise-unbound frame that also qualifies as unhandled, classifies `Suspend`; any other
   bound match classifies `Positive`, subject to the tier-2 heuristic below. The classification is
   applied to the live CLL in the same end-of-pass `logical_links` critical section that already
   runs `merge_registrant_writeback` — a plain link-field write there, not a new
   registrant-writeback participant (ADR-101 stays untouched): `Positive` clears
   `tx_suspended_by_error` unconditionally; `Suspend` sets it only when live Active
   `CP_SuspendQueueOnError == 1` at apply time; the whole apply step is additionally gated on the
   link's `connect_generation` matching the batch's own snapshot generation, so a reconnected
   link never inherits a stale pass's classification. A wake-on-transition (`TxItem::ResumeWake`)
   is sent, mirroring `ioctl_resume_tx_queue`'s existing lock-then-release-then-send shape (never
   holding `logical_links` and `shared_channels` together), whenever applying the classification
   flips `tx_suspended()` true → false.

   **Correction (post-merge review round, design-advisor + edge-case-hunter): classification is
   written exactly once per frame, from that frame's FINAL binding outcome, never as a loose side
   effect of the registrant scan itself.** The first cut wrote `queue_error_class` directly
   inside `bind_registrant`'s tier-1 (non-vacuous) scan the moment ANY registrant's own `rc_cfg`
   judged the frame unhandled — even a registrant that never went on to actually bind the frame —
   which produced three bugs: (1) a tester-present reply could be spuriously flagged `Suspend` by
   an unrelated registrant's `rc_cfg` (most easily reproduced with a registrant whose own
   `request_sid` is `None`, so `is_unhandled_negative`'s SID-echo gate never excludes anything)
   before `bind_frame`'s own, later tester-present-discard check ever ran, with no rollback when
   that check then fired; (2) a registrant with only a *vacuous* descriptor cannot bind in the
   tier-1-non-vacuous scan at all (the `!e.is_vacuous()` gate), so its own classification was
   computed, discarded, and then silently overwritten by the separate tier-1-vacuous scan pass,
   which never recomputed the unhandled-negative check for its own accepted match and so always
   wrote `Positive`; (3) a tier-2 (Receive Only) registrant has no `rc_cfg` at all, so its
   accepted-match classification was unconditionally `Positive` — including for a bound `0x7F`-led
   payload, wrongly auto-resuming a suspended queue on what is actually a negative response.

   The corrected shape: `bind_registrant` no longer takes a `queue_error_class` out-parameter at
   all. It takes a `sighted_unhandled: &mut bool` instead — a pure SIGHTING record, OR'd across
   every scan call for one frame, set when the tier-1-non-vacuous scan observes some registrant's
   own `rc_cfg` judge the frame unhandled, regardless of whether that registrant (or any other)
   ever actually binds it. Classification itself is now computed fresh, for every scan variant,
   only at the accepted-match site (`bind_registrant`'s own `matches_got += 1` arm), from THAT
   bound registrant's own `rc_cfg` — and returned to the caller alongside the match, rather than
   written as a side effect mid-scan. `bind_frame` (which owns `entry.queue_error_class`) then
   writes it at exactly one of three points, mutually exclusive per frame: (a) a registrant binds
   — the returned classification is applied immediately; (b) tester-present claims the frame —
   no write at all, unconditionally, closing bug (1) with no rollback/snapshot bookkeeping needed;
   (c) nobody binds it (the `FrameBinding::Unbound` fallthrough, the paradigm case this ADR was
   built for) — `Suspend` is written here IFF `sighted_unhandled` was raised anywhere during the
   scans for this frame. Bug (2) is closed because the vacuous-descriptor registrant's own
   classification is now computed fresh at its actual (tier-1-vacuous) binding site, instead of
   inheriting a stale value from a different scan pass.

   Bug (3)'s fix is a deliberate heuristic, not a full solution: a tier-2 (Receive Only)
   registrant still has no `rc_cfg` (RC handling remains tier-1 only -- per ISO 22900-2 §8.2.6.3.4's
   "7F Handling" row, 0x7F auto-handling only proceeds for a ComPrimitive that actively sends a
   request and awaits its response; a receive-only ComPrimitive is excluded), so it cannot
   classify `Suspend` the way tier-1 does. Rather than blanket-classify every tier-2 match
   `Positive` (the bug), a bound tier-2 match whose payload starts with `0x7F` now writes NO
   classification at all — neither `Suspend` nor `Positive` — leaving any existing suspension
   exactly as it was. This extends the existing `rc_byte_offset < 2` accepted-residual class this
   ADR's Consequences section already documents: a missed auto-resume is the safe failure
   direction, and the client's explicit recovery actions (`PDU_IOCTL_RESUME_TX_QUEUE`, or a
   `CoptUpdateparam` demoting `CP_SuspendQueueOnError` to `0`) remain available regardless —
   `PDU_IOCTL_CLEAR_TX_QUEUE` alone only cancels the held backlog and does not itself resume
   dispatch (see the Consequences section's own corrected bullet on this).

   **Further correction (post-merge review round, Codex + design-advisor): the same
   decline-to-classify heuristic unified across BOTH the `Some(rc_cfg)` and `None` cases.** The
   fix above initially wrote the accepted-match classification as a binary per-branch choice —
   `Some(cfg)` always resolved to `Suspend`/`Positive`, `None` alone got the decline-to-classify
   treatment for a `0x7F`-led payload. That conflated `is_unhandled_negative`'s four distinct
   `false` outcomes (not `0x7F`-led at all; `0x7F`-led but the SID doesn't echo this registrant's
   own `request_sid`, i.e. unattributable to its own request; `rc_byte_offset < 2`; a truncated
   `7F <sid>` with no byte at `rc_byte_offset`) into a single `Positive`, so a tier-1 registrant
   with an `rc_cfg` present still wrongly auto-resumed on an unattributable or truncated negative
   response. The corrected rule is unified across both `rc_cfg` states: `Some(cfg)` classifies
   `Suspend` only when `is_unhandled_negative` positively CONFIRMS it; every other `0x7F`-led
   accepted match — `Some(cfg)` unconfirmed for any of the above reasons, or no `rc_cfg` at all
   (tier-2) — declines to classify; anything not `0x7F`-led classifies `Positive`. See the
   Consequences section's extended `rc_byte_offset < 2` bullet for the accepted-residual
   reasoning this generalizes.

### Live-Active vs. frozen-`rc_cfg` split

The timeout hook and the apply step both read live Active `CP_SuspendQueueOnError` — new ground
relative to `RcHandlingConfig`'s own fields, all of which are frozen at COP call time (ADR-067's
binding-timing model, reinforced by ADR-110's later live-read carve-out for
`LOCK_PHYSICAL_COM_PARAMS`). The split is deliberate and mirrors ADR-110's own precedent for
exactly this kind of question: "is this negative response handled" is COP-scoped
policy — the RC engine's auto-handling contract belongs to the COP that issued the request, so
it is judged against that COP's own frozen `rc_cfg` snapshot — while "should an error suspend
the queue" is CLL-scoped policy that can legitimately change between one frame and the next
(a client can flip `CP_SuspendQueueOnError` at any time via `CoptUpdateparam`), so it is judged
live, at classification-apply time and at timeout time, exactly like ADR-110's
`LOCK_PHYSICAL_COM_PARAMS` conflict check moved off call-time binding for the same reason.

### Four explicit cleanup sites, not ADR-123's eight-site sweep

Because `tx_suspended_by_error` has no cross-CLL inputs, it needs none of ADR-123's eight
`recompute_lock_tx_suspensions`-invariant sites. It has exactly four explicit clear sites
instead, chosen by what each one already means for the analogous ioctl/lock flags:

1. **`cancel_held_tx_items`'s `reset_suspended`-gated branch** (disconnect/destroy/
   `PDU_IOCTL_RESET`/`PDU_IOCTL_CLEAR_TX_QUEUE` all route through this one function). Clears
   `tx_suspended_by_error` alongside the existing `tx_suspended_by_ioctl` clear — grouped with
   the *ioctl-class* clear, not the lock-class one, since this flag protects no sibling CLL's
   privilege the way a held physical-resource lock does. Whether this branch actually fires for
   a given caller is unchanged from before this ADR: it depends solely on that caller's own
   `reset_suspended` argument (`PDU_IOCTL_CLEAR_TX_QUEUE` already passes `false` here, so it
   cancels a held item without itself resuming dispatch — a pre-existing, documented behavior
   this ADR does not alter).
2. **`ioctl_resume_tx_queue`.** A client's explicit `PDU_IOCTL_RESUME_TX_QUEUE` is a full manual
   escape from any suspend source that protects no sibling CLL — clears
   `tx_suspended_by_error` alongside `tx_suspended_by_ioctl`, leaving `tx_suspended_by_lock`
   untouched (unchanged ADR-123 behavior).
3. **`handle_update_param`'s Active-param promotion.** A `CoptUpdateparam` promotion landing
   Active `CP_SuspendQueueOnError == 0` clears `tx_suspended_by_error` in the same critical
   section as the promotion write, applying the same wake-on-transition pattern as the bind-time
   classification.
4. **`handle_channel_hard_error`'s per-CLL offline handling.** Clears `tx_suspended_by_error`
   alongside this function's other per-CLL offline resets, so a dead link's error-suspension
   never survives into a reconnect. No wake is sent here — the CLL just went offline.

### Addendum (follow-up fix, post-merge Codex review + design-advisor): `CoptUpdateparam`-specific recovery exemption

**The bug.** The documented recovery path — queue a `CoptUpdateparam` that demotes Active
`CP_SuspendQueueOnError` to `0` — deadlocked once any transmitting item was already parked in
`tx_held` under error-suspend. `dispatch_tx_item`'s FIFO no-overtake clause (the same one that
protects lock-suspend's ordering, `!tx_held.is_empty()`) caught the recovery `CoptUpdateparam`
too, since it shares that clause with the lock-class gating this ADR's original Decision
describes above — the recovery item was pushed to the *back* of `tx_held` instead of executing.
`drain_tx_held_backlog` then only ever popped the *front* of `tx_held`, and only when the front
was non-transmitting or the suspension was fully cleared; the front was the still-blocked
transmitting item, so nothing ever popped. The recovery item was permanently stuck: it could not
dispatch (held behind the FIFO clause) and could not drain (never at the front, and the front
never popped) — making the held item a precondition of its own release. The prior test suite
encoded this as a *workaround* rather than a fix: `rc78_disabled_suspends_and_resumes_via_update_param`
issued a manual `PDU_IOCTL_CLEAR_TX_QUEUE` before the `CoptUpdateparam`, with a comment
documenting the exact deadlock this addendum now closes. That test is corrected below to
exercise the real recovery flow without the workaround.

**The fix: an `UpdateParam`-specific exemption, not a blanket non-transmitting one.** A blanket
"any non-transmitting item bypasses error-suspend's FIFO gate" would recreate the very
comm-state-inversion hazard this ADR's original Decision (and ADR-123 Fix B) gate against — e.g.
an empty-data `CoptStopcomm` overtaking an already-held `CoptStartcomm`. Only `TxItem::UpdateParam`
is the item kind that can actually clear `tx_suspended_by_error` (via `handle_update_param`'s
promotion path): `RestoreParam` only copies Active → Working in memory and cannot clear it, and
`Delay`/`ResumeWake` are irrelevant here (`ResumeWake` is filtered out before ever reaching the
siphon). The fix therefore narrowly exempts `TxItem::UpdateParam` alone from the FIFO clause,
specifically while `tx_suspended_by_error` is set — `tx_suspended_by_ioctl`'s unconditional block
is never bypassed, matching this ADR's original Decision that the ioctl source protects no
sibling and needs no such carve-out to begin with (it already has its own explicit escape,
`PDU_IOCTL_RESUME_TX_QUEUE`).

The exemption fires whenever `tx_suspended_by_error` is set, **regardless of whether
`tx_suspended_by_lock` is also set concurrently**. This is safe because the two suspend sources
protect different things: the lock's guarantee is resource-level (no transmitting traffic on the
shared physical resource while a sibling holds `LOCK_PHYSICAL_TX_QUEUE`), never an ordering
guarantee over *this* CLL's own queue — a non-transmitting item executing under a concurrently
held lock is already legal per this ADR's original Decision and ADR-123 Fix B/D (a
non-transmitting item is never held up by a sibling's lock alone). The error source's recovery
item is different in kind: since it is itself a queued item, an unconditional hold on it — even
one shared with the lock's own gating — makes it impossible to ever release, which the lock's own
non-transmitting carve-out was never at risk of (a lock resolves by an external `unlock_resource`
call, not by something inside the held queue itself).

A second stranding path, independent of the FIFO clause, was also closed:
`recompute_lock_tx_suspensions`'s wake-target set (this ADR's original Decision's list of
`recompute_lock_tx_suspensions`-invariant call sites is unaffected in count; only the set's
*contents* change) is now keyed on the `tx_suspended_by_lock` transition alone, not on the CLL's
*effective* `tx_suspended()` transition. This is NOT a defense against a sibling holding the lock
while a plain in-flight COP on the locked CLL times out — that ordering cannot strand anything (see
the reachability analysis below: every channel's single serial poll task sequences both error
setters before any later dispatch attempt on that same channel, so a recovery `CoptUpdateparam`
dispatched after the timeout always observes the flag and takes the bypass above). The real,
narrower windows this closes are nondeterministic ones on a dual-channel (UUDT companion, ADR-046)
CLL: (a) a companion pass's classification writeback lands, under the lock's FIFO clause, only
*after* the main task has already siphoned a recovery `CoptUpdateparam` behind a sibling's lock —
under the old effective-transition-only contract, if the sibling then unlocks while
`tx_suspended_by_error` is still set from that late-landing classification, `tx_suspended()` stays
`true` throughout (lock before, error after) and no wake fires, stranding the `CoptUpdateparam`
until the client's next unrelated dequeue on that CLL; (b) the Fix-F concurrent-suspend
`push_front` window (ADR-123). Widening the wake-target set to fire on the `tx_suspended_by_lock`
transition alone closes both: a spurious wake while still effectively suspended is harmless, since
`drain_tx_held_backlog` re-runs its own authoritative gate check regardless of why it was woken
(the same reasoning this ADR's original Decision already applies to the bind-time
classification's own wake-on-transition).

`drain_tx_held_backlog`'s own pop gate gained a matching fallback: its existing front-pop arm is
unchanged verbatim (front-pop under `tx_suspended_by_lock`/`tx_suspended_by_error` alone still
requires the front item to be non-transmitting, exactly as this ADR's original Decision
describes). A new fallback arm, active only while `tx_suspended_by_error` is set and the front-pop
condition does not hold, searches `tx_held` for the first `TxItem::UpdateParam` from anywhere in
the deque (not only the front) and dispatches it out of order, preserving the relative order of
everything else. This cannot livelock the way an earlier, cruder fix (ADR-123 Fix F) warns
against: at drain-re-entry (`is_backlog_drain == true`) `dispatch_tx_item`'s own FIFO clause is
already suppressed, and `UpdateParam` never transmits, so a middle-popped `UpdateParam` can never
be re-siphoned back into `tx_held` — once its own promotion clears `tx_suspended_by_error`, the
same drain loop's next iteration resumes popping the front normally.

Structural note on reachability: the plain "lock held while a COP on the SAME locked CLL times out
mid-flight, setting error" precondition is genuinely unreachable — `LockResource`'s own busy check
(`LOCK_PHYSICAL_TX_QUEUE`, this ADR's sibling, ADR-123 Fix C/J) rejects lock acquisition while any
*transmitting* COP is executing on the shared resource, for the COP's entire lifetime, and only a
transmitting (tier-1) COP can ever drive a `Suspend` classification — so the lock can never be
acquired while a COP capable of setting `tx_suspended_by_error` on that same CLL is still live.
However, a **mirrored** path does reach lock+error coexistence: the busy check is grant-time-only,
keyed on the lock-acquiring CLL's *then-current* resource id. A not-yet-connected SAE J1850 CLL B
can acquire the lock before its `hw_protocol_id` resolves (so it doesn't yet match any other
resource); once connected, `autodetect_sae_j1850_flavor` resolves B's flavor and calls
`recompute_lock_tx_suspensions` in the same critical section, with no busy-check re-validation —
retroactively setting `tx_suspended_by_lock` on any other CLL A that now shares the resolved
resource, even if A has a live transmitting COP the busy check would have rejected had B's identity
been known at grant time. If that COP later times out, A is genuinely lock-suspended and
error-suspended at once.

Even so, this mirrored path cannot strand the recovery `CoptUpdateparam`: each channel's poll task
is strictly serial, and both error setters — the receive-phase timeout hook and the end-of-pass
classification apply — are sequenced, on that same task, before it can attempt any later dispatch
on the channel. A recovery `CoptUpdateparam` dispatched after either setter therefore always
observes `tx_suspended_by_error` already set and takes the bypass above; one dispatched before
either setter simply executes (no suspension exists yet). This holds across the ioctl, lock,
J1850-autodetect-retroactive-lock, and receive-only-COP orderings alike, and a J1850 CLL has no
UUDT companion task to break the serialization. The wake-widening/middle-pop fallback's real
justification is therefore the two nondeterministic dual-channel windows described in the Decision
section above (a late companion-pass classification landing under a sibling's lock; the Fix-F
concurrent-suspend `push_front` window) — not this single-channel autodetect path, which the
dispatch-side bypass alone already resolves correctly.

### Third addendum (Codex-review fix, design-advisor mechanism): a per-link `error_suspend_epoch` closes a classification-vs-explicit-clear race

**The bug (two related race shapes, one root cause).** `poll_rx_inner`'s classification pipeline
computes a frame's `queue_error_class` (`Suspend`/`Positive`) during the per-frame scan, but only
applies it to `link.tx_suspended_by_error` at the end-of-pass writeback block. Frames land in the
per-CLL event queue (`rx_buf`, mid-scan) and are exposed to the client BEFORE this end-of-pass
apply runs. Two race shapes follow from that gap:

1. **Client-reaction.** The client observes a delivered frame (an unhandled negative response)
   and calls `PDU_IOCTL_RESUME_TX_QUEUE` before this same pass's end-of-pass writeback runs. That
   ioctl clears `tx_suspended_by_error` synchronously. Moments later, the SAME pass's writeback
   block applies the now-stale `Suspend` classification (computed before the resume), silently
   re-suspending the queue and undoing the client's explicit resume — the client sees success but
   the queue stays suspended until a second resume.
2. **Companion-vs-promotion (design-advisor, found while ruling on the fix above; a non-client-
   reaction shape).** A UUDT-companion poll task's pass classifies `Suspend` from its own frame
   batch; concurrently, the MAIN poll task's `handle_update_param` promotes Active
   `CP_SuspendQueueOnError` to `0` (clearing `tx_suspended_by_error`) in response to an
   earlier-queued recovery `CoptUpdateparam`. No delivery-ordering fix can catch this shape, since
   the triggering "exposure" was an earlier COP, not the racing frame.

**The fix: a per-link suspension epoch.** `LogicalLinkState` gains `error_suspend_epoch: u64`,
initialized `0`, advanced by every explicit action that clears an ACTIVE `tx_suspended_by_error` —
content classifications captured before an advance are stale and must not apply. The four bump
sites are exactly this ADR's four existing explicit-clear sites above (`ioctl_resume_tx_queue`,
`cancel_held_tx_items`'s `reset_suspended`-gated branch, `handle_update_param`'s promotion-clear
block, `handle_channel_hard_error`'s per-CLL offline handling), each already inside a
`logical_links` critical section alongside its existing clear of `tx_suspended_by_error` — the
epoch bump is added in the SAME critical section, via a small shared helper,
`LogicalLinkState::clear_error_suspension`. It is deliberately NOT bumped by the batched `Positive`
classification apply — that is in-band content, not an explicit action.

**Fail-open correction (post-merge Codex review round, design-advisor mechanism): the epoch must
NOT bump when there was nothing active to invalidate.** The bump sites above originally advanced
`error_suspend_epoch` unconditionally, even when `tx_suspended_by_error` was already `false` —
reasoning that "an explicit resume must also invalidate any in-flight classification the client
hasn't seen yet" applied regardless of the flag's current value. That reasoning proved backwards: a
client resuming from an earlier, ALREADY-RESOLVED suspend episode (flag already `false`) still
bumped the epoch, and if a genuinely NEW, fresh `Suspend` classification from a NEW unhandled
negative response happened to land in the same poll pass, the epoch mismatch discarded it
entirely — even though that classification has nothing to do with the resume that bumped the
epoch. The queue then failed to suspend on a real, fresh unhandled error: the dangerous failure
direction for a feature whose whole purpose is stopping traffic after an unhandled error (fail-open
instead of fail-safe). The corrected rule: the epoch advances only when an explicit action actually
cleared an ACTIVE error-suspension — there must be something to invalidate. `LogicalLinkState`
gains a small helper, `clear_error_suspension(&mut self) -> bool`, that clears
`tx_suspended_by_error` and bumps `error_suspend_epoch` only when the flag was `true` beforehand,
returning whether it was, for the caller's own use — no current call site needs this beyond the
narrowing check just described. It is NOT equivalent to "did the effective suspension change":
that also depends on `tx_suspended_by_ioctl`/`tx_suspended_by_lock`, which this per-source flag
alone cannot reflect. `ioctl_resume_tx_queue` (`rpc_misc.rs`) discards the return value entirely and
unconditionally sends `ResumeWake` whenever a `tx_queue` exists; `handle_update_param` (`events.rs`)
also discards it, instead computing its own `old_effective = link.tx_suspended()` before the call and
comparing against `!link.tx_suspended()` after, precisely because the raw per-source boolean cannot
stand in for that OR. All four bump sites now route through this helper instead of bumping the epoch
inline. `handle_update_param`'s promotion-clear bump was already self-limiting (reached only inside
the branch where the promotion also disables the policy), so the helper's narrowing just applies the
same "nothing to invalidate" rule consistently there too; its existing live-policy re-check at apply
time (`if link.active.suspend_queue_on_error()`) remains an independent safety net for the wake
decision, unchanged.

**Connect-time reset, added by the same correction.** Narrowing the teardown-site bumps to
flag-was-true-only reopens a boundary leak the OLD unconditional-bump behavior incidentally closed:
a stale offline-window classification could otherwise leak `tx_suspended_by_error = true` into a
freshly reconnected session. Concretely, if `tx_suspended_by_error` reads `false` at the moment a
CLL goes offline (disconnect or hard error), the narrowed clear no longer bumps the epoch — but
`connect_generation` is unchanged until the *next* `ConnectComLogicalLink` finalizes, and the
apply-time epoch check alone does not cover the pre-reconnect offline window, so a late-landing
classification (computed before the offline transition, sharing both the still-current generation
and the still-current epoch) could still apply after the CLL went offline, setting
`tx_suspended_by_error = true` on a link that is not yet reconnected. `finalize_connected_link`
(`rpc_link.rs`) closes this directly: every finalized connect (including a same-`cll_handle`
reconnect) now resets `tx_suspended_by_error = false` in the same publication critical section that
stamps the fresh `connect_generation`, with no epoch bump — none is needed, since that fresh
generation alone already discards any old-session pass's classification from applying afterward.
This is correct hygiene independent of the leak it closes: a fresh session must never inherit a
prior session's error-suspension state.

**Second correction (Codex review round; design-advisor's final ruling after two prior attempts at
this same mechanism each closed one race and reopened another): capture-at-fold sequencing
replaces the pass-level epoch entirely.** This subsection narrates the arc, so a future reader
does not have to re-derive it: (1) the original unconditional-bump epoch (above) closed the client-reaction race but
introduced the fail-open bug (a resume of an already-resolved, unrelated episode could discard a
genuinely fresh `Suspend` classification from a different, later, unrelated frame in the same
pass); (2) the fail-open correction's conditional (`if was_suspended`) bump (above) closed the
fail-open bug but reopened the ORIGINAL race for a specific sub-case: the FIRST suspend-worthy
frame of an episode is classified while `tx_suspended_by_error` is still `false` (nothing
suspended yet), delivered to the client, and if the client reacts immediately with
`PDU_IOCTL_RESUME_TX_QUEUE`, the conditional bump does not fire (the flag was `false` at that
moment) — so the pass's own end-of-pass writeback then applies the stale classification anyway,
undoing the resume. Root cause both times: a PASS-level staleness signal cannot distinguish "this
pass's classification predates an unrelated resume" from "this pass's classification is a genuine
reaction target the client just resumed away from," because the capture point (pass-snapshot time)
never aligned with the actual moment of risk (the specific frame's exposure to the client).

The governing principle this correction applies instead: **an explicit clear-suspension action
invalidates exactly the classification whose contributing frame's EXPOSURE it might be reacting
to — captured at the moment that frame's classification was folded into the pass, not at
pass-snapshot time and not gated on the live flag's state at bump time.** Concretely:

1. `LogicalLinkState::error_suspend_epoch` is renamed `error_action_seq` (same `u64`, different
   semantics and name to make the break visible): a monotonic sequence compared against the
   sequence captured at the moment a frame's classification last folded `Suspend` into a pass's
   per-CLL entry — NOT at pass-snapshot time.
2. `clear_error_suspension`'s `if was_suspended`-gated bump is removed; the bump is unconditional
   again, matching the ORIGINAL (pre-fail-open-correction) bump's shape — but this time the fix is
   entirely on the CAPTURE side, not the bump side, so the fail-open bug cannot recur: a bump can only ever
   invalidate a fold that already happened before it (a now-stale captured seq), never a fold that
   happens after (which captures the bumped seq itself and so still matches at apply time).
3. `CllRxEntry` replaces its pass-snapshot-time `error_suspend_epoch: u64` field with
   `suspend_seq: Option<u64>`, initialized `None` per pass. It is captured — briefly re-acquiring
   `logical_links`, with no nesting against `rx_buf` (ADR-080 lock-ordering discipline) — at the
   EXACT point `bind_frame` writes/rewrites the per-CLL entry's classification to `Suspend`, and
   strictly BEFORE that same frame is exposed to the client via `rx_buf`. A later frame in the same
   pass that does NOT rewrite the classification to `Suspend` (a `Positive` fold, an accepted match
   that declines to classify, or an unbound frame that sights nothing unhandled) leaves an
   already-captured `suspend_seq` untouched — re-capturing on such a frame would wrongly extend a
   stale `Suspend`'s validity past a legitimate intervening explicit clear.
4. The end-of-pass apply gate (`queue_error_class_to_apply`) is asymmetric between the two classes,
   where the old epoch check was uniform: a `Suspend` classification applies iff
   `link.error_action_seq == entry.suspend_seq` (fold-time-captured), IN ADDITION to the existing
   `connect_generation` check and the existing live-policy re-check
   (`link.active.suspend_queue_on_error()`); a `Positive` classification needs NO seq check at all
   — only the `connect_generation` check — since a stale `Positive` discard was already established
   as a no-op by construction by the original epoch fix, and requiring a seq comparison for it would
   only risk discarding a genuine `Positive` fold that never had (or needed) a seq to compare.
5. `finalize_connected_link`'s connect-time reset (unchanged by this correction) is now purely
   hygiene rather than closing a live leak on its own: with the bump unconditional again, every
   teardown-site clear already bumps `error_action_seq` regardless of the flag's prior state, which
   alone invalidates any pre-offline fold's captured `suspend_seq` at apply time. The reset remains
   because a fresh session must never inherit a prior session's error-suspension state, full stop,
   independent of whichever mechanism also happens to block a stale pass from applying.

Trace confirming this closes all three findings: the original race (stale `Suspend` captured seq
`S` at fold time; resume bumps to `S+1` unconditionally; apply sees `S ≠ S+1`, discards) — closed.
The fail-open case (resume of an already-resolved episode bumps `S -> S+1`; a genuinely NEW, later
frame folds `Suspend` AFTER the resume and captures `S+1`, since fold happens after the bump in
wall-clock time; apply sees `S+1 == S+1`, applies, the queue suspends correctly) — closed, and this
is exactly the case the pass-level epoch got wrong, because its capture predated the frame's actual
arrival, while fold-time capture does not have this problem. This round's finding (flag `false`,
first frame of an episode: the frame folds `Suspend`, captures seq `S` at that exact moment, and is
then exposed to the client; the client reacts with a resume, bumping to `S+1`; apply sees `S ≠
S+1`, discards; the resume is honored) — closed.

`CllRxEntry`'s `suspend_seq: Option<u64>` (described in point 3 above) replaces the old
pass-snapshot-time `error_suspend_epoch: u64` field entirely. The end-of-pass apply gate (point 4
above) is factored into the same small pure helper as before, `queue_error_class_to_apply` —
unit-tested directly against its full truth table (`events.rs`, `queue_error_class_to_apply_tests`),
now over `entry_suspend_seq: Option<u64>`/`live_error_action_seq: u64` rather than a pair of
pass-snapshot epochs. A dedicated unit test, `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend`
(`events.rs`, `bind_frame_tests`), pins the fold-capture timing itself — `bind_frame` gained a
`wrote_suspend: &mut bool` out-param, set `true` at exactly the write site that writes `Suspend`
(never on a `Positive` write, a declined-to-classify accepted match, or a frame that sights
nothing) — since `bind_frame` itself is sync and cannot take the `logical_links` lock the actual
`suspend_seq` capture needs, `poll_rx_inner` is what performs the capture, gated on this exact
flag, immediately after `bind_frame` returns and strictly before this frame's delivery.

**Why the timeout hook needed no such treatment, at the time.** The timeout hook
(`wait_for_expected_response_inner`) already implements "set-before-expose" correctly: it sets
`tx_suspended_by_error` inside its own critical section BEFORE the `PduErrEvtRxTimeout` event is
emitted, because it is a single synchronous action. The classification pipeline's problem is
structural, not incidental to this one hook: a whole poll pass's classification only completes
(and is only applied) at end-of-pass, even though frames must be exposed to the client mid-pass —
the fold-then-apply shape is what creates the window an epoch is needed to close. This remains
true of the hook's OWN correctness (it still needs no capture-side treatment for its own set), but
see the third correction immediately below for why the hook nonetheless gained a role in the
sequencing mechanism itself, as a bump source other classifications must be sequenced against.

**Third correction (Codex review round; design-advisor re-derived the mechanism from first
principles and confirmed a fourth real bug on it): unified `error_state_seq` closes a
companion-`Positive`-vs-timeout race.** The second correction's reasoning for exempting `Positive`
classifications from any seq check — "a stale `Positive` discard is a no-op by construction" —
covered only CONTENT-vs-CONTENT races (companion-pass classifications racing each other). It never
considered a `Positive` classification racing the TIMEOUT HOOK specifically: the hook sets
`tx_suspended_by_error = true` through a code path entirely separate from the classification
pipeline, with no seq association at all up to this point.

**The bug.** On a dual-channel CLL (UUDT companion, ADR-046), a companion-channel poll pass folds a
`Positive` classification (capturing nothing, since `Positive` wasn't sequenced), then pauses
before its own end-of-pass writeback. Meanwhile the PRIMARY channel's receive-phase timeout fires
independently, setting `tx_suspended_by_error = true`. The companion pass's delayed writeback then
applies its `Positive` classification unconditionally, clearing the flag the timeout JUST set and
sending a wake — letting queued transmissions run despite an unhandled timeout that should still be
suspending them. This violates the intended semantics: a positive response resuming the queue must
genuinely follow the error, not predate it.

**The fix: one unified counter, symmetric gating for both classifications.** `error_action_seq` is
renamed `error_state_seq` — the semantics genuinely change from "clear actions only" to "every
synchronous state change to `tx_suspended_by_error`." The bump set gains a fifth site: the timeout
hook's own `tx_suspended_by_error = true` set (inside the same `still_on_this_channel` critical
section that already guards the event emission, strictly BEFORE `send_error_event` — preserving the
existing set-before-expose property), bumping directly rather than through
`clear_error_suspension` since it SETS rather than clears. `CllRxEntry::suspend_seq` is renamed
`class_seq` and generalized to capture on EITHER a `Suspend` OR a `Positive` write (`bind_frame`'s
`wrote_suspend` out-param is renamed `wrote_class` and generalized identically) — the existing rule
that only a frame that actually WRITES/REWRITES the classification refreshes the capture is
unchanged. `queue_error_class_to_apply`'s gate becomes symmetric: both classifications now require
`link.error_state_seq == entry.class_seq`, in addition to the existing `connect_generation` check;
`Suspend`'s existing live-policy re-check (`link.active.suspend_queue_on_error()`) is folded into
the same function as the one remaining Suspend-only differentiator, once the seq/generation gating
shape became identical for both variants.

**Governing rule.** The counter sequences SYNCHRONOUS state changes — whose evidence time equals
their apply time, so they are never themselves stale — against DEFERRED content classifications,
whose evidence time is their fold time. Deferred-vs-deferred (two passes' classifications racing
each other) remains last-writer-wins per the existing cross-channel residual, which is why
classification APPLIES do not themselves bump the counter — making them bump would re-key
cross-channel outcomes to apply order rather than evidence order, the exact defect that residual
already documents, just manifesting as discard instead of overwrite.

Trace confirming this closes all four shapes, the first three unchanged from the second correction:
(1) the original race (stale `Suspend` captured seq `S`; resume bumps `S -> S+1`; apply sees
`S ≠ S+1`, discards) — unchanged. (2) The fail-open case (resume of a resolved episode bumps
`S -> S+1`; a genuinely new frame's fold captures `S+1` after the bump; apply sees `S+1 == S+1`,
applies) — unchanged. (3) The first-frame-of-episode case (flag `false`, the frame folds and
captures `S` before exposure; the client's resume bumps to `S+1`; apply sees `S ≠ S+1`, discards;
the resume is honored) — unchanged. (4) NEW — the companion-`Positive`-vs-primary-timeout race: the
companion folds `Positive` and captures `S`; the timeout sets the flag AND bumps to `S+1`; the
companion's delayed apply sees `S ≠ S+1`, discards, sends no wake, and the queue correctly stays
suspended — closed. A side-effect check: a pending `Suspend` classification captured pre-timeout
that somehow reaches apply after the timeout's bump is discarded too, but this is harmless — the
flag is already `true` from the timeout itself, so losing a redundant "set true again" is a no-op,
not a regression.

**Orthogonal to the existing last-writer-wins content-vs-content residual.** This mechanism (epoch,
then capture-at-fold sequencing, then the unified counter) does not change — and is not a fix
for — the existing accepted residual on companion-pass classification races (see the "Accepted
residual — companion-pass classification races" bullet below): two concurrent passes classifying
the SAME CLL from genuinely concurrent frame content still resolve by writeback-landing order, not
frame chronology, exactly as before. This mechanism governs a different axis entirely — a
synchronous state change (explicit clear action, or now also the timeout hook's own set) versus a
deferred content classification that predates it — not content versus content. The unified counter
deliberately excludes classification APPLIES from its own bump set for exactly this residual's
documented reason (see the governing rule above): bumping on apply would re-key the
already-accepted last-writer-wins content-vs-content outcome to apply order instead of evidence
order, turning today's accepted "last writeback wins" overwrite into a discard of whichever pass's
apply happened to run second — both passes would have captured the same pre-bump seq at fold
time, so the first apply lands normally and its own counterfactual bump invalidates the second
pass's now-stale capture — a strictly worse outcome for the same accepted residual (a silent loss
instead of a well-understood overwrite), not a fix for it.

**Fourth correction (Codex review round; design-advisor's full re-derivation — the fifth
amendment to this sequencing mechanism overall): split direction-specific anchors, superseding
the third correction's unified `error_state_seq`.** This subsection continues the arc the third
correction's own subsection narrates: pass-level epoch → conditional (`was_suspended`-gated)
bump → capture-at-fold sequencing → unified counter → **split direction-specific anchors (this
round)**.

**Codex's finding.** `bind_frame` is synchronous and holds no `logical_links` guard; the seq
capture (`entry.class_seq = link.error_state_seq`) happens in a SEPARATE, later
`logical_links.lock().await` acquisition in `poll_rx_inner`, immediately after `bind_frame`
returns. Between the synchronous fold and the async capture, an explicit clear action or the
timeout hook could bump `error_state_seq`, and the capture would absorb the already-bumped value
for a classification whose fold predates that bump.

**Design-advisor's re-derivation: this only matters for HALF of Codex's finding, and the other
half exposes the third correction's own design error, not a bug in its implementation.**

- **Suspend-vs-explicit-clear direction: NOT a bug.** The correct invalidation anchor for
  `Suspend` has always been EXPOSURE, not fold time — an explicit clear should only invalidate
  content the client could actually have seen. A resume landing in the fold-to-capture window
  necessarily happens BEFORE this frame is delivered to the client (the capture site always runs
  before delivery, by construction — see `poll_rx_inner`'s own comment at the capture site). So
  the client cannot have been reacting to this specific frame — from the resume's perspective it
  is a genuinely NEW incident, not a race — and absorbing the post-bump seq while still applying
  `Suspend` is the CORRECT outcome. This needs no code change for `Suspend` at all, only this
  documentation and a code comment at the capture site so a future reviewer does not re-file this
  half.
- **Positive-vs-timeout direction: a REAL bug, and deeper than the non-atomicity Codex
  described.** The correct anchor for `Positive` is WIRE-EVIDENCE order — a positive response
  can only genuinely resume a suspension it postdates on the wire. Every frame in one
  `PassThruReadMsgs` batch physically arrived before that read call returned. If a timeout is
  determined AFTER that read (i.e. after all of that batch's frames already arrived on the wire),
  then NONE of that batch's frames — including any classified `Positive` — may validly clear that
  timeout's suspension, regardless of exactly when within the batch's processing the fold
  happened. Fold-time capture (even made perfectly atomic with the fold) still gets this wrong
  for a `Positive` folded from an EARLIER-arriving frame in the same batch, whenever the seq
  capture merely happens to run, in program order, after the timeout's bump — the real fix needs
  BATCH-READ-time anchoring, not frame-fold-time anchoring, for this direction.

**The fix: split the unified counter back into two direction-specific counters with different
anchors and capture points.** `LogicalLinkState::error_state_seq` is split into two fields:
`error_clear_seq: u64`, bumped ONLY by the four explicit-clear sites (via the existing
`clear_error_suspension` helper, unconditional bump unchanged) — `Suspend`'s own counter; and
`error_set_seq: u64`, bumped ONLY by the timeout hook's own flag-set, in the same critical
section as before, before the event emission, just retargeted to this new field — `Positive`'s
own counter.

`Suspend` gating reverts to the second correction's (capture-at-fold) semantics essentially
unchanged, Suspend-only again: `bind_frame`'s generalized `wrote_class: &mut bool` out-param
(the third correction's own generalization) reverts to Suspend-only semantics, renamed
`wrote_suspend`. `CllRxEntry::class_seq` reverts to being Suspend-specific, renamed
`suspend_seq`, captured at the SAME existing post-fold site in `poll_rx_inner` (immediately after
`bind_frame` returns `wrote_suspend = true`), now reading `link.error_clear_seq` — no change to
WHERE or WHEN this capture happens, since that timing is exactly the EXPOSURE anchor `Suspend`
needs (see the reasoning above).

`Positive` gating moves to an entirely NEW mechanism — batch-read-time capture, not a revert of
anything prior. `CllRxEntry` gains `set_seq_at_read: u64`, populated once per pass at
`build_cll_rx_entries` construction time (before any frame in the pass is processed), reading the
then-current `link.error_set_seq` — under the SAME `logical_links` lock acquisition
`build_cll_rx_entries` already takes for `connect_generation`, so this costs no new lock traffic.
`Positive` classifications gate on `entry.set_seq_at_read == link.error_set_seq` at apply time
instead of any fold-time capture; `Positive` no longer participates in the fold-capture mechanism
at all — `bind_frame` never sets `wrote_suspend` for a `Positive` write.

Apply-gate restructuring (`queue_error_class_to_apply`): `Suspend` applies iff
`connect_generation` matches AND `entry.suspend_seq == Some(link.error_clear_seq)` AND the
existing live-policy re-check (`link.active.suspend_queue_on_error()`); `Positive` applies iff
`connect_generation` matches AND `entry.set_seq_at_read == link.error_set_seq`. Neither
classification's APPLY bumps either counter (unchanged reasoning from the third correction —
preserves the existing cross-channel content-vs-content last-writer-wins residual). No scenario
needs a CROSS check (`Suspend` checked against `error_set_seq`, or `Positive` against
`error_clear_seq`): an explicit clear racing a stale `Positive` is clear-vs-clear (applying the
stale `Positive` is a wake-free no-op regardless, since the flag is already `false` from the
clear); a timeout racing a stale `Suspend` is set-vs-set (idempotent — the flag is already `true`
from the timeout).

**Governing rule (supersedes the third correction's governing rule; design-advisor's own
statement).** Explicit clears invalidate `Suspend` content anchored at EXPOSURE — a clear cannot
invalidate what the client never saw, so fold-time capture (which always precedes delivery) is
the correct anchor for this direction. Autonomous sets (the timeout hook) invalidate `Positive`
content anchored at BATCH-READ evidence order — a positive response can only genuinely resume a
suspension it postdates on the wire, and every frame in one read batch predates that batch's own
read completing, so batch-read-time capture (not fold-time) is the correct anchor for this
direction. **The third correction's unified `error_state_seq` was superseded because one capture
point cannot correctly serve both anchors simultaneously — this was the third correction's own
design error, not an implementation bug in it**: no amount of making the fold-then-capture
sequence more atomic could have fixed the `Positive` direction, since fold time was never the
correct anchor for it to begin with.

Trace confirming this closes every shape (the first three unchanged from the second/third
corrections, verified against the split fields):

1. Original race (stale `Suspend` vs. explicit resume of an active suspension): captured
   `suspend_seq` `S`; resume bumps `error_clear_seq` `S -> S+1`; apply sees `S ≠ S+1`, discards —
   unchanged.
2. Fail-open case (resume of a resolved episode, then a genuinely new frame folds `Suspend`
   afterward): resume bumps `error_clear_seq` first; the new frame's fold captures the
   ALREADY-bumped value; apply matches, applies, suspends — unchanged.
3. First-frame-of-episode (flag `false`, client reacts with a resume): fold captures `S` before
   exposure; resume bumps to `S+1`; apply sees `S ≠ S+1`, discards; the resume is honored —
   unchanged.
4. Companion-`Positive`-vs-primary-timeout (the third correction's own fix target): the
   companion's batch was read (and `set_seq_at_read` captured) BEFORE the timeout bumps
   `error_set_seq` — mismatch at apply, the stale `Positive` is discarded, the queue stays
   suspended. Now correctly closed at BATCH granularity, not merely frame-fold granularity: this
   also correctly handles a `Positive` folded from a frame that arrived in the SAME batch as, but
   was processed slightly before, the timeout's bump — fold-time capture alone could not
   distinguish that case, since it has no way to tell "folded from a frame that arrived before
   the bump" apart from "folded after the bump merely because of unrelated processing order
   within the same pass"; batch-read anchoring makes the distinction moot, since the comparison
   value is fixed before any frame in the batch — including any intervening bump within the
   batch's own processing — was even touched.
5. This round's Codex finding, `Suspend` half: confirmed NOT a bug per the anchor reasoning
   above — no code change, only this documentation and the capture-site code comment.
6. This round's Codex finding, `Positive` half: closed by batch-read-time anchoring (points 4
   above) — a `Positive` can never absorb a same-batch-or-earlier timeout's bump incorrectly,
   since its comparison value was fixed at batch-read time. **Correction (sixth amendment,
   below): this point's own trace conflated two different things that are NOT equivalent —
   "captured before any frame in the batch is PROCESSED" (what the fifth amendment's
   `build_cll_rx_entries`-construction-time capture actually achieved) and "captured at (or
   before) the batch's own READ-COMPLETION time" (what the anchor actually needs).
   `build_cll_rx_entries` runs strictly AFTER `read_messages` returns, with a genuine
   intervening `.await` (`ctx.last_bus_activity.lock().await`, for any batch with real content)
   between read-return and that construction-time capture — so a concurrent bump landing in
   that specific gap was NOT closed by the fifth amendment's capture point, contrary to what
   this point claimed. See the sixth amendment subsection immediately below for the real fix.**
7. Genuine-recovery direction still works: a positive response arriving in a LATER batch (read
   after the timeout) correctly captures the post-timeout `error_set_seq` at that later batch's
   own read time, matches at apply, and clears the suspension.

### Fifth correction (design-advisor re-derivation, the sixth amendment to this sequencing mechanism overall): pre-read capture for the batch anchor, superseding the fifth amendment's construction-time capture

This subsection continues the arc the third correction's own subsection narrates: pass-level
epoch → conditional (`was_suspended`-gated) bump → capture-at-fold sequencing → unified counter
→ split direction-specific anchors (fourth correction / fifth amendment) → **pre-read capture for
the batch anchor (this round)**.

**The bug: the fifth amendment's own capture point still ran AFTER the read, with a genuine
intervening `.await`.** `CllRxEntry::set_seq_at_read` was populated once per pass at
`build_cll_rx_entries` construction time — but `build_cll_rx_entries` is called from
`poll_rx_inner` strictly AFTER `ctx.api.lock().await.read_messages(...)` has already returned and
that lock has been released (around the point the batch's bus-activity timestamp is stamped,
`*ctx.last_bus_activity.lock().await = ...`, which itself is an `.await` for any batch containing
real content). A concurrent task sharing only `ctx.logical_links`'s mutex with this poll task — a
UUDT companion channel's own independent poll task, or this same channel's own receive-phase
timeout hook — can bump `error_set_seq` in that gap. The fifth amendment's capture then absorbs
the post-bump value even though the batch's own read genuinely completed strictly BEFORE the
bump, so the Positive fold from that batch incorrectly matches at apply time and clears a
suspension the timeout just correctly raised. This is the fail-open shape the fourth and fifth
amendments each already fixed once, resurfacing at a new capture-timing location.

**Why simply moving the capture earlier does not fix this.** Moving the capture to immediately
after `read_messages` returns does not close the gap either: that capture itself still requires
`ctx.logical_links.lock().await`, which is its own yield point strictly after the read returns —
any post-read capture point has an identical structural gap a concurrent bump can land in. The
true anchor `Positive` needs is the counter's value as of the batch's own READ-COMPLETION — not
"before any frame in the batch is processed" (the fifth amendment's own, now-corrected framing,
see point 6's correction above). No single capture point can sit exactly AT read-completion,
because reaching it requires two different async locks — `ctx.api` for the read itself,
`ctx.logical_links` for the counter — and no single instant can be inside both locks' critical
sections at once.

**The fix: capture BEFORE the read starts, trading an unreachable equality for an achievable,
sufficient inequality.** `Positive`'s anchor only needs `T_capture <= T_read-completion` — any
bump strictly after read-completion must still cause a discard, which it does as long as capture
happens no later than read-completion. This is achievable on the OTHER side: `T_capture <=
T_read-start <= T_read-completion` holds trivially if capture happens before the read even
begins. Concretely, `poll_rx_inner` now takes `ctx.logical_links.lock().await` ONCE, immediately
BEFORE acquiring `ctx.api` for the read call, and snapshots `error_set_seq` for every CLL handle
on this channel (the same primary-or-UUDT-companion membership filter `build_cll_rx_entries`
itself already uses) into a `HashMap<u32, u64>`. The `logical_links` guard is dropped immediately
after the snapshot — strictly before `ctx.api` is acquired, so the two locks are never nested,
preserving ADR-080's lock-ordering discipline (and the reverse: `ctx.api`'s guard is fully
released, at the end of the `messages`-resolving block, before `logical_links` is ever acquired
again for `build_cll_rx_entries` itself). This snapshot is threaded into `build_cll_rx_entries` as
a new parameter; `CllRxEntry::set_seq_at_read` becomes `Option<u64>`, stamped from the snapshot
map (`Some(seq)` for a present handle) rather than re-read live from `LogicalLinkState` inside
`build_cll_rx_entries` — re-reading live there would reintroduce the identical structural gap
one function call later.

**The direction of the residual error, restated precisely (design-advisor's own reasoning,
verbatim, also present as a code comment at the capture site in `events.rs`):** "A timeout
bumping `error_set_seq` between this pre-read capture and the read's actual completion causes a
genuinely POST-error positive response in that same batch to be discarded (its captured seq is
pre-bump) — this is over-discard, fail-CLOSED, and self-correcting on the CLL's next batch's
positive response. This is the accepted tradeoff versus the alternative: exact 'at
read-completion' equality is structurally unachievable across two different async locks
(`ctx.api` for the read, `ctx.logical_links` for the counter) — no single instant can be inside
both critical sections at once. The inequality `T_capture <= T_read-start <= T_read-completion`
is what's actually achievable and sufficient: it never causes fail-OPEN (a stale Positive
incorrectly clearing a genuine timeout-triggered suspension), only an occasional,
self-correcting fail-CLOSED (a genuine recovery positive discarded, requiring one more positive
response to actually resume) — the safe direction for a mechanism whose entire purpose is not
resuming on unconfirmed evidence."

**The "CLL connected mid-window" case.** If a CLL handle `build_cll_rx_entries` needs to process
is absent from the pre-read snapshot — connected strictly after the snapshot was taken but
before `build_cll_rx_entries` runs, a narrow window — `entry.set_seq_at_read` is stamped `None`
for it instead of a live fallback. `queue_error_class_to_apply` treats `None` identically to a
mismatched `Some(seq)`: always discard `Positive`. This is conservative-by-construction (no valid
batch-anchor exists to compare against) and self-correcting on that CLL's next pass, once it has
a valid snapshot.

**Why an atomic counter was rejected.** Making `error_set_seq` a lock-free atomic (read from
outside the `logical_links` critical section, avoiding the pre-read `logical_links` acquisition
entirely) was considered and rejected. `error_set_seq`'s bump and `tx_suspended_by_error = true`'s
own set are currently paired inside ONE `logical_links` critical section (the timeout hook's own
set-before-expose block) — an atomic read from outside that section could observe the counter
bump and the flag write as separately-ordered events, since nothing enforces their combined
visibility together once the counter is no longer protected by the same lock as the flag it
sequences against. That trades a lock-discipline argument — checkable by inspection, matching
every prior amendment's own style (as this ADR's whole arc demonstrates) — for a memory-ordering
argument, a categorically harder property to verify correct, and exactly the kind of change that
tends to open a NEW round on this mechanism rather than close it. The pre-read capture, plain
mutex discipline throughout, is the terminal, verifiable shape for this anchor.

**Trace confirming this closes the residual gap, restating points 1-3/5/7 as unaffected (only
point 4/6's own mechanism changes):**

1-3, 5, 7. Unchanged from the fourth/fifth amendments' own traces above — none of these concern
   the `Positive` batch-anchor's capture timing.
4/6. Companion-`Positive`-vs-primary-timeout, now closed at the CORRECT anchor: the companion's
   batch's `error_set_seq` snapshot is taken before that batch's own `PassThruReadMsgs` call even
   starts, so any bump landing after this pre-read snapshot — including one landing in the
   `read_messages`-returned-to-`build_cll_rx_entries`-runs gap the fifth amendment's own capture
   point did not close — is caught: `entry.set_seq_at_read` still holds the PRE-bump value,
   mismatching the LIVE `error_set_seq` at apply time, so the stale `Positive` is discarded and
   the queue correctly stays suspended.

### Second addendum (Codex-review fix, gate narrowing): the bypass must key off the item's OWN recovery-worthiness, not its variant alone

**The bug.** The addendum above's `recovery_bypass` (`dispatch_tx_item`'s siphon gate) and
`drain_tx_held_backlog`'s matching middle-pop fallback both exempted **every**
`TxItem::UpdateParam` from the FIFO no-overtake clause while `tx_suspended_by_error` was set —
not only one whose own queued snapshot actually clears `CP_SuspendQueueOnError`. An unrelated
`CoptUpdateparam` (its own call-time `params` snapshot, ADR-067, leaves `CP_SuspendQueueOnError`
still enabled/nonzero) got the identical bypass even though it does nothing to recover the
queue — overtaking an older, client-submitted-earlier held transmitting item to apply unrelated
hardware parameters or promote the `UniqueRespIdTable`, for zero recovery benefit.

**The fix.** Both sites now gate the bypass on the item's own queued `params` snapshot reading
the policy disabled: `TxItem::UpdateParam { params, .. }` with the added guard
`!params.suspend_queue_on_error()` (`ComParamSet::suspend_queue_on_error`, `service.rs`).
Concretely, `dispatch_tx_item`'s siphon gate is now

```rust
let recovery_bypass = link.tx_suspended_by_error
    && matches!(&item, TxItem::UpdateParam { params, .. } if !params.suspend_queue_on_error());
```

and `drain_tx_held_backlog`'s middle-pop search predicate narrows identically. A non-recovery
`UpdateParam` sitting in `tx_held` now stays FIFO-blocked like any other non-transmitting item
under error-suspend — it is no longer eligible for the middle-pop fallback either, once it is no
longer the recovery item.

**Correct by construction — no live re-check needed.** The bypass's other conjunct
(`link.tx_suspended_by_error`) already handles the only case that could otherwise require one: if
a *different*, concurrent `UpdateParam` already promoted `CP_SuspendQueueOnError` to `0`, that
promotion's own clear of `tx_suspended_by_error` runs in the same critical section, on the same
serial per-channel poll task (see the addendum above's reachability argument), strictly before
any later dispatch attempt on that channel. So by the time this item's own bypass predicate is
evaluated, `tx_suspended_by_error` is already `false` and the item takes normal FIFO
treatment — there is no window where a stale `true` reading and an already-cleared suspension
coexist, and thus no double-bypass risk from evaluating only the item's own frozen snapshot.

**No new deadlock.** A non-recovery `UpdateParam` correctly stays FIFO-parked behind an older
held transmitting item: it genuinely cannot clear the suspension, so holding it recreates no
self-referential wait (unlike the original addendum's bug, where the ONLY item capable of
recovery was itself blocked on its own release). A genuine recovery `UpdateParam` queued later
still gets its own independent bypass evaluation at ITS OWN dispatch attempt and executes
normally, clearing the suspension and draining the backlog — including any earlier-parked
non-recovery `UpdateParam`s, in FIFO order.

**Dependency on `apply_bustype_lock`'s substitution scope (ADR-110).** `apply_bustype_lock` can
substitute some of an `UpdateParam`'s queued `params` keys with the CLL's pre-call Active values
under a sibling's held `LOCK_PHYSICAL_COM_PARAMS`, but only for `PDU_PC_BUSTYPE`-class keys.
`CP_SuspendQueueOnError` is ERRHDL-class, not BUSTYPE-class, so this substitution never touches
it, and the bypass predicate above stays exact today. This is a documented dependency, not an
incidental fact: a future widening of `apply_bustype_lock`'s substitution class to cover
ERRHDL-class keys would let a sibling's lock silently rewrite an `UpdateParam`'s recovery-eligibility
after the snapshot was captured, and must re-derive this gate's correctness before shipping.

**Documented "bypass reordering" quirk.** If a non-recovery `UpdateParam` (older, still parked)
and a recovery `UpdateParam` (newer, bypasses and executes first) are both queued, the recovery
item's promotion happens first, but the older non-recovery item's promotion — once drained —
happens after and overwrites the newer promotion's Active set wholesale, including re-enabling
`CP_SuspendQueueOnError` back to `1` in the final Active state. This only re-enables the *policy*;
nothing re-triggers `tx_suspended_by_error` retroactively (it is only ever set at the two trigger
sites above, never as a side effect of a promotion), so no re-suspension occurs from this
overwrite alone — a fresh trigger (timeout or unhandled negative) is required to suspend again.
This is an instance of the existing accepted "bypass reordering" consequence this ADR's addendum
already documents below; this fix strictly narrows that class (fewer `UpdateParam`s ever overtake
at all now) rather than introducing a new hazard.

### Seventh amendment (Codex-review fix r3680112304, design-advisor mechanism): eager publish-before-exposure for `Suspend`

**The bug.** The `wrote_suspend` block (`poll_rx_inner`) captured `entry.suspend_seq` from the
live `error_clear_seq` but did not itself publish `link.tx_suspended_by_error`; that write only
happened later, in the end-of-pass reconciliation loop, AFTER the triggering frame had already
been delivered to the client via `deliver_or_enqueue`. Delivery is genuinely live (ADR-115's
`live_sender` opportunistically drains `rx_buf` the moment the item is pushed), so any task
reacting to that delivery — a companion UUDT channel's independent poll task (ADR-046) on a
dual-channel CLL, or, just as reachable on a single-channel CLL, the gRPC-handler task servicing
the client's own reactive `SendMsg`/`StartComPrimitive` call — could call `dispatch_tx_item` and
observe `tx_suspended_by_error == false` before this same pass's own end-of-pass loop ever ran,
dispatching a transmitting item the parameter was supposed to hold. Framing this as a
dual-channel-only hazard (Codex's original wording) undersells it: the window exists for any CLL,
because `dispatch_tx_item` always runs on a task independent of whichever poll task delivered the
frame.

**The fix.** The `wrote_suspend` block now publishes `link.tx_suspended_by_error = true` in the
SAME critical section that captures `entry.suspend_seq`, strictly before `deliver_or_enqueue`
exposes the frame:

```rust
if wrote_suspend {
    let mut links = ctx.logical_links.lock().await;
    if let Some(link) = links.get_mut(&entry.handle) {
        entry.suspend_seq = Some(link.error_clear_seq);
        if link.connect_generation == entry.connect_generation
            && link.active.suspend_queue_on_error()
        {
            link.tx_suspended_by_error = true;
        }
    }
}
```

The gate is `connect_generation` freshness plus a live re-check of `CP_SuspendQueueOnError`
policy — deliberately NOT a sequence-number comparison. That omission is not a shortcut: the
value `entry.suspend_seq` would be compared against (`link.error_clear_seq`) is read from the
exact same live link, in this exact same critical section, two lines above this write — comparing
it back to itself would be vacuous. This eager gate is precisely `queue_error_class_to_apply`'s
`Suspend` arm minus that vacuous seq check.

**Why no seq bump.** Deliberately, this write does not bump `error_clear_seq` (or `error_set_seq`).
A bump would break the end-of-pass loop's existing same-batch last-frame-wins handling: if a LATER
frame in this same batch classifies `Positive` for the same CLL, the end-of-pass loop must still
be able to apply it via the unchanged `entry.set_seq_at_read == Some(link.error_set_seq)` check —
bumping here would stale out that same-batch check against itself. The two SET-direction paths
(this eager write, and the receive-phase timeout hook) are asymmetric on purpose: the timeout
hook's bump is correct because its evidence genuinely postdates the batch already read; a frame
folded earlier in the SAME batch as a later `Positive` does not postdate it.

**Why no wake.** This write is false → true only (suspending); the wake-worthy true → false
transition belongs exclusively to the end-of-pass loop's `Positive` arm, which is unchanged and
remains the only place a `TxItem::ResumeWake` is sent from this mechanism.

**Interaction with the end-of-pass loop (unchanged, still authoritative).** The end-of-pass loop
re-applies `Suspend`/`Positive` exactly as before this amendment. Re-applying `Suspend` a second
time (once eagerly, once at end-of-pass) is idempotent. If an explicit resume lands between the
eager write and the end-of-pass loop, that resume's own clear-site bump of `error_clear_seq`
(unconditional, per the third amendment) makes `entry.suspend_seq != link.error_clear_seq` at
end-of-pass, so `queue_error_class_to_apply` correctly discards — no re-suspension of a
just-resumed CLL. A second `wrote_suspend` firing later in the same batch (a different registrant
on the same CLL also classifying `Suspend`) re-derives `entry.suspend_seq` from the then-live
`error_clear_seq` and re-asserts `true`; if an explicit clear raced in between the two firings,
the second firing correctly treats it as a fresh incident and re-suspends, matching this ADR's
existing "an explicit clear predating a frame's exposure is not a race, it's a genuinely new
incident" reasoning (Decision section, second correction).

**Interaction with reconnect.** `finalize_connected_link` unconditionally resets
`link.tx_suspended_by_error = false` in the same critical section that stamps a fresh
`connect_generation` (`rpc_link.rs`). Any eager write from a stale pass targeting the old
generation is gated out by this write's own `connect_generation` check, and cannot survive into
the new generation regardless.

**No live re-check of `deliver_or_enqueue`'s own delivery against `connect_generation`.**
Edge-case-hunter review flagged, as a pre-existing and broader property of `poll_rx_inner` (not
introduced or widened by this amendment): delivery itself is not re-checked against live
`connect_generation` for most registrant shapes (only the narrow eager-cyclic-deadline-confirm
path does), and `rx_buf` is the same `Arc` reused across a reconnect on the same `cll_handle`, so
a frame snapshotted before a reconnect could in principle still be delivered into the new
session's event stream. This affects all frame delivery, not specifically
`CP_SuspendQueueOnError` classification, and this amendment's own gate does not misapply
`tx_suspended_by_error` across generations either way — out of scope for this fix; recorded in
`j2534-0404-service/docs/implementation-notes.md`'s backlog for a future look.

**Tests.** Two tests added (`queue_error_suspend.rs`): one reacts to a delivered suspend-worthy
frame with a fresh transmitting COP with no settle delay and asserts it stays held; the other
confirms a same-batch `[Suspend, Positive]` sequence still resumes with a wake by end of pass.
Neither test is a genuine fail-before/pass-after discriminator for the eager write's own
contribution — this crate's `current_thread` `#[tokio::test]` runtime never yields mid-pass when
uncontended (confirmed by edge-case-hunter via revert-and-rerun), so the whole pass, including the
end-of-pass reconciliation, always completes synchronously before any other task could react,
regardless of this fix. No delay/synchronization-injection hook exists in this suite's mock
backdoor to force the interleaving deterministically, and a multi-threaded test runtime
(`#[tokio::test(flavor = "multi_thread", ...)]`, used once elsewhere in the workspace) was
considered and rejected as still non-deterministic for this purpose. Both tests are kept as
steady-state pins (they would catch a regression that pushed either write behind a genuine
`.await`); the fix's correctness was verified instead by direct code inspection (the eager write
sits two statements before `deliver_or_enqueue`, under the same lock guard, no `.await` between)
and independently re-derived by both a design-advisor pass and an edge-case-hunter pass.

### Eighth amendment (Codex-review fix r3680291021): `ioctl_resume_tx_queue` gated on `connect_generation`

**The bug.** `ioctl_resume_tx_queue` (`rpc_misc.rs`, `PDU_IOCTL_RESUME_TX_QUEUE`) captures
`channel_key`/`tx_queue` in one `logical_links` acquisition, then clears
`tx_suspended_by_ioctl`/`tx_suspended_by_error` (via `clear_error_suspension`) and sends a
`TxItem::ResumeWake` through that captured `tx_queue` in a SECOND, later acquisition — necessarily
separate acquisitions because `tx_queue` is read from `shared_channels`, which must never nest
inside `logical_links` (ADR-080). A same-`cll_handle` disconnect and reconnect completing in the
gap between the two would clear the NEW session's suspend flags while sending the wake through the
OLD session's `tx_queue` — stranding anything the new session had already parked, with the flags
now (incorrectly) reading resumed.

**The fix.** `connect_generation` is captured alongside `channel_key` in the first acquisition and
re-checked against the live value in the second; a mismatch skips both the flag clears and the
wake send entirely. This is the same freshness-gate shape already used by every other explicit
clear site in this mechanism (see the `CoptUpdateparam`-promotion path, `events.rs`, which checks
`connect_generation` before its own `clear_error_suspension` call, in the same critical section as
the check — confirmed, while investigating this finding, to already be correctly gated and not to
share this bug). `ioctl_resume_tx_queue` was the one site where the freshness capture and the
clear/wake landed in genuinely different critical sections, because of the `shared_channels`
lookup in between — that structural difference is what let this one site diverge from the
otherwise-consistent pattern.

**Tests.** No new regression test: this race requires interleaving two concurrent RPC calls (this
ioctl vs. a reconnect) across the exact gap between two `logical_links` acquisitions in
`ioctl_resume_tx_queue`, and — same limitation as the seventh amendment's tests — this harness has
no hook to force that interleaving deterministically (the two acquisitions are separated by a real
`.await` on `shared_channels`, but nothing in this test suite can pause a specific in-flight RPC
call at that exact point to inject a concurrent reconnect). Verified instead by direct code
inspection (the `same_connection` check and both actions it gates are computed and consumed inside
one uninterrupted `logical_links` critical section, with no `.await` between the check and the
mutation — confirmed by an independent `edge-case-hunter` pass) and by the full existing suite
staying green (908 tests).

**Accepted residual — `tx_suspended_by_ioctl` is not reset by `finalize_connected_link` on
(re)connect, unlike `tx_suspended_by_error`.** Discovered by the `edge-case-hunter` pass that
reviewed this amendment. `finalize_connected_link` (`rpc_link.rs`) resets
`link.tx_suspended_by_error = false` on every connect (this ADR's own hygiene reset, documented in
the Decision section above) but never touches `tx_suspended_by_ioctl`. Per ISO 22900-2 (2022)
§8.5.4/§8.5.5 (Tables 44-45), `PDU_IOCTL_SUSPEND_TX_QUEUE`/`RESUME_TX_QUEUE` carry no
CLL-not-connected return code, and this service's handlers apply no connected-state guard either —
a client may legitimately call `PDU_IOCTL_SUSPEND_TX_QUEUE` on a disconnected-but-still-`Created`
CLL. A disconnect DOES clear `tx_suspended_by_ioctl` (`cancel_link_cops` →
`cancel_held_tx_items(reset_suspended = true)`), but a subsequent connect does not re-clear it, so
a suspend issued in the disconnected window leaves a freshly (re)connected session starting life
already suspended, for a suspension no live client action ever targeted. Independent of this
amendment's own fix (the `same_connection` gate above is correct regardless), but a mismatch
no-op on `ioctl_resume_tx_queue` can now surface this pre-existing asymmetry more visibly than
before, since the new session is no longer incorrectly "rescued" by a stale resume call that used
to clear the flag unconditionally. Not fixed here — recorded in
`j2534-0404-service/docs/implementation-notes.md`'s backlog.

**Not extended to `PDU_IOCTL_RESET`'s `cancel_held_tx_items(..., true)` call — CONFIRMED separate
bug, deliberately deferred, not merely undetermined.** `ioctl_reset` (`rpc_misc.rs`) snapshots
`targets` (including each CLL's identity) in one `logical_links` acquisition, then performs several
further `.await`s per target (`api.lock()`, filter teardown, buffer clears) before reaching its
`cancel_held_tx_items(cll_handle, true)` loop — and that helper, unlike this amendment's fix, has
NO `connect_generation` check at all: it acts on whatever `LogicalLinkState` is live for
`cll_handle` at the time it runs, not the session RESET was actually snapshotting. A same-handle
disconnect+reconnect completing in that window means `ioctl_reset` drains and CANCELS
(`PduCopstCancelled`, delivered live) the RECONNECTED session's own already-queued `tx_held` items,
and resets that new session's suspend flags — an initial characterization of this call site (this
amendment's first draft) wrongly waved this off as safe because "RESET sends no wake, so nothing
can be misdirected"; an `edge-case-hunter` pass confirmed a concrete repro and correctly identified
that reasoning as incomplete — the hazard here is destructive cancellation of live, unrelated work
and a spurious cancellation notification to a client that never asked for it, not merely a
misdirected wake. This is confirmed, not speculative, and is a real user-visible correctness/data-
loss issue on the reconnected session. Deliberately NOT fixed in this PR: `ioctl_reset` operates at
module scope across every CLL on the module, and whether module-wide `PDU_IOCTL_RESET` should be
generation-scoped per CLL (matching this amendment's fix) or is intentionally meant to act on
"whatever is live right now" is a genuine design question this fix did not resolve — recorded in
`j2534-0404-service/docs/implementation-notes.md`'s backlog for a `design-advisor`-routed follow-up.
**Resolved by ADR-161**, which generation-scopes `ioctl_reset` across all three of its post-snapshot
phases (not only `cancel_held_tx_items`).

## Alternatives Considered

1. **Reuse `tx_suspended_by_ioctl`.** Rejected: a stray positive response would silently clear a
   client's own explicit `PDU_IOCTL_SUSPEND_TX_QUEUE`.
2. **Reuse `tx_suspended_by_lock`.** Rejected: `recompute_lock_tx_suspensions`'s from-scratch
   sweep would clobber error-suspend state on any unrelated sibling lock transition.
3. **Classify unhandled negatives inside `wait_for_expected_response_inner`'s own receive-phase
   loop, alongside the timeout hook, instead of at bind time.** Rejected: that loop only ever
   sees the single COP it is waiting on: an unhandled negative response bound to a *different*
   registrant on the same CLL, or one that reaches no registrant at all (an otherwise-unbound
   frame), would never be observed there. `bind_registrant`'s tier-1 scan is the only place that
   sees every content frame against every registrant's `rc_cfg`, so it is the only correct place
   to classify.
4. **Delegate `is_unhandled_negative` to `detect_pending_rc` and treat its `None` result as
   "unhandled."** Rejected: `detect_pending_rc` returns `None` both when the RC engine is fully
   disabled (`any_enabled() == false`, this ADR's actual target case) and when a frame simply
   isn't a negative response at all — those two cases must NOT be conflated for suspend
   purposes, and delegating would.
5. **(Sixth amendment) Make `error_set_seq` a lock-free atomic, read from outside the
   `logical_links` critical section, instead of a pre-read snapshot under that lock.** Rejected:
   `error_set_seq`'s bump and `tx_suspended_by_error = true`'s own set are currently paired
   inside ONE `logical_links` critical section (the timeout hook's own set-before-expose block).
   An atomic read from outside that section could observe the counter bump and the flag write as
   separately-ordered events, since nothing would enforce their combined visibility together once
   the counter is no longer protected by the same lock as the flag it sequences against. That
   trades a lock-discipline argument — checkable by inspection, matching every prior amendment's
   own style — for a memory-ordering argument, a categorically harder property to verify correct,
   and exactly the kind of change that tends to open a NEW round on this mechanism rather than
   close it. The pre-read capture, plain mutex discipline throughout, is the terminal, verifiable
   shape for this anchor.

## Consequences

- **Accepted residual — tester-present bypass.** The same class of residual ADR-123 (citing
  ADR-081) and ADR-081 already accept for the other two suspend sources: this service's
  tester-present dispatch runs entirely outside the `tx_suspended`/`tx_held` path, so
  `CP_SuspendQueueOnError` does not suspend it either. Not newly introduced by this ADR.
- **Accepted residual — `rc_byte_offset < 2` framings are unclassifiable.** Same class as
  ADR-100's own accepted residual for `detect_pending_rc`: this service does not know those
  protocols' negative-response shape well enough to guess an equivalent gate. The timeout hook
  still covers those protocols; only bind-time NRC classification is affected. Extended
  (post-merge review round, Codex + design-advisor): the same decline-to-classify treatment
  now also covers a tier-1-bound, `0x7F`-led frame that `is_unhandled_negative` returns `false`
  for a reason OTHER than "genuinely not a negative response" — specifically, an NRC that can't
  be attributed to this registrant's own request (its SID doesn't echo the registrant's
  `request_sid`, so it's actually a negative response to a different, unrelated request that
  happened to bind this registrant via a broad/vacuous expected-response descriptor) or a
  truncated `7F <sid>` too short to contain a byte at `rc_byte_offset` at all. Both cases were
  previously misclassified `Positive`, wrongly clearing `tx_suspended_by_error` and releasing
  the held backlog on a negative response that was never actually confirmed genuine. Same safe
  direction as the tier-2 residual above: a missed auto-resume, never a wrong one. One
  consequence worth calling out: on an `rc_byte_offset < 2` protocol, a tier-1-bound GENUINE
  positive response that happens to start with byte value `0x7F` no longer auto-resumes either
  — `is_unhandled_negative` declines to classify for that protocol regardless of payload
  content, so this decline-to-classify path fires purely on the leading byte, same accepted
  tradeoff, extended to a case the original bullet didn't call out.
- **Accepted residual — companion-pass classification races are last-writer-wins overwrites, not
  merged.** Unlike the registrant fields `merge_registrant_writeback` handles, `queue_error_class`'s
  end-of-pass apply is a plain conditional overwrite: `Suspend`/`Positive` has no commutative merge
  the way `matches_got` (summed delta) or `cyclic_deadline` (monotone max) do, because its meaning
  is the order of events, and two independent poll tasks (primary channel + UUDT companion,
  ADR-046) share no frame-arrival clock. If both passes classify the same CLL in the same window,
  the pass whose critical section lands last wins — by writeback scheduling, not frame
  chronology — so the flag can transiently reflect the chronologically earlier frame in either
  direction. Accepted because: the window is one poll pass; the state self-corrects on the next
  classified frame or timeout; and the client's explicit recovery actions
  (`PDU_IOCTL_RESUME_TX_QUEUE`/`CoptUpdateparam`-to-`0` — not `PDU_IOCTL_CLEAR_TX_QUEUE` alone,
  which cancels the backlog but does not itself resume dispatch) remain available. A cross-channel
  ordering fix would require carrying the classifying frame's device
  timestamp and a per-link high-water mark — deliberately not built; revisit only on field
  evidence. Neither of the fifth amendment's split counters (`error_clear_seq`/`error_set_seq`)
  is bumped by a classification APPLY, for either direction — specifically to keep this residual
  unchanged: bumping on apply would re-key this already-accepted last-writer-wins outcome to
  apply order instead of evidence order, turning today's accepted "last writeback wins" overwrite
  into an outright discard of whichever pass's apply happened to run second — both passes would
  have captured the same pre-bump seq (`Suspend`) or the same pre-bump batch-read value
  (`Positive`), so the first apply would land normally and its own counterfactual bump would then
  invalidate the second pass's now-stale capture — a strictly worse outcome for the same accepted
  residual (a silent loss instead of a well-understood overwrite), not a fix for it. See the
  governing rule in the fourth correction's own subsection above. **Extended (seventh amendment):**
  the eager mid-batch `Suspend` publish added by the seventh amendment is also a participant in
  this same last-writer-wins dynamic — it can land, from one pass, in between a companion pass's
  own eager write and that companion pass's end-of-pass reconciliation, with the usual outcome
  (later critical section wins, self-corrects on the next classified frame or timeout). This does
  not widen the residual: the eager write only ever sets `true`, so two eager writes from
  concurrent passes for the same CLL never conflict with each other, and the end-of-pass loop
  remains the sole source of a `false` transition (and thus the sole source of a wake) — the
  accepted last-writer-wins shape described above is unchanged, just with one more writer sharing
  it on the `true` side.
- **Accepted residual — narrowed by the second correction (capture-at-fold sequencing) to a single
  remaining ambiguity: an explicit action landing AFTER a Suspend-worthy frame was exposed but NOT
  actually in reaction to it.** The two pass-granular discard windows the FIRST correction (the
  pass-level epoch) could not distinguish — "the frame the client reacted to" vs. "an unrelated,
  later fresh frame in the same pass" — are both closed by fold-time capture (see the trace in the
  Decision section's second-correction discussion above). What remains is narrower and structural,
  not pass-granular: once `entry.suspend_seq` is captured at the exact moment a frame's
  classification folds `Suspend`, and that frame is then exposed to the client, this mechanism has
  no way to tell "the client's next explicit clear-suspension action is a genuine reaction to THIS
  frame" apart from "the client's next explicit clear-suspension action is unrelated, and just
  happens to land after this frame's exposure" — exposure-ordering is the finest available signal
  without reading client intent, and both cases discard the classification identically. Scope
  limits, restating the existing mitigating argument (unchanged by this correction): the loss is
  usually not permanent — a tier-1 receive phase that saw the discarded NRC and never receives a
  subsequent positive still ends in a receive timeout, whose set-before-expose hook re-suspends
  independently of this mechanism; the incident is permanently lost only when the unhandled negative
  ALSO matches the COP's own permissive expected descriptor and completes the phase as a successful
  match. Frame-granular exposure ordering shared across the delivery and action paths (distinct from
  the fold-time capture this correction already adds) is the known complete fix for this LAST
  ambiguity, judged disproportionate to this now doubly-narrow shape; this bullet is the durable
  record of that deferral.
- **Accepted residual — the `Positive` batch-anchor (sixth-amendment correction, superseding the
  fourth/fifth amendments' own framing of this same residual) means a genuinely non-racing
  recovery `Positive` can also be discarded as stale, with no equivalent self-correcting
  mechanism to the Suspend-side residual above, and a WIDER discard window than either a
  (never-correct) fold-time capture or the fifth amendment's own construction-time capture would
  give.** Before the third correction, `Positive` classifications applied unconditionally (no seq
  check), so a positive response arriving in the same pass window as an intervening timeout bump
  would always clear the suspension — even though, per the trace above, that is exactly the case
  that must correctly discard (the positive's own evidence predates the timeout it would
  otherwise silently override). The batch anchor closes this correctly, but this residual's own
  framing needed one further correction: earlier rounds described the anchor as "the batch's own
  read time, fixed before any frame in it is processed" and treated that as equivalent to "at (or
  before) the batch's own read-COMPLETION" — those are NOT the same thing (see the sixth
  amendment subsection above for the full argument), and the fifth amendment's actual capture
  point (`build_cll_rx_entries` construction time, strictly AFTER `read_messages` returns) did not
  achieve the read-completion anchor it was believed to. The sixth amendment's pre-read capture
  achieves the true, sufficient inequality (`T_capture <= T_read-start <= T_read-completion`)
  instead of an unreachable equality, at a deliberately wider discard radius than exact
  read-completion anchoring would give: any `Positive` from a batch whose PRE-READ snapshot
  predates a bump is discarded, even if that bump lands after the read itself already completed
  (i.e., even later than the fourth/fifth amendments' own, already-wider-than-fold-time radius) —
  this is not narrower than necessary, it is the necessary width for what is actually achievable
  across two different async locks (see the sixth amendment's own reasoning for why exact
  read-completion equality is structurally unreachable). The cost: a `Positive` classification
  whose containing batch's pre-read snapshot was taken just before an unrelated bump (a client's
  own explicit action, or a DIFFERENT COP's timeout on the same CLL) is discarded even when it was
  a completely legitimate resume signal; unlike the Suspend-side ambiguity above, this has no
  independently documented self-healing path — a discarded `Positive` does not automatically
  retry itself the way a discarded `Suspend` gets re-armed by its own COP's eventual receive
  timeout. The client's explicit recovery actions (`PDU_IOCTL_RESUME_TX_QUEUE`/
  `CoptUpdateparam`-to-`0` — `PDU_IOCTL_CLEAR_TX_QUEUE` alone cancels the backlog but does not
  itself resume dispatch) remain the fallback if no later, non-racing
  positive response — from a batch whose pre-read snapshot was taken at or after the bump — ever
  lands cleanly enough to apply. Accepted because the discard window is bounded by one poll pass's
  own pre-read snapshot, and correctness (never resuming on evidence that does not provably
  postdate the suspending event on the wire) is prioritized over availability (a resume
  opportunity lost to an unlucky race) for a mechanism whose entire purpose is holding traffic
  until a trustworthy positive response is confirmed.
- **Addendum — accepted FIFO deviation for the recovery `CoptUpdateparam`.** A `CoptUpdateparam`
  may now execute ahead of one or more error-held transmitting items that were queued before it —
  those items then run, in their own original relative order, only after the promotion clears
  `tx_suspended_by_error`. Deliberate: those transmitting items could not have run until *some*
  recovery action landed anyway (that is the definition of "held"), so the recovery item running
  first, rather than never, is a pure improvement with no reachable case where a client-visible
  ordering guarantee is broken by it — unlike a blanket non-transmitting bypass, which could
  invert two *client-submitted, order-significant* items against each other (see the addendum
  above). **Second-addendum quirk:** if an OLDER non-recovery `UpdateParam` was also parked
  (unblocked, not bypassed, since the gate-narrowing fix above no longer exempts it), it still
  promotes after the recovery item once drained via normal FIFO, and its own — still
  policy-enabled — snapshot wholesale-overwrites the recovery promotion's Active set, including
  re-enabling `CP_SuspendQueueOnError` back to `1`. This never retroactively re-triggers
  `tx_suspended_by_error` (only the two trigger sites ever set it), so no re-suspension occurs
  from the overwrite alone; see the second addendum for the full argument.
- **Addendum — the prior test suite encoded the deadlock as a workaround, not a fix; now
  corrected.** `rc78_disabled_suspends_and_resumes_via_update_param` previously issued a manual
  `PDU_IOCTL_CLEAR_TX_QUEUE` before its recovery `CoptUpdateparam`, with its own comment
  documenting the deadlock this addendum closes, rather than exercising the real client recovery
  flow. It has been rewritten to queue the recovery `CoptUpdateparam` directly behind the held
  transmitting COP, with no workaround, and to assert the held COP itself goes on to dispatch.
- **Structural-argument-only — the drain-side UpdateParam middle-pop (including its
  gate-narrowing search predicate from the second addendum above) and the
  `recompute_lock_tx_suspensions` wake-widening have no deterministic regression test, and none is
  constructible in this harness.** The dispatch-side recovery bypass covers every deterministically
  reachable stranding: each channel has one serial poll task, and both error setters (the
  receive-phase timeout hook and the end-of-pass classification apply) are sequenced before any
  subsequent dispatch attempt on that channel, so a recovery `CoptUpdateparam` reaching the siphon
  after an error always sees the flag — verified by ordering analysis across the ioctl, lock,
  J1850-autodetect-retroactive-lock, and receive-only-COP variants (a J1850 CLL has no companion
  task to break the serialization). The two residual stranding paths are (a) a UUDT-companion
  pass's classification writeback landing after the main task has siphoned the `CoptUpdateparam`
  under a sibling's lock, and (b) the Fix-F concurrent-suspend `push_front` window — both
  unforceable without a production pause hook (ADR-123 Findings A/E/G precedent). Both fixes close
  these windows by construction; spurious wakes are no-ops through the drain gate. A full-suite
  pass with either hunk reverted is therefore expected and is not evidence the hunk is dead code; a
  future PR adding an error setter that does NOT run at a poll-task phase boundary must re-verify
  this argument. A deterministic test of the autodetect *retroactive suspension itself* (CLL B's
  flavor resolution siphons CLL A's next transmitting COP; unlock drains it) is constructible if the
  mock harness gains probe-outcome injection for SAE J1850 autodetect — offered as a follow-up in
  `j2534-0404-service/docs/implementation-notes.md`'s backlog, not built here. This mechanism's own
  remaining ambiguity (the "Accepted residual — narrowed by the second correction..." bullet above)
  is the same class of unforceable-without-a-production-pause-hook timing window as (a)/(b) here —
  a genuinely concurrent "client reacts to an exposed frame before this pass's own end-of-pass
  writeback runs" interleaving has no natural preemption point in this harness's single-threaded
  test runtime, matching this bullet's own precedent (and ADR-086 rounds 11/13's identical
  observation for a different race class); the third amendment's own new sub-case (the first
  suspend-worthy frame of an episode) is unforceable for the identical structural reason — see
  `reconnect_does_not_inherit_prior_sessions_error_suspension`'s doc comment
  (`tests/grpc_mock/queue_error_suspend.rs`) for the specific note on that sub-case. No
  deterministic regression test was attempted for either; `suspend_applies_when_generation_seq_and_policy_all_match`
  and `suspend_discarded_on_seq_mismatch` (`events.rs`, `queue_error_class_to_apply_tests`) instead
  pin the pure apply-time decision this mechanism reduces the race to, directly. The
  companion-`Positive`-vs-primary-timeout race (first identified by the third correction, closed
  correctly only by the fourth correction's batch-read anchor) is the same class of
  structurally-unforceable end-to-end scenario — see that same test's doc comment for the fifth
  amendment's own note, and `positive_discarded_when_batch_was_read_before_a_timeout_bump`/
  `positive_applies_when_batch_was_read_at_current_error_set_seq` (`events.rs`,
  `queue_error_class_to_apply_tests`) for the pure decision this round's scenario reduces to. **The
  two-mechanism reality (fifth amendment):** this bullet's structural-unforceability argument now
  applies independently to BOTH direction-specific anchors, not one shared mechanism — a
  genuinely concurrent "companion pass folds/reads a classification while the primary channel's
  own explicit-clear or timeout lands in the same window" interleaving has no natural preemption
  point in this harness's independently-ticking, unpaused-timer poll tasks for either `Suspend`
  (`error_clear_seq`) or `Positive` (`error_set_seq`) alone, and no interleaving forces both at
  once either — the pure-function tests above cover each direction's own truth table completely,
  which is why no new end-to-end integration test was added for the fifth amendment despite it
  splitting the mechanism in two.
- **Accepted residual — a granted physical-channel lock can retroactively cover an already-live
  foreign transmission.** `LockResource`'s busy check (§9.4.13.2 b)) is grant-time-only, keyed on
  the acquiring CLL's *then-current* resource id. A not-yet-connected SAE J1850 CLL can acquire
  `LOCK_PHYSICAL_TX_QUEUE` before its flavor resolves; `autodetect_sae_j1850_flavor`'s later
  `recompute_lock_tx_suspensions` call folds in any other CLL that now shares the resolved resource
  without re-running the busy check, so a CLL with an already-executing transmitting COP the busy
  check would have rejected at grant time can end up lock-suspended anyway, bounded by that COP's
  own lifetime. Same class as ADR-123's own accepted residuals (suspension is self-healing; in-flight
  work is never revoked): fixing it means either revoking a granted lock retroactively or re-running
  the busy check inside autodetect, disproportionate to a window bounded by one COP's lifetime.
- **Held COPs report `PDU_COPST_IDLE`, unchanged.** Per ADR-117: a siphoned, not-yet-dispatched
  TxItem was never marked executing, so `GetStatus` continues to report Idle for it exactly as
  it already does for the ioctl/lock suspend sources.
- **Auto-resume requires a live positive RX.** Absent a positive response arriving on the wire
  (typically via a tier-2/monitor registrant, since the original COP that triggered the
  suspension has itself already finished), only two explicit client actions resume dispatch:
  `PDU_IOCTL_RESUME_TX_QUEUE`, or a `CoptUpdateparam` demoting `CP_SuspendQueueOnError` to `0`.
  `PDU_IOCTL_CLEAR_TX_QUEUE` is NOT a resume action — it only cancels currently held items
  (matching its own pre-existing `reset_suspended = false` contract) and leaves
  `tx_suspended_by_error` set, so any later transmitting COP is held again too; a client using it
  to recover must still follow up with one of the two actions above.
- **Not implemented, deliberately out of scope:** `CP_RepeatReqCountApp` (same inert-stub state
  as `CP_SuspendQueueOnError` was before this ADR, not requested here); any
  `vci-service-interface`/proto change (`GetComParam`/`SetComParam` already carry this ComParam
  ID generically, needing no schema change); a new `PDU_IT_INFO`-style callback announcing the
  suspend/resume transition (ISO 22900-2 defines none for this mechanism, matching ADR-123's own
  accepted residual for the analogous §9.4.13.3 use-case-4 callback).
- **Cross-reference — `bind_registrant`/`bind_frame` signature unified with ADR-148's concat
  merge.** ADR-148 (`CP_EnableConcatenation`) landed on `main` concurrently with this ADR's own
  merge and independently widened the same `bind_registrant`/`bind_frame` call sites. The two
  changes were reconciled when ADR-148's branch merged `main`: `bind_registrant`'s return widened
  to a 4-tuple carrying both the concat `absorbed` flag and this ADR's `QueueErrorClass`
  classification (computed via a new `classify_queue_error` helper at every accepted-frame return
  site, including concat continuation-fast-path absorbs), and `poll_rx_inner` applies this ADR's
  `tx_suspended_by_error` publish-before-exposure write ahead of concat's own batch delivery for
  the same polled frame. See ADR-148's Amendment 11 for the full reconciliation design and
  rationale.
- Test file added: `j2534-0404-service/tests/grpc_mock/queue_error_suspend.rs` — originally 8
  tests covering a timeout-triggered suspend (CAN-family) held until an explicit
  `PDU_IOCTL_RESUME_TX_QUEUE`; an unmapped-NRC-triggered suspend (KWP-family) draining two
  FIFO-held COPs via a later positive response bound to an independent tier-2 monitor; a mapped
  NRC (`0x78`) with its `CP_RCxxHandling` disabled suspending and resuming via `CoptUpdateparam`;
  the same NRC with handling enabled NOT suspending (no regression to `rc_handling.rs`'s existing
  RC78 auto-handling); `CP_SuspendQueueOnError = 0` (default) never suspending;
  `PDU_IOCTL_CLEAR_TX_QUEUE` cancelling a held item without itself resuming dispatch; error-
  suspend composing independently with a sibling's `LOCK_PHYSICAL_TX_QUEUE`; and a
  non-transmitting `CoptUpdateparam` still executing while error-suspended. The correction round
  above added 3 more, one per bug it fixed: a tester-present negative reply, with an unrelated
  tier-1 registrant's `request_sid: None` `rc_cfg` present on the same CLL, causing no
  suspension; a vacuous-descriptor-only registrant still suspending the queue on a SID-echoing
  unmapped-NRC `0x7F` response; and, on an already-suspended CLL, a tier-2 monitor matching a
  `0x7F`-led payload leaving it suspended while a tier-2 monitor matching a genuine positive
  payload still resumes it (the pre-existing auto-resume path, unregressed).
- **Addendum test changes:** `rc78_disabled_suspends_and_resumes_via_update_param` rewritten to
  drop the `PDU_IOCTL_CLEAR_TX_QUEUE` workaround (see above); 2 new regression tests added —
  `ioctl_suspend_then_error_suspend_drains_fully_via_explicit_resume` (a transmitting item and a
  recovery `CoptUpdateparam` held together under `PDU_IOCTL_SUSPEND_TX_QUEUE`, error-suspend then
  also triggers, and an explicit `PDU_IOCTL_RESUME_TX_QUEUE` drains both correctly — exercises the
  ioctl gate's own unconditional block staying intact, though tracing shows it resolves via the
  pre-existing front-pop arm rather than the new middle-pop fallback, since
  `PDU_IOCTL_RESUME_TX_QUEUE` clears both suspend sources in the same critical section) and
  `delay_does_not_overtake_held_transmitting_item_under_error_suspend` (a `CoptDelay`, standing in
  for the brief's originally-proposed `CoptStopcomm` per the reachability note in the addendum
  above, does not overtake an already-held transmitting item — guards against a future
  simplification back to a blanket non-transmitting bypass). A third proposed regression
  (the lock-then-error-then-unlock stranding scenario) was found structurally infeasible to
  construct deterministically with this harness and was not added; see the accepted verification
  gap bullet above.
- **Second-addendum test changes (gate narrowing):** one new regression test added,
  `unrelated_update_param_stays_fifo_blocked_then_recovery_drains_in_order` — queues a
  non-recovery `CoptUpdateparam` (its own snapshot leaves `CP_SuspendQueueOnError` enabled)
  behind an already-held transmitting item and asserts both stay blocked (the discriminating
  assertion: reverting the gate-narrowing predicate back to matching any `UpdateParam` makes this
  fail), then queues a genuine recovery `CoptUpdateparam` and asserts the completion order
  (recovery item, then the transmitting item, then the non-recovery item, drained via normal
  FIFO) — pinning the "older non-recovery snapshot promotes last and overwrites the policy value"
  quirk documented above. The pre-existing `rc78_disabled_suspends_and_resumes_via_update_param`
  (a genuine recovery item) is unaffected by the narrowed predicate and continues to pass
  unchanged. The drain-side middle-pop's own narrowed predicate remains structural-argument-only,
  per the residual bullet above — no test exercises that path specifically.
- **Fail-open-fix (first correction) test changes, since INVERTED by the second correction below --
  kept here as an accurate historical record of that round, not of current behavior:** two unit
  tests were added, co-located immediately after `LogicalLinkState::clear_error_suspension`
  (`service.rs`, `clear_error_suspension_tests`, mirroring `events.rs`'s
  `queue_error_class_to_apply_tests` location convention) — one cell each of the (flag was `true` /
  flag was already `false`) truth table, asserting the epoch bumped (and the helper returned `true`)
  only in the former case. **This assertion is now the OPPOSITE of current behavior** (the second
  correction below restores an unconditional bump); the same two tests were rewritten in place,
  same location and cell-per-truth-table-row convention, to assert the bump is unconditional in
  BOTH cells — see the second correction's own test-changes bullet below. One new integration test
  added, `reconnect_does_not_inherit_prior_sessions_error_suspension`
  (`j2534-0404-service/tests/grpc_mock/queue_error_suspend.rs`) — error-suspends a CLL via timeout
  (no explicit resume issued), disconnects it still suspended, reconnects the same `cll_handle`, and
  asserts a freshly queued transmitting COP dispatches immediately, pinning the observable outcome
  of the "a fresh session never inherits a prior session's error suspension" invariant end to end.
  On its own this integration test cannot distinguish `finalize_connected_link`'s connect-time
  reset from `cancel_link_cops`'s pre-existing disconnect-time clear, since its graceful
  disconnect-before-reconnect flow runs both before the reconnect completes. A second, deterministic
  white-box unit test, `finalize_connected_link_resets_prior_sessions_error_suspension`
  (`j2534-0404-service/src/service/rpc_link.rs`, co-located with the existing
  `ensure_uudt_companion_channel_rejects_when_module_marked_not_avail` unit test), isolates the
  connect-time reset directly: it inserts a `LogicalLinkState` with `tx_suspended_by_error = true`
  straight into `logical_links` and calls `finalize_connected_link` on it with no disconnect in the
  picture at all, so `cancel_link_cops`'s clear never runs and only `finalize_connected_link`'s own
  reset can account for the flag reading `false` afterward — no racy interleaving is needed to
  isolate this contribution, since the two clearing sites are reachable independently rather than
  only through one shared racy window. Both of these two tests are UNCHANGED by the second
  correction (per that correction's own scope: it does not touch the connect-time reset) and remain
  green against current code.
- **Second-correction (capture-at-fold sequencing) test changes.** `clear_error_suspension_tests`
  (`service.rs`) rewritten in place: `bumps_seq_and_returns_true_when_flag_was_suspended` and
  `bumps_seq_and_returns_false_when_flag_was_not_suspended` now both assert `error_action_seq`
  advances (the bump is unconditional again) — the second test's own name and assertion are the
  direct inversion of the first correction's `leaves_epoch_unchanged_and_returns_false_when_flag_was_not_suspended`
  it replaces. `queue_error_class_to_apply_tests` (`events.rs`) rewritten to construct
  `entry_suspend_seq: Option<u64>`/`live_error_action_seq: u64` instead of a pair of pass-snapshot
  epochs, and gained `positive_applies_when_generation_matches_regardless_of_seq` (the discriminating
  cell for "`Positive` needs no seq check at all") and `suspend_discarded_when_fold_seq_was_never_captured`
  (the defensive `entry_suspend_seq == None` cell); `positive_discarded_on_epoch_mismatch` was
  removed, since `Positive` no longer has a seq to mismatch. A new unit test,
  `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend` (`events.rs`, `bind_frame_tests`),
  pins the fold-capture timing directly: it drives `bind_frame` through a Suspend fold, a Positive
  fold, a bound-but-declined-to-classify fold, a second Suspend fold, and an unbound/unremarkable
  frame, asserting `bind_frame`'s `wrote_suspend` out-param is `true` on exactly the two Suspend
  folds and `false` on every other call — the "wrong implementation" shape (re-capturing on any
  call that merely leaves `queue_error_class == Some(Suspend)` true, rather than only the call that
  actually wrote it) is exactly what this test's Frame 5 case would catch. No new integration test
  was added reproducing this round's Codex finding (the first-suspend-worthy-frame-of-an-episode
  race) end-to-end — see `reconnect_does_not_inherit_prior_sessions_error_suspension`'s doc comment
  (`tests/grpc_mock/queue_error_suspend.rs`) for why that interleaving is structurally unforceable
  in this harness, matching the extended structural-argument-only bullet above.
- **Third-correction (unified `error_state_seq`) test changes.** `queue_error_class_to_apply_tests`
  (`events.rs`) rewritten for symmetric gating: every existing cell now passes a
  `live_suspend_queue_on_error: bool` sixth argument (the fourth-amendment restructure that folded
  `Suspend`'s live-policy check into this function), `suspend_applies_when_generation_and_seq_both_match`
  is renamed `suspend_applies_when_generation_seq_and_policy_all_match`, and
  `positive_applies_when_generation_matches_regardless_of_seq` is replaced by
  `positive_applies_regardless_of_live_policy_value` (seq is no longer a case where `Positive` gets a
  free pass — only the live-policy argument still is). New cells added: `positive_discarded_on_seq_mismatch_after_a_timeout_bump`
  (directly models the bug scenario's seq arithmetic — a companion fold's captured seq vs. a
  timeout's bump — and is the discriminating case a version of this function that still exempted
  `Positive` from the seq check would fail), `positive_applies_when_captured_after_a_timeout_bump`
  (the genuine-recovery direction, confirming the new gate does not regress it),
  `positive_discarded_when_fold_seq_was_never_captured` (symmetric with the existing `Suspend`
  defensive cell), and `suspend_discarded_when_live_policy_is_off` (pins the folded-in policy check
  directly). `bind_frame_tests`' fold-capture-timing test,
  `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend`, is renamed
  `wrote_class_flag_set_on_the_frame_that_folds_either_classification` and its Frame 2 assertion is
  INVERTED: a `Positive` fold now must report `wrote_class = true` (previously asserted `false`),
  matching the generalized capture. `LogicalLinkState::error_state_seq`'s own doc comment and
  `clear_error_suspension`'s are updated in place (unchanged behaviorally) to note the timeout hook's
  new role as a fifth, SET-side bump site; `clear_error_suspension_tests` itself is unaffected (its
  own behavior did not change). No new integration test was added reproducing the
  companion-`Positive`-vs-primary-timeout race end-to-end — see
  `reconnect_does_not_inherit_prior_sessions_error_suspension`'s doc comment
  (`tests/grpc_mock/queue_error_suspend.rs`) for the fourth round's own structural-unforceability
  note, matching the extended structural-argument-only bullet above.
- **Fourth-correction (split direction-specific anchors, fifth amendment) test changes.**
  `LogicalLinkState::error_state_seq` splits into `error_clear_seq`/`error_set_seq`;
  `clear_error_suspension_tests` (`service.rs`) is renamed field-for-field
  (`error_state_seq` → `error_clear_seq`) with no behavioral change to what it pins (the bump
  stays unconditional). `CllRxEntry::class_seq` splits into `suspend_seq: Option<u64>`
  (Suspend-only, same fold-time capture site) and `set_seq_at_read: u64` (new, populated once per
  pass in `build_cll_rx_entries`). `bind_frame`'s `wrote_class` out-param reverts to
  `wrote_suspend`, Suspend-only again; `bind_frame_tests`'
  `wrote_class_flag_set_on_the_frame_that_folds_either_classification` is renamed
  `wrote_suspend_flag_set_only_on_the_frame_that_folds_suspend` and its Frame 2 assertion is
  RE-INVERTED back to the second correction's original shape: a `Positive` fold must NOT report
  `wrote_suspend = true` (undoing the third correction's own inversion of this same assertion).
  `queue_error_class_to_apply` gains two new parameters (`entry_set_seq_at_read`/
  `live_error_set_seq`, replacing the single shared seq pair for the `Positive` branch);
  `queue_error_class_to_apply_tests` (`events.rs`) is rewritten accordingly: `Suspend` cells carry
  over essentially unchanged (renamed to the split fields) plus one new cross-check,
  `suspend_ignores_error_set_seq_and_set_seq_at_read` (confirms `Suspend` is indifferent to the
  `Positive`-only counter pair). `Positive` cells are entirely rewritten for batch-anchor
  semantics: `positive_discarded_on_seq_mismatch_after_a_timeout_bump` and
  `positive_applies_when_captured_after_a_timeout_bump` are removed (the fold-time-seq mechanism
  they pinned for `Positive` no longer exists) and replaced by
  `positive_discarded_when_batch_was_read_before_a_timeout_bump` — THE critical discriminating
  test: `entry_set_seq_at_read` set to a value simulating a batch read before an intervening
  timeout, `live_error_set_seq` bumped once to simulate that timeout, with no fold-time parameter
  involved at all, confirming the classification is discarded purely on the batch-read/live
  mismatch — and `positive_applies_when_batch_was_read_at_current_error_set_seq` (the
  genuine-recovery direction). `positive_discarded_when_fold_seq_was_never_captured` is removed
  (no longer applicable — `Positive` has no `Option`-valued fold-time seq to be absent);
  `positive_ignores_error_clear_seq_and_suspend_seq` is added as `Positive`'s own cross-check.
  `LogicalLinkState::error_clear_seq`/`error_set_seq`'s own doc comments,
  `clear_error_suspension`'s, `CllRxEntry::suspend_seq`/`set_seq_at_read`'s, `bind_frame`'s, the
  timeout hook's bump site, and `queue_error_class_to_apply`'s are all updated in place to state
  the new direction-specific anchors and add the EXPOSURE-anchor code comment at the `Suspend`
  capture site (`poll_rx_inner`) design-advisor's ruling requires, so a future reviewer does not
  re-file the `Suspend` half of this round's Codex finding. No new end-to-end integration test was
  added — see the "two-mechanism reality" addition to the structural-argument-only bullet above.
- **Fifth-correction (pre-read capture for the batch anchor, sixth amendment) test changes.**
  `CllRxEntry::set_seq_at_read` changes type from `u64` to `Option<u64>`; `build_cll_rx_entries`
  gains a new parameter, `set_seq_snapshot: &HashMap<u32, u64>`, and stamps `set_seq_at_read` from
  it instead of re-reading `LogicalLinkState::error_set_seq` live. Two new unit tests,
  `present_cll_is_stamped_from_the_snapshot_not_live_state` and `absent_cll_is_stamped_none`
  (`events.rs`, new `build_cll_rx_entries_tests` module), pin this stamping logic directly against
  a supplied snapshot map — a present handle stamps `Some(the snapshotted value)`, deliberately
  DIFFERENT from a `minimal_link()` fixture's own live `error_set_seq`, proving the function reads
  the snapshot and not live state; an absent handle stamps `None`, the "connected mid-window"
  case. `queue_error_class_to_apply`'s `entry_set_seq_at_read` parameter changes from `u64` to
  `Option<u64>`; every existing `Positive` cell in `queue_error_class_to_apply_tests` (`events.rs`)
  is updated to pass `Some(seq)` instead of a bare integer, and a new cell,
  `positive_discarded_when_set_seq_at_read_is_none`, pins the `None`-always-discards rule
  directly — confirming this holds even when `live_error_set_seq` happens to equal the "obvious"
  default `0` a naive `unwrap_or(0)` might have produced, so the discard is provably
  conservative-by-construction rather than a coincidental match. As with every prior round on this
  mechanism, the actual cross-task interleave this round closes (a companion channel's own poll
  task, or this channel's own timeout hook, bumping `error_set_seq` between the pre-read snapshot
  and the read's own completion) remains structurally unforceable deterministically in this
  harness — no new end-to-end integration test was attempted; see the structural-argument-only
  bullet above, now updated to reference this round's pre-read-capture argument instead of the
  superseded "captured before any frame is processed" framing (see point 6's correction in the
  fifth correction's own subsection above for exactly what was wrong with that framing).

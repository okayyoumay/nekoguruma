# ADR-173: Correct SAE J2534-2 Repeat Messaging `Condition` Semantics (Inverted in ADR-165)

**Date:** 2026-08-12
**Status:** Accepted
**Affects:** `j2534-0404-mock/src/lib.rs` (`RepeatSlot`, `note_rx_frame_for_repeat_slots`,
             `spawn_repeat_worker`, `advance_repeat_slot_windows`, the
             `IOCTL_QUERY_REPEAT_MESSAGE` handler), `j2534-0404-service/src/service/rpc_misc.rs`
             (`ioctl_start_repeat_message`, `ioctl_query_repeat_message`, the
             `LOCK_PHYSICAL_TX_QUEUE` staleness check), `j2534-0404-service/tests/grpc_mock/
             repeat_message.rs`, [ADR-165](ADR-165-j2534-2-repeat-messaging-phase12.md)
             (Context's `Condition` paraphrase, Decision 6, and the condition-0 carve-outs from
             its rounds 7/8/17 — partially superseded, see its amended Status line; Decisions
             1-5 remain in force)

## Context

A SAE J2534-2 (DEC2020) already-implemented-phases conformance audit
(`j2534-0404-service/docs/implementation-notes.md`'s "SAE J2534-2 (DEC2020) already-implemented-
phases conformance audit" section) found that `START_REPEAT_MESSAGE`'s `Condition` parameter is
implemented backwards relative to clause 14.2.2.1 — and the inversion isn't a narrow code bug, it
is baked into [ADR-165](ADR-165-j2534-2-repeat-messaging-phase12.md) itself. ADR-165's own Context
paraphrase (and its Decision 6) describe `Condition = 0` as ignoring received traffic entirely and
`Condition = 1` as stopping on a match or on silence. The real clause 14.2.2.1 requirement is the
opposite pairing: `Condition = 0` (`REPEAT_MESSAGE_UNTIL_MATCH`) keeps retransmitting through
silent intervals and stops only on a matching received frame; `Condition = 1`
(`REPEAT_MESSAGE_WHILE_MATCH`) keeps retransmitting only while every received frame matches, and
stops on the first non-matching frame *or* on a silent interval — two independent stop triggers.
Clause 14.2.1 additionally requires every slot's mask/pattern to be evaluated against all incoming
traffic, before the channel's own message filters apply.

This misreading survived roughly 20 subsequent PR review rounds because each round audited its own
fix against ADR-165's own (wrong) paraphrase rather than re-reading clause 14.2.2.1 directly — the
spec's own value names (`REPEAT_MESSAGE_UNTIL_MATCH`/`REPEAT_MESSAGE_WHILE_MATCH`) appear nowhere
in this repository. The concrete consequence: `j2534-0404-mock`'s `Condition = 0` worker never
evaluates received traffic at all (runs until a client explicitly stops it), and its
`Condition = 1` worker transmits exactly once and then waits for a single match-or-timeout —
neither path is a genuine repeating retransmission loop, and neither honors the real stop rule for
its own condition value. The service layer (`rpc_misc.rs`) cemented the same misreading with three
special-cased "condition 0 never evaluates mask/pattern" carve-outs added across rounds 7, 8, and
17: response-header resolution, mask/pattern length-equality validation, and template size-range
validation are all skipped for `Condition = 0`.

One part of the original audit finding does not hold up under direct code review: it claimed the
D-PDU mask/pattern composition ADR-165 Decision 3 established (prepending a wire-scoped,
header-exact-match template ahead of the client's own `mask_data`/`pattern_data`) was built for the
inverted semantics and would not transplant cleanly to the real one. Direct reading of
`rpc_misc.rs`'s composition (and confirmation against clause 14.2.1's own "evaluated against all
incoming traffic" framing) shows the opposite: Decision 3's mechanism is unaffected by which
condition value triggers which stop rule, and remains correct as-is. This ADR does not change it.

This is also a design decision that resolves half of an already-tracked P2 finding in the same
audit (SAE J2534-2 clause 14.2.2.3's `MsgId`-release rule, and Table 53's QUERY status polarity):
the state-machine rewrite this ADR makes necessarily touches the same self-termination code path,
so the mock-side half of that P2 is folded in here rather than requiring the same slot-lifecycle
code to be restructured twice. The P2's remaining service-side residuals, and the audit's separate
P3 findings (Repeat Messaging discovery-constant wiring, an unrelated pin-bitmap discovery issue),
are unaffected and stay in the backlog as already recorded.

## Decision

1. **Corrected `Condition` semantics, evaluated per-frame at arrival, plus one per-interval
   silence check for `Condition = 1` only.** A received frame's masked-and-patterned outcome
   (match vs. non-match) is decided immediately when it arrives — clause 14.2.2.1's stop triggers
   are tied to the reception event itself, not deferred to an interval boundary. `Condition = 0`:
   any matching frame, at any point, ends the slot; a non-matching frame or a silent interval never
   does. `Condition = 1`: any non-matching frame, at any point, ends the slot; additionally, an
   interval elapsing with **no** frame received at all also ends it — this silence check resets
   every interval (a match in interval *N* does not immunize interval *N+1*; `TimeInterval` is
   itself defined per transmission-to-transmission window, so silence is naturally a per-window
   property). A frame this service already treats as ineligible for repeat-slot evaluation (the
   existing `REPEAT_INELIGIBLE_RX_STATUS_MASK` gate — loopback echoes, TX indications) counts as
   neither a match, a non-match, nor a "received" frame for the silence check — including the
   worker's own transmit echo, so a `Condition = 1` slot on an otherwise-silent bus still
   terminates on silence rather than being kept alive by its own loopback.

2. **`j2534-0404-mock`'s worker becomes one unified loop for both conditions**, replacing the two
   separate (and non-repeating) branches. `RepeatSlot`'s single `matched: bool` is replaced with
   `terminated: bool` (the terminal state, set either by `note_rx_frame_for_repeat_slots` on a
   per-frame trigger or by the worker on a silence timeout) and `rx_seen_this_interval: bool`
   (cleared at each transmit, set on any eligible received frame, consulted only by `Condition = 1`
   at the interval deadline). `note_rx_frame_for_repeat_slots` now evaluates every non-terminated
   slot regardless of `condition` (previously gated to `condition == 1` only). The existing
   lock-free-fast-check-then-locked-re-check epoch discipline (established across ADR-165's own
   rounds 8 and 21) carries over unchanged to every mutation site.

3. **A terminated slot is retained, not removed, until an explicit `STOP_REPEAT_MESSAGE`** — this
   resolves the mock-side half of the already-tracked P2: clause 14.2.2.3 requires `MsgId` to
   survive self-termination, and Table 53 defines QUERY status as 1 (in progress) vs. 0 (ceased,
   `MsgId` still valid), not "still exists vs. `ERR_INVALID_MSG_ID`" as the prior
   worker-removes-itself model implemented. The mock's QUERY handler now reports 1 for a live slot,
   0 for a terminated-but-unstopped one, and `ERR_INVALID_MSG_ID` only once genuinely removed by
   `STOP_REPEAT_MESSAGE`. The QUERY handler must itself advance the slot's windows (Decision 7)
   before deriving this status, not just read `terminated` as a stale snapshot. The service's own
   `rpc_misc.rs` forwards this verbatim and needs no change to the QUERY path itself — but
   `prune_stale_repeat_message_ids`/the
   `LOCK_PHYSICAL_TX_QUEUE` staleness check must now treat status 0 as "not actively transmitting,
   does not block the lock grant" **without** pruning the `MsgId` from `repeat_message_ids` (the
   claim stays valid, just inactive, until an explicit `STOP`) — the existing
   `ERR_INVALID_MSG_ID`-triggers-a-prune rule is unaffected and stays correct for genuinely-gone
   slots.

4. **All three condition-0 carve-outs (rounds 7/8/17) reverse to unconditional.** Response-header
   resolution, mask/pattern length-equality validation, and template size-range validation now
   apply identically to both conditions — after this fix, the two conditions' `START` validation
   path is identical except for forwarding the `Condition` value itself; only the device-side
   continue/stop rule differs. **Client-visible behavior change:** a `Condition = 0` link with no
   resolvable response header, previously accepted (since the old code never needed to evaluate
   mask/pattern for that condition), is now rejected the same way `Condition = 1` already correctly
   rejects one — a currently-accepted configuration whose own stop criterion could never actually
   evaluate correctly against wire frames now fails fast instead of silently never stopping.

5. **Decision 3's mask/pattern composition (wire-scoped, header-exact-match) is reaffirmed
   unchanged** — see Context above; it was already correct and is now simply applied
   unconditionally per Decision 4, not redesigned.

6. **`Condition == 1`'s per-interval silence timeline is grid-anchored, not reset at each
   transmit** (Codex review, PR #60 round 3, design-advisor consult, amending this ADR after two
   prior review rounds' patches to the same code proved insufficient). The straightforward
   implementation of Decision 1's silence check — clear `rx_seen_this_interval` and recompute the
   next deadline as `Instant::now() + TimeInterval` every time the worker actually transmits —
   re-anchors the timeline to whenever the worker thread happens to wake, not to a fixed
   TimeInterval-aligned grid. A worker delayed by more than one full `TimeInterval` (a severe
   scheduler stall) then silently skips every intervening interval's silence check instead of
   evaluating it, since the recomputed deadline is always in the future relative to "now"
   regardless of how many boundaries were actually crossed while the worker was stalled. The fix:
   `interval_deadline` always holds the end of the *earliest* window still owed a silence
   evaluation, and a single routine advances it by whole `TimeInterval` steps from wherever it
   already sits — never by re-anchoring to `Instant::now()` — closing and evaluating every elapsed
   window along the way (a multi-window stall is fast-forwarded in O(1) rather than looped, since
   any window that had eligible traffic would already have advanced the stored deadline past it
   when that traffic arrived, so every window this routine finds still elapsed is provably silent).
   This routine is the single place both `Condition == 1`'s silence stop-trigger and the routine
   window-close/reset of `rx_seen_this_interval` happen, invoked when an eligible frame arrives,
   when the worker wakes, and (Decision 7) when a client calls QUERY, so stop-condition evaluation
   is exact regardless of which of the three notices a boundary first or how late any of them
   notices it. Before the slot's first transmit, no window has logically started yet, so a frame
   arriving in that gap credits no window (rather than being wrongly attributed to "interval 1").

7. **`IOCTL_QUERY_REPEAT_MESSAGE` is the routine's third caller, not just an observer of the other
   two's work** (Codex review, PR #60 round 4, found immediately after Decision 6 landed). A QUERY
   landing in the gap between a `Condition == 1` window's deadline and either `note_rx_frame_for_
   repeat_slots` or `spawn_repeat_worker` next noticing it would otherwise read a stale
   `terminated == false` and report status 1 (live) for a slot that has, in fact, already gone
   silent — and the service's `prune_stale_repeat_message_ids`/`LOCK_PHYSICAL_TX_QUEUE` staleness
   check (Decision 3) would then wrongly keep blocking a lock grant on it, since it trusts exactly
   this status. The QUERY handler now calls `advance_repeat_slot_windows` under the same lock,
   before deriving the status it returns, closing this gap the same way the other two call sites
   already did.

## Consequences

- A `Condition = 1` slot on a shared or noisy bus now terminates on the first pre-filter frame that
  doesn't match its own scoped template — spec-correct per clause 14.2.1's "all incoming traffic"
  rule, but a real behavior change from the prior (never actually repeating) implementation, and
  one worth flagging to any future reader as intentional rather than a regression.
- `ADR-165`'s Status line is annotated at the granularity of its actually-wrong parts (Context's
  `Condition` paraphrase, Decision 6, the rounds-7/8/17 carve-outs) — its remaining Decisions
  (1-5: the overall `REPEAT_MSG_SETUP`/`MsgId` plumbing, native IOCTL forwarding rather than
  service-side reimplementation, Decision 3's mask/pattern composition, etc.) remain in force
  unchanged.
- **Accepted residual:** the audit's separately-tracked P2 (service-side implications beyond the
  mock-side self-termination fix here) and P3s (Repeat Messaging discovery-constant wiring, an
  unrelated pin-bitmap discovery issue) are unaffected by this fix and remain open in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog.
- **Process lesson, worth recording explicitly**: this misreading survived ~20 review rounds
  because every round validated a fix against ADR-165's own paraphrase of the spec rather than the
  spec clause itself — a foundational-document paraphrase error is not self-correcting under
  normal incremental review, since each round's own scope is implicitly bounded by trusting the
  ADR it's building on. A conformance audit that re-reads the primary spec text end-to-end,
  independent of any existing ADR's framing, is what caught this; the existing per-phase
  conformance-audit practice this repo already runs is the right mechanism, not a gap to close.
- Decision 6's grid-anchored mechanism replaces two prior same-PR patches to the identical code
  (an `interval_deadline`-gate-only fix, then a carry-over-flag fix layered on top of it) that each
  addressed a narrower symptom of the same underlying re-anchoring bug without eliminating its
  root cause — a reminder that a third same-symptom review finding on the same code is itself a
  signal to look for a wrong invariant rather than patch again narrowly, which is what prompted the
  design-advisor consult that produced Decision 6.

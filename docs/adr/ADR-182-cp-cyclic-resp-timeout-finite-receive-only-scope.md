# ADR-182: Widen `CP_CyclicRespTimeout` and Tier-2 Detachment to Finite-`N` Created-Receive-Only COPs

**Date:** 2026-08-18
**Status:** Accepted (narrowly supersedes ADR-100 Decision §4's `-1`-only `CP_CyclicRespTimeout`
scope and ADR-100's Out of scope Stage 3 inline-execution boundary, for the created-receive-only
finite-`N > 0` subfamily specifically)
**Affects:** `j2534-0404-service` service, service/events, service/service_params, docs
(RPC_API_GUIDE, GLOSSARY, j2534-0404-architecture, IMPLEMENTATION_NOTE)

## Context

### The 2009-vs-2022 textual shift

[ADR-100](ADR-100-cop-registry-two-tier-binding.md) Decision §4 scoped `CP_CyclicRespTimeout`'s
governing role to a created-receive-only (`NumSendCycles == 0`, ADR-059) registrant with
`NumReceiveCycles == -1` only. That scope was grounded in ISO 22900-2:2009(E)'s RECEIVE ONLY
text at the time (`vehicle-comm-specs/iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md:661-662,748,5923`),
which described the `-1` (infinite) case as the one governed by this ComParam and left the
finite-`N` receive-only case with no completion mechanism of its own besides reaching its
count or an explicit client cancel.

ISO 22900-2:2022 moved this anchor. Table 6's closing rows
(`vehicle-comm-specs/iso22900-2-2022/ISO_22900-2_2022(en).md:678`) describe the RECEIVE ONLY
family — `NumSendCycles == 0`, any `NumReceiveCycles` — as finishing on reaching its target
count, OR, when `CP_CyclicRespTimeout` is configured, erroring out on that timeout's expiry;
with the timeout at its default of `0` (disabled) for every protocol preset, the row describes
no other timeout mechanism governing this family at all — in particular, not `CP_P2Max`. Table
9's RECEIVE ONLY NOTE 2 (`:756`) and the ComParam's own Annex B description (`:6282`) both
extend `CP_CyclicRespTimeout` to `NumReceiveCycles > 0`, not only `-1`, corroborating Table 6's
reading. This service's current `CP_P2Max`-derived timeout for the finite-`N` case was
therefore never actually 2009-conformant either — the 2009 text described the finite receive-only
case as ending on count or explicit cancel, not on any timeout — the `CP_P2Max` behavior was an
extrapolation from the SEND-AND-RECEIVE family's own timeout mechanism that nobody re-checked
against the RECEIVE ONLY family's own (silent-on-timeout) 2009 text.

This is specific to the RECEIVE-ONLY family (`NumSendCycles == 0`). SEND-AND-RECEIVE COPs
(`NumSendCycles != 0`) are unaffected: the 2022 text's own SEND AND RECEIVE row keeps their
finite receive timeout `CP_P2Max`-governed, unchanged. `NumReceiveCycles == -2` (IS-MULTIPLE)
receive-only is also unaffected — neither Table 6 nor Annex B names a `CP_CyclicRespTimeout`
role for it, and it keeps its existing `CP_P2Max`-governed collect-until-window-closes shape.

### Why swapping the deadline source alone is not enough — the same defect class ADR-100 already condemned for `-1`

ADR-100's own Context section documented why the pre-ADR-100 `-1` (IS-CYCLIC) shape could not
simply keep its `CP_P2Max` window and run inline: an unbounded, inline receive hold wedges the
owning poll task, blocking every other ComPrimitive queued on the same physical channel or a
sibling CLL sharing it. That is exactly the shape a finite-`N` created-receive-only COP would
be pushed into by literally swapping its deadline source from `CP_P2Max` to
`CP_CyclicRespTimeout` while leaving it inline: `CP_CyclicRespTimeout` defaults to `0`
(disabled) on every protocol preset, so a quiet-bus finite-`N` monitor with default ComParams
would legitimately never receive its target count — and, running inline, would wedge the poll
task indefinitely, exactly the defect class ADR-100 fixed for `-1` by detaching it into tier 2
at creation instead of leaving it inline. The fix here is therefore the same shape ADR-100
already used for the `-1` subtype: widen the *detached* tier-2 lifecycle to also cover finite
`N > 0`, rather than attempting to keep it inline under a different deadline source.

## Decision

### 1. Widen the detach gate

`wait_for_expected_response`'s gating flag (renamed `is_comparam_timed_receive_only`, was
`is_receive_only_cyclic`) widens from `wait.created_receive_only && wait.num_receive_cycles ==
-1` to:

```
wait.created_receive_only && (wait.num_receive_cycles == -1 || wait.num_receive_cycles > 0)
```

Both the `cyclic_timeout_ms`/`cyclic_deadline` population on the `CopRegistrant` AND the
immediate `ReceivePhaseOutcome::DetachedToTier2` return are gated on this single, widened flag
— a finite-`N` created-receive-only COP now detaches to tier 2 immediately at creation, exactly
like the `-1` subtype already did, instead of running inline through
`wait_for_expected_response_inner`. `NumReceiveCycles == -2` (IS-MULTIPLE) created-receive-only
is deliberately excluded from this widening — it stays tier-2-from-creation (unchanged, ADR-100
round-9 Finding-1) but keeps running inline/blocking, `CP_P2Max`-governed, exactly as before
this ADR; neither Table 6 nor Annex B names a `CP_CyclicRespTimeout` role for it, and ADR-100's
own Out of scope Stage 3 boundary remains in force for this specific subfamily (see that
section's own note, updated by this ADR).

### 2. `CP_CyclicRespTimeout` replaces `CP_P2Max` entirely for the widened family, armed at creation

For the widened family (`-1` and finite `N > 0`), `CP_CyclicRespTimeout` is the sole
completion-timing mechanism — `CP_P2Max` is never consulted for it at all, matching how the
`-1` subtype already worked and closing the gap this ADR's Context section describes for
finite `N`. The deadline is armed at COP creation, not at first accepted match — this is a
deliberate interpretation choice, not a literal reading of Annex B's "timer enabled after the
first positive response" wording. Arming at first match would make a permanently-quiet bus
(the timeout-`0`-by-default, no-match-ever case) indistinguishable from an explicitly disabled
timeout — since neither would ever arm — silently defeating Table 6's own stated
count-or-timeout caveat for a chatty-then-silent target rather than a permanently silent one.
Arming at creation instead makes `CP_CyclicRespTimeout == 0` mean exactly what Table 6 and the
`-1` subtype's own shipped behavior already establish it to mean: "no deadline at all, ends
only by count/cancel/hard-error" — the same disabled-by-default reading the `-1` subtype has
used since ADR-100 Stage 1, now extended verbatim to finite `N` rather than reinvented for it.
No change to the `-1` subtype's own arm point (still at creation) — this ADR only extends the
same, already-shipped mechanism to the newly-widened finite-`N` shape.

The deadline restarts on every accepted match via the SAME mechanism the `-1` subtype's own
restart-on-match already uses (`bind_registrant`'s unconditional `if let Some(ms) =
r.cyclic_timeout_ms.filter(|&ms| ms > 0) { r.cyclic_deadline = Some(now + ms) }`, keyed only on
whether `cyclic_timeout_ms` is populated, not on which `NumReceiveCycles` shape produced it) —
no new restart logic was written; widening `cyclic_timeout_ms`'s population at creation is
sufficient for this shared mechanism to cover the new family automatically.

### 3. Completion becomes reap-driven via the maintenance sweep, with count-completion checked first

`reap_expired_cyclic_registrants` (the existing per-tick sweep, called from
`run_due_tick_duties` and, per the ADR-095 amendment, every other long poll-task hold) now
checks TWO independent reap conditions for a tier-2 (`RegistrantTier::ReceiveOnly`) registrant,
in this order per registrant:

- **(a) Count-completion, checked FIRST:** `tier == ReceiveOnly &&
  matches_needed.is_some_and(|n| matches_got >= n)` (`is_receive_only_count_complete`, new) →
  `PduCopstFinished`, no error event. No watermark/soundness gate — unlike time-based expiry,
  a monotonically-incrementing count that has reached its target is unconditionally a sound,
  legitimate finish; `matches_got` is already fully merged across a CLL's primary/UUDT
  companion channels by the existing `merge_registrant_writeback` (ADR-101 Decision §A) every
  poll pass. This condition is new — the `-1` subtype's `matches_needed` is always `None`
  (unbounded), so it can never satisfy this arm; `-1`'s own behavior is completely unaffected
  by adding it.
- **(b) Expiry, checked SECOND** (the existing `cyclic_deadline`-expiry mechanism,
  `is_cyclic_deadline_expired` + ADR-101 §E's `is_cyclic_reap_sound` watermark gate, both
  reused verbatim/unchanged): for a finite-`N` registrant (`matches_needed.is_some()`), expiry
  now ADDITIONALLY emits `PduErrEvtRxTimeout` and applies ADR-147's `CP_SuspendQueueOnError`
  hook (the same classify-and-bump-`error_set_seq` shape used by every other finite-count
  receive-phase timeout in this file, near the end of
  `wait_for_expected_response_inner`) BEFORE transitioning to `PduCopstFinished` — per Table
  6's RECEIVE ONLY row, an expiry with the target count still unmet is this family's error
  path. For `-1` (`matches_needed.is_none()`), expiry stays FINISHED-only, completely
  unchanged — `-1` has no target count to fall short of, so there is nothing for a timeout to
  be an error about, exactly as ISO 22900-2 §9.2.6.3.4 RECEIVE ONLY NOTE 1 already described
  for it.

A registrant satisfying both conditions in the same tick (its target count reached exactly as
its deadline also happens to expire) reaps via count-completion only, never reported as a
timeout — count-completion is checked first specifically to guarantee this.

ADR-101 §E's per-channel drain-watermark soundness scheme for expiry reaping is reused
verbatim, with no logic change — its own argument (every channel that could still deliver this
registrant a match has been exhaustively drained past the deadline) generalizes to the widened
finite-`N` family exactly as it already covered `-1`.

## Consequences

- **Client-visible behavior change, deliberately called out (not silent), mirroring ADR-100 §5's
  own precedent for its unbound-frame-discard flip:** a finite-`N` created-receive-only COP on a
  quiet bus, with every protocol preset's default `CP_CyclicRespTimeout = 0`, now runs until its
  target count is reached or the client cancels it — it no longer errors out with
  `PduErrEvtRxTimeout` after one `CP_P2Max` window. This must be called out in release notes and
  `docs/rpc-api-guide.md` (done in this same PR).
- Finite-`N` created-receive-only monitors no longer block/wedge the owning poll task — they
  free their physical channel for other queued ComPrimitives immediately at creation, the same
  benefit ADR-100 already delivered for the `-1` subtype.
- **Count-completion `PDU_COPST_FINISHED` now has up to one maintenance-tick
  (`POLL_INTERVAL_MS`-bounded, 10ms plus the nearest injected-hook latency per the ADR-095
  amendment) of latency instead of being immediate** — it is reap-driven, not synchronous with
  the completing match. A regression test
  (`receive_only_finite_n_count_completion_latency_bounded_by_poll_interval`) asserts this
  latency stays within a generous bound.
- **Accepted residual, recorded not fixed (separate, adjacent finding, out of scope for this
  ADR):** ISO 22900-2:2022 Table 6's closing row forbids the `NumSendCycles == 0,
  NumReceiveCycles == -2` combination, but this service currently accepts and executes it
  (Table 5 and Table 6 disagree with each other on this point in the 2022 text itself). Tracked
  as a new backlog entry in `j2534-0404-service/docs/implementation-notes.md`'s Prioritized
  Backlog, citing this ADR's audit round; not implemented here.
- **Accepted residual, recorded not fixed (separate, adjacent finding, out of scope for this
  ADR):** ADR-100 Decision §4's own already-deferred "migrated (tier-1→tier-2) IS-CYCLIC COPs
  should also get `CP_CyclicRespTimeout`" follow-up remains open. The 2022 SEND AND RECEIVE
  text states more plainly than the 2009 text did that a migrated tier-2 registrant's
  completion semantics are not automatically identical to a created-receive-only one's — a
  slightly stronger textual anchor for eventually revisiting this follow-up. Recorded as an
  update to ADR-100 Decision §4's own text and to the existing backlog item covering this
  follow-up; not implemented here.
- **Accepted ordering, Codex review round (PR #78 follow-up fix), design-advisor-traced:**
  `CancelComPrimitive` racing the maintenance reap (`reap_expired_cyclic_registrants`) in the
  gap between that RPC's two separate lock acquisitions (looking up `primitives`, then marking
  `cancelled_cops` under `logical_links`) can let the reap fully finish the same COP first. The
  client-visible outcome of that ordering is exactly the already-documented ADR-128
  already-terminal no-op-success outcome (success, plus whatever terminal event the reap itself
  emits — FINISHED, or FINISHED preceded by the genuine `PduErrEvtRxTimeout` and its
  `CP_SuspendQueueOnError` side effect if an expiry is what triggered the reap) — not a
  silently-overridden cancel, since the cancel's own real linearization point is the
  `cancelled_cops` insert, and cancellation of an in-flight item is already documented as
  best-effort. The one genuine defect this ordering exposed — a `cancelled_cops` entry left
  behind with nothing left alive to remove it — is closed by a two-sided cleanup: a self-check in
  `rpc_cancel_com_primitive` right after its own mark-and-defer insert, and a late drain in
  `reap_expired_cyclic_registrants`'s own terminal-emission call site, both routed through a
  shared helper (`drain_cancelled_cop_if_finalized`, `j2534-0404-service/src/service/
  events_event_senders.rs`). No behavior change to the RPC's response in any ordering.
- **Batch-cancel sites (PR #78 edge-case-hunter follow-up):** the same leak class was found at
  three more read-then-mark call sites racing the same maintenance reap —
  `CoptStopcomm`'s cancel-all block (`rpc_primitive.rs`), `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler
  (`rpc_misc.rs`), and `cancel_send_recv_cops_for_cll` (`events_j1939_claim.rs`) — and all three
  are now covered by the same `drain_cancelled_cop_if_finalized`
  helper. The normal-completion side (`handle_send_recv`/`handle_delay`'s
  `emit_terminal_if_live` callers) was closed the same PR: a winning `primitives` removal now
  drains any pre-existing `cancelled_cops` mark for that cop directly inside `emit_terminal_if_live`
  itself, and `cancel_link_cops` wholesale-clears a CLL's `cancelled_cops` in its own
  registrants-clear critical section.
- **Follow-up work:** regression tests — quiet-bus default-timeout finite-`N` stays alive and
  cancels cleanly; nonzero-timeout finite-`N` errors then finishes with the
  `CP_SuspendQueueOnError` hook verified; per-match deadline restart with count-completion
  winning over expiry; count-completion latency bound; `-2` unaffected regression; `-1`
  subtype's own tests unmodified — all added in this PR
  (`j2534-0404-service/tests/grpc_mock/cop_ctrl_cycles.rs`). Docs — `rpc-api-guide.md`'s
  `num_receive_cycles`/`CP_CyclicRespTimeout` rows, `glossary.md`'s **NumReceiveCycles**/
  **Receive Only** entries, `j2534-0404-architecture.md`'s tier-2 description — same commit as
  the code change per `CLAUDE.md`.

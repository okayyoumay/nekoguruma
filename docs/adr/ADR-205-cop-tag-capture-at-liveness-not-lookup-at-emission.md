# ADR-205: `cop_tag` Correctness — Capture at Liveness, Never Look Up at Emission

**Date:** 2026-09-01
**Status:** Accepted (Decision item 1's operational definition sharpened, round 5 — see Amendment
below)
**Affects:** `iso22900-service` (rpc_link, rpc_module, rpc_primitive, rpc), `j2534-0404-service`
(events, events_concat, events_event_senders, and every call site of the removed `send_error_event`
fresh-lookup form — see Consequences), ADR-204 (this ADR does not supersede it; it hardens the
invariant ADR-204 introduced but left implicit)

## Context

ADR-204 added a client-supplied `cop_tag`, echoed verbatim on every gRPC event carrying a
`cop_handle`. Across four independent Codex review rounds on that PR, the identical bug class was
found and fixed three times at three different call sites, each time re-derived from scratch and
each time narrowly scoped to the one flagged site:

1. `iso22900-service`'s `rpc_start_com_primitive` (commit `b4a741b`): `cop_tags`' insert-or-clear
   ran after the native-API lock (`self.api`) was released, a separate, later `.await`-bearing lock
   acquisition a cancelled RPC future could be dropped inside of, leaving the reconcile never run.
2. `j2534-0404-service`'s `reap_expired_cyclic_registrants` (commits `21fdb4e`, `77d9c38`): COPs
   collected for reaping under one `logical_links` lock, then (after release) a separate, later
   fresh `primitives` lookup resolved the tag — a concurrent cancel/teardown landing in the gap
   could remove the entry first.
3. `j2534-0404-service`'s main RX/`result_data` path (commit `1fe8a10`): a frame's `cop_handle`
   decided once per poll pass against a registrant snapshot, but the tag re-resolved fresh, later,
   across several intervening `.await` points before delivery — the same gap, in this feature's own
   central use case (a `CoptSendrecv` COP's actual response payload).

A fourth review round then found two more instances in the same PR, still uncommitted at the time of
this ADR:

4. `iso22900-service`'s `rpc_destroy_com_logical_link`: `terminate_cop_tags_for_link`'s prefix sweep
   (`cop_tags.retain(|(module, cll, _), _| !(*module == h_mod.0 && *cll == h_cll.0))`) ran after
   `self.api` was released, with an intervening `terminate_subscription` await. Since the repo's own
   ISO mock always reuses the same numeric CLL handle after destroy, a concurrent
   `CreateComLogicalLink`+`StartComPrimitive` landing in that gap could insert a legitimate new tag
   under the same `(module, cll)` prefix — which the stale sweep would then erase, a new failure
   shape (destroying valid data, not merely returning stale data).
5. `j2534-0404-service`'s `send_error_event` (the ordinary fresh-lookup form, distinct from the
   `send_error_event_with_tag` variant fix 2 added): a concrete new example (the `CoptStartcomm`
   failure path) confirmed the previously-recorded "~20-30 estimated
   `revert_hardware_to_live_active`-adjacent call sites" backlog item (`j2534-0404-service/docs/
   implementation-notes.md`) actually understated the surface — any of `send_error_event`'s ~39 call
   sites without a same-critical-section tag snapshot has this gap, not just the
   `revert_hardware_to_live_active`-adjacent ones.

A `design-advisor` consult on this recurrence (this ADR's own analysis) established two facts that
make a structural fix both correct and cheap:

- **`CopEntry.cop_tag` is write-once.** It is assigned exactly once, at `StartComPrimitive`
  insertion time (`j2534-0404-service/src/service/rpc_primitive.rs`), and never mutated again for
  that COP's lifetime. Consequently, once any code has read a COP's tag from a live `CopEntry`, that
  value stays valid forever for that COP — there is no "the tag changed since I read it" case to
  guard against, only "did I read it before or after the entry might have been removed."
- **`iso22900-service`'s native-API mutex (`self.api`) is already this crate's lifecycle
  serialization point.** Every path that could mint a successor for a reused `(module, cll)` pair —
  `CreateComLogicalLink`, `StartComPrimitive` — must acquire `self.api` first (established by
  `b4a741b`'s own fix). A stale sweep running inside that same guard, rather than after releasing
  it, does not need to detect a handle-reuse race after the fact (a generation counter, an ABA
  token) — the race becomes structurally unrepresentable, because no successor can exist until the
  sweep has already completed.

Given these two facts, the four narrowly-scoped point-fixes above share a single root cause: a
"resolve the tag fresh, from mutable shared state, at emission/cleanup time" default that a prose
doc-comment (on the now-removed `send_error_event`, claiming "every other call site calls it
immediately... unaffected") asserted was safe for the untouched majority of call sites, a claim that
review round 4 falsified. Prose invariants over dozens of call sites do not hold under review
pressure; a type signature that makes the invariant impossible to violate does.

## Decision

**1. `cop_tag` is captured wherever/whenever the code already holds a live reference to the COP's
entry — never re-resolved at emission time.** This follows directly from write-once-ness: any
in-hand read is valid forever, so there is no reason for an event-sending function to ever perform
its own lookup. In `j2534-0404-service`, this means deleting `send_error_event`'s fresh-lookup form
outright and merging its signature into `send_error_event_with_tag`'s (the tag travels paired with
the `cop_handle` as one value the caller already has — e.g. `Option<(u32, Option<Vec<u8>>)>`, or an
equivalent small type — never as a `cop_handle` alone that the callee re-resolves). This makes the
compiler enumerate every one of the ~39 call sites in one pass, the same audit four Codex rounds
performed one site at a time; each site's fix is mechanical, since the tag is almost always already
in hand from whatever earlier read decided this event should be sent at all (e.g. the `CoptStartcomm`
failure path already reads the entry for other fields and only failed to keep the tag alongside them).

**2. In `iso22900-service`, every `cop_tags` mutation tied to a native lifecycle event (the
`rpc_start_com_primitive` insert-or-clear reconcile, and both teardown sweeps —
`terminate_cop_tags_for_link`/`terminate_cop_tags_for_module`) executes inside the same held
`self.api` guard as the native call that changes that lifecycle state, in the established
api-outer/`cop_tags`-inner lock order.** `rpc_destroy_com_logical_link` and `rpc_module_disconnect`
(confirmed by this ADR's `design-advisor` consult to share the identical gap — fixing only the one
Codex-flagged site would have repeated ADR-088's own "fixed one variant, left the sibling open"
mistake) both move their sweep inside a manually-acquired `self.api` guard, mirroring
`rpc_start_com_primitive`'s own established shape from `b4a741b`. This designates `self.api` as the
authoritative lifecycle serialization point for `cop_tags`: a handle-reuse race is not detected after
the fact (no generation counter or ABA token is introduced) — it is made structurally impossible,
since no successor CLL/COP can be created until the guard holding the sweep is released.

**3. Emission-gating (whether to fire an event at all — the existing "first-wins" idioms,
`emit_terminal_if_live` and friends) remains a separate concern, governed by the liveness checks
already in place, and is unaffected by this ADR.** This ADR is scoped to WHICH tag value accompanies
an event that is already going to be sent, not whether it's sent.

### Alternatives rejected

- **Generation/epoch keys on `cop_tags`** (detect a handle-reuse race after the fact rather than
  serializing it away). Rejected for `iso22900-service`: redundant once the sweep moves inside the
  already-existing `self.api` critical section, which eliminates the window rather than merely
  flagging it — a second mechanism solving an already-closed problem. (`j2534-0404-service`'s own
  `connect_generation` field, ADR-086, solves a genuinely different problem — disambiguating a
  physical-channel identity across a real hardware I/O `.await` that cannot be serialized away — and
  is not analogous here.)
- **`(module, cll, cop, generation)` keying, or an `Arc`-per-COP structure whose own lifetime IS the
  liveness signal.** A correct alternative to `iso22900-service`'s fix, but a materially larger
  restructure for the identical guarantee the api-guard fix already provides at near-zero
  incremental cost.
- **Keep both `send_error_event` forms; add an audited "this call site is safe because..." comment
  to each of the ~39 sites.** This is the pattern that produced review rounds 2 through 4 — a prose
  contract across many call sites, unenforced by the type system, already falsified once.
- **Point-fix only the two sites Codex's fourth round actually named, leave the class open for a
  fifth round to find the next instance.** Rejected as the whole reason this ADR exists: four rounds
  of "fix the one flagged site" already demonstrated this doesn't converge, per `design-advisor`'s
  own explicit recommendation to stop point-fixing once a bug class recurs past `.claude/README.md`
  Cost-policy rule 3's two-instance threshold.

## Consequences

- `j2534-0404-service/src/service/events_event_senders.rs` loses its fresh-lookup `send_error_event`
  function; every one of its ~39 call sites is updated to supply a pre-captured
  `(cop_handle, cop_tag)` pair (or `None`) instead of a bare `cop_handle`. This is a mechanical,
  compiler-driven migration, not a design decision at each site — the value to pass is almost always
  already available from whatever read decided the call should happen at all.
- `iso22900-service`'s `rpc_destroy_com_logical_link` and `rpc_module_disconnect` both restructure to
  hold `self.api` manually (rather than via `with_api_for_link`, matching `rpc_start_com_primitive`'s
  own precedent) across the native teardown call and the `cop_tags` sweep, with no intervening
  `.await`.
- **Discriminating regression tests** for both fixes reuse `77d9c38`'s established technique
  (`tokio::sync::Mutex`'s FIFO-fair semaphore semantics to force a race deterministically, no timing
  margin needed) — confirmed fail-without/pass-with before landing, per this repo's own lesson from
  that same commit's own first attempt.
- **Accepted residual, recorded not fixed by this ADR**: `iso22900-service`'s `rpc_subscribe_event`
  has a structurally similar (but non-tag-scoped) ABA shape — its subscription-map insert happens
  before the native `register_event_callback` call, across a separate `self.api` acquisition, and
  (unlike the fixes above) `rpc_subscribe_event` does not currently hold `self.api` across the whole
  sequence the way this ADR's fix does for destroy/disconnect. Left open as its own, pre-existing,
  non-`cop_tag` concern (already tracked as a P2 in `iso22900-service/docs/implementation-notes.md`
  from an earlier review round); this ADR does not touch it.
- **Accepted residual, unaffected by this ADR**: `j2534-0404-service/docs/implementation-notes.md`'s
  existing P3 item about `send_error_event`'s own re-verification-at-generation-boundary question is
  a *different* gap (whether a stale-generation event should fire at all, i.e. emission-gating) from
  what this ADR closes (which tag value accompanies an event that's already firing) — it survives
  this ADR's removal of the P2 backlog entry describing the now-closed "estimated 20-30 sites" tag
  question, restated as self-contained rather than cross-referencing the deleted entry.
- Every one of ADR-204's own prior "not fixed here, tracked as backlog" notes for this bug class is
  now resolved by construction (the compiler enumerates and forces every site), so the P2 backlog
  entry describing it is deleted from `j2534-0404-service/docs/implementation-notes.md`, per this
  repo's own backlog-entry convention — its still-open residual (the P3 above) is extracted first,
  self-contained, before deletion.
- Future review checklist item (recorded here since it's the practical enforcement of this ADR's
  Decision, not itself a new decision): a `primitives`/`cop_tags` tag lookup that is not sourced from
  the same critical section that established the associated handle's liveness is a defect on sight,
  in either crate, going forward — not something to re-litigate case by case.
- **Amended, round 5 (see Amendment below)**: a `primitives` read of the shape
  `.get(&cop_handle).and_then(|entry| entry.cop_tag.clone())` that flattens "entry absent" into
  `cop_tag: None` and proceeds as if the COP were still live, OR a liveness check performed only
  after an already-irreversible side effect (a buffer drain, a queue mutation) has run, is a defect
  on sight — the same review-checklist item above, stated precisely enough to catch both shapes.

## Amendment (round 5, 2026-09-01)

A fifth Codex review round on PR #116 found the identical bug class again, in a form this ADR's
original Decision text did not precisely rule out: `handle_start_comm`
(`j2534-0404-service/src/service/events.rs`) captured `cop_tag` via
`ctx.primitives.lock().await.get(&cop_handle).and_then(|entry| entry.cop_tag.clone())` — a read that
had the liveness verdict in hand (entry present vs. absent) and **flattened it away** into a plain
`Option<Vec<u8>>`, then emitted `PduCopstExecuting` unconditionally regardless of which case held. A
`design-advisor` consult (continuing this ADR's own escalation) confirmed the same flattening idiom,
independently introduced, in four sibling handlers — `handle_delay`, `handle_stop_comm`,
`handle_update_param`, `handle_restore_param` — and two milder variants with the same root cause:
`reap_expired_cyclic_registrants` (deciding to reap, and applying side effects, before checking
whether `cancel_link_cops` had already removed the entry) and
`finalize_and_deliver_concat_buffers_if_live` (`events_concat.rs`, delivering a drained batch via the
now-deleted `resolve_cop_tag` helper without checking whether the entry had already been cancelled
out from under it). `handle_send_recv` was the one call site that already did this correctly, and
served as the fix's template.

**Sharpened operational definition of Decision item 1's "capture at liveness":** the tag is read from
the `CopEntry` in the same held `primitives` critical section whose existence check constitutes the
caller's liveness verdict, and (a) an absent entry is handled as that verdict — bail or skip, never
flattened via `.and_then`/`.map`/`.filter` into a plain `Option<tag>` that the caller then acts on as
if the COP were live — and (b) any effect justified by that verdict (a status emission, a buffer
drain, a queue mutation) occurs while that same guard is still held, or is otherwise provably ordered
after it, never before. `handle_send_recv` (`events.rs`, its `let cop_tag = { ... }` block) is the
normative template: lock `primitives`, bail on absence, clone the tag, emit `PduCopstExecuting` while
the guard is still held, all in one block.

**Fix applied:** `handle_start_comm`, `handle_delay`, `handle_stop_comm`, `handle_update_param`, and
`handle_restore_param` were all converted to `handle_send_recv`'s template — `let Some(entry) =
prims.get(&cop_handle) else { return; };`, with the `PduCopstExecuting` emission still inside that
same critical section. `reap_expired_cyclic_registrants` now checks `prims.get(&cop_handle)` first,
inside its per-cop loop, and `continue`s past every side effect (suspend-queue bump,
`registrants.retain`, `cancelled_cops.remove`, the `found` push) when the entry is already gone —
first-wins, leaving that registrant's cleanup to `cancel_link_cops`'s own later `registrants.clear()`.
`finalize_and_deliver_concat_buffers_if_live` gained the same bail, and — per a same-round
`edge-case-hunter` finding on the fix's own first attempt — the bail was ordered to run **before**
`finalize_concat_buffers(r)` drains the registrant's buffer, not after: checking liveness after an
irreversible drain still discards the buffered data while reporting nothing wrong, exactly the
"checked too late" failure this amendment's operational definition part (b) now names explicitly.
`resolve_cop_tag` (`events_event_senders.rs`) lost its last production caller in this fix and was
deleted. A new discriminating regression test was added for each of the two non-mechanical fixes
(`handle_start_comm`'s bail path in `events_handle_start_comm_cop_tag_tests.rs`;
`finalize_and_deliver_concat_buffers_if_live`'s bail-before-drain ordering in
`events_registrant_lifecycle_tests.rs`), both confirmed fail-without/pass-with per this repo's
established discipline.

This is scoped as an in-place amendment, not a new ADR: it corrects the operational statement of this
ADR's own Decision item 1, not a new decision — the underlying invariant (capture at liveness, never
resolve fresh at emission) is unchanged; only its precise definition is sharpened.

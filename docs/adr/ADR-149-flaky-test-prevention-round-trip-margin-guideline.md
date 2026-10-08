# ADR-149: Flaky-Test Prevention — Centralized Round-Trip Margin Guideline for `tests/grpc_mock/`

**Date:** 2026-07-29
**Status:** Accepted
**Affects:** `j2534-0404-service/tests/grpc_mock/harness.rs` (new constant, module-doc guideline), `j2534-0404-service/tests/grpc_mock/tester_present_send_type.rs`, `j2534-0404-service/tests/grpc_mock/stopcomm_data_tx.rs` (existing comments retargeted to the new constant), `.claude/agents/implementer.md` (pointer), `j2534-0404-service/docs/implementation-notes.md` (pointer + triage note)

## Context

`j2534-0404-service/docs/implementation-notes.md`'s "Test-suite
reliability: past flaky-test root causes" section records five flaky-test
incidents in `tests/grpc_mock/`, each independently diagnosed and fixed
(2026-07-22, 2026-07-28). Reviewing them together (rather than as isolated fixes)
surfaced three recurring write-time causes and one wrong fix attempt:

1. Wall-clock margin that didn't account for round-trip/event-propagation
   overhead (measured at up to ~85ms in this harness under load) — most
   directly, a test with two client-observable round trips inside its
   timing window flaked with the identical configured margin a
   one-round-trip sibling handled safely, showing the needed margin scales
   with round-trip count, not a flat per-test constant.
2. Chained relative `tokio::time::sleep()` calls whose individual
   scheduling slop compounds, versus an anchored `sleep_until(deadline)`
   pattern that doesn't drift the same way.
3. An assertion against shared/queued state (an "is finished" check) that
   didn't filter to the specific entity under test, so an unrelated
   entity's own activity in the same queue could satisfy it — a
   correctness bug that widening timing margins alone could not fix (and
   in fact made fail *more* reliably once uncovered, which is what led to
   finding the real cause).
4. A first attempted fix for one of the timing incidents relaxed an exact
   assertion (`== 1` to `>= 1`) to paper over the flake; a Codex review
   caught that this silently defeated the assertion's actual purpose (it
   would no longer distinguish a real regression that stopped sending the
   immediate frame). The corrected fix widened the configured interval
   instead, preserving the exact assertion.

Each test file re-measures and re-documents the ~85ms round-trip figure
independently ("measured elsewhere in this file"), with no shared
reference — a new test author has no single place to find the number or
the lessons above before writing a new timing-sensitive test, so the same
mistakes recur across otherwise-unrelated PRs.

A `design-advisor` review of an initial three-rule proposal (margin
multiplier, anchored deadlines, entity-scoped predicates) found it
incomplete on two points: the margin rule as originally stated (a flat
multiplier) is disproven by incident 1's own one-vs-two-round-trip
comparison, and the assertion-weakening lesson from incident 4 (rule D
below) was missing entirely despite being independently
Codex-review-documented.

## Decision

1. **One canonical source, `tests/grpc_mock/harness.rs`'s module doc**,
   holds the write-time guideline, next to a new
   `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS` constant that centralizes the
   previously-scattered ~85ms figure. `.claude/agents/implementer.md` and
   the worker-crate backlog's Known-Flaky-Tests preamble get a bare
   pointer to it, not a restated copy — flaky tests in this codebase's
   history were written across many PRs, including sessions that never
   pass through `implementer`'s own briefs, so `implementer.md` cannot be
   the sole home without missing those authors; a pointer avoids creating
   a second copy that could drift from the canonical text (the same
   pointer-over-restatement pattern ADR-144 applied to a different
   duplicated-condition problem).
2. **Margin is budgeted per client-observable round trip inside the
   window being tested, not as a flat total.** A test with N round trips
   needs roughly N times the per-round-trip ceiling, with generous
   headroom (this repo's resolved fixes used ~3-7x).
3. **Anchor deadlines with `sleep_until`, not chained relative sleeps**,
   for any test asserting timer/deadline behavior.
4. **Scope assertions on shared/queued state to the specific entity under
   test** (e.g. `cop_handle`-qualified predicates), following the existing
   `is_finished_for_cop_a`-style naming convention.
5. **Never weaken an exact assertion to fix a timing flake.** Widen the
   configured interval/window instead, preserving the assertion's ability
   to catch the regression it exists to detect. When widening, keep the
   elapsed/upper bound meaningfully below the interval's un-overridden
   default, so a regression where the override silently didn't take
   effect is still caught rather than passing inside a too-generous
   window (the actual, deliberate detail of incident 4's corrected fix).
6. **Not adopted: a CI/lint mechanical backstop** (e.g. a grep ban on
   `tokio::time::sleep(` in `tests/grpc_mock/`, or a check that a
   predicate references a specific handle). Rejected — legitimate sleeps
   remain even after every one of these five fixes (initial/settle delays
   in the corrected test bodies), so a textual ban would false-positive
   on the resolved code itself; whether a predicate is "scoped enough" is
   a semantic judgment a lint cannot evaluate. This differs from ADR-144's
   `gate-sync-check`, which backstops mechanical text-equality, not
   test-design judgment — that precedent does not transfer here. The
   existing Codex review loop (which caught the rejected assertion
   weakening in incident 4) and the Known-Flaky-Tests triage protocol
   (`.claude/agents/cargo-runner.md`) remain the actual backstops.
7. **A distinct triage lesson (not a write-time rule) is recorded in
   `implementation-notes.md`'s "Test-suite reliability" section (and the
   worker-crate backlog's preamble), not folded into the four rules
   above:** if widening a suspected timing margin makes a test fail *more*
   often or deterministically, that disproves the margin theory —
   instrument rather than keep widening (incident 3's actual diagnosis
   path).

## Consequences

- The ~85ms figure has one source (`GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`)
  instead of six independently-measured comments; a future re-measurement
  updates one constant, and the six existing comment sites now name it
  instead of restating a bare number.
- New test authors — whether working through `implementer` or directly —
  have one place to find both the number and the four rules before writing
  a new timing-sensitive test.
- **Accepted residual:** this is a documented guideline, not a mechanically
  enforced one. Nothing prevents a new test from ignoring it; the Codex
  review loop and Known-Flaky-Tests triage remain the actual backstops,
  same as before this ADR for any judgment-dependent test-design question.
- **Accepted residual:** `GRPC_ROUND_TRIP_OVERHEAD_CEILING_MS`'s value
  (~85ms) is specific to this harness's current measured environment and
  will drift if the harness, CI runner, or dependency versions change
  meaningfully; it is a documented estimate to budget from, not a
  guaranteed ceiling.
- No change to `gate-sync-check`, `MARKER_FILES`, or any prior ADR's
  decisions — this is a new, independent guideline for a different
  problem (test-design flakiness, not `.claude/` governance-file
  duplication).

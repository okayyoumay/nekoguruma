# ADR-102: CP_P2Star Reloads Per 0x78; CP_RC78CompletionTimeout Reinstated as Total Ceiling

**Date:** 2026-07-19
**Status:** Accepted — amends ADR-056 and ADR-057 (0x78/RC78 handling only; RC21/RC23 anchoring and the µs→ms conversion fix are unchanged). Unrelated to, and does not interact with, ADR-100/ADR-101's two-tier COP response-binding registry — this ADR is scoped entirely to the RC78/RC21/RC23 *deadline computation* inside `wait_for_expected_response_inner`, not to frame attribution/routing.
**Affects:** `j2534-0404-service/src/service.rs` (`RcHandlingConfig`),
             `j2534-0404-service/src/service/events.rs` (`wait_for_expected_response_inner`),
             `j2534-0404-service/src/service/comparam_defaults.rs`,
             `j2534-0404-service/src/service/service_params.rs`

## Context

ISO 14229-2 §7.3 specifies that a UDS client receiving NRC 0x78
(RequestCorrectlyReceived-ResponsePending) reloads its P2*client timer on
*each* occurrence — the extended wait window resets every time the ECU
sends another 0x78, with no standards-defined ceiling on the total number
of occurrences or total elapsed time (the client's own escape is to
abort/cancel the request).

ADR-056 correctly moved `RcHandlingConfig::from_params`'s RC78 deadline
source from the non-standard `CP_RC78CompletionTimeout` to the
standards-named `CP_P2Star`, matching D-PDU/ISO 14229 convention for what
that ComParam is *for*. However, ADR-057's anchor-once-at-first-occurrence
fix — written to bound `CP_RC78CompletionTimeout`, whose name and doc
comment describe a total-duration ceiling — stayed attached to the same
deadline after ADR-056 repointed it at `CP_P2Star`. The result: a client
that sets `CP_P2Star` to ISO 14229-2's typical `P2*server_max` default (5 s)
gets a **5 s total cap** on the entire 0x78 sequence, not a 5 s
*per-occurrence* reload — a spec-legal ECU that emits periodic 0x78 during a
long operation (e.g. flash erase, emitting 0x78 roughly every 4 s) times out
well before the operation actually completes. Every existing preset masked
this because ADR-056's preset-sync step seeded `CP_P2Star` from each
preset's `CP_RC78CompletionTimeout` value (25–30 s), large enough that the
practical difference rarely surfaced.

Separately, `CP_RC78CompletionTimeout` (`service_params.rs`) has been fully
inert since ADR-056 — get/settable but read by no code path — even though it
remains pre-seeded with real per-protocol values and its own name and doc
comment describe exactly the anti-stall total-ceiling role this codebase's
anchor-once mechanism actually needs.

A related gap, found across two rounds of PR review on this same change
before it was temporarily reverted for an unrelated main-branch conflict and
is being reapplied here: neither the 0x78 deadline write nor the RC21/RC23
deadline write in `wait_for_expected_response_inner` clamped its computed
deadline to `match_reset_ceiling` — the absolute deadline `CoptStopcomm`'s
non-cancellable IS-MULTIPLE receive phase uses to bound an otherwise
-uncancellable wait (ADR-087). Beyond the tail-deadline clamp, the RC21/23
branch's own chunked `CP_RC21/23RequestTime` sleep and subsequent retransmit
ran unconditionally, ignoring the ceiling until *after* both had already
completed — so a client-configured request time longer than the ceiling
could still hold the non-cancellable phase open well past it.

## Decision

**CP_P2Star reloads on every 0x78 occurrence.** The 0x78 branch now sets
`deadline = now + rc78_p2_star_ms` unconditionally on each occurrence (the
D-PDU/ISO 14229-2 P2*client reload semantics), rather than reading a
`get_or_insert_with`-anchored single deadline.

**CP_RC78CompletionTimeout is reinstated as an independent total-duration
ceiling**, anchored once at the *first* 0x78 in the COP. Every reloaded
deadline is clamped to this ceiling when one is configured. `0`/absent
disables the ceiling entirely — ISO 14229-2 defines none, so there is no
sensible default duration to substitute; `CancelComPrimitive` remains the
escape for a link that wants no ceiling.

`RcHandlingConfig` gains a renamed field `rc78_p2_star_ms` (was
`rc78_completion_timeout_ms`, unchanged read source `CP_P2Star`, default
5000 ms) and a new field `rc78_total_ceiling_ms: Option<u32>` (read from
`CP_RC78CompletionTimeout`, µs→ms, `0`/absent → `None`).

**RC21/RC23 anchor-once semantics are unchanged** — those two codes have no
standards "reload per occurrence" mechanism analogous to P2*, so ADR-057's
original anchor-once fix remains correct for them.

**Preset seeding**: the ADR-056 sync step in `comparam_defaults.rs` that
copied each preset's `CP_RC78CompletionTimeout` value into `CP_P2Star` is
removed. Presets keep their existing `CP_RC78CompletionTimeout` values
(25–30 s) as the ceiling; `CP_P2Star` falls back to `RcHandlingConfig`'s own
5000 ms default for every preset that does not explicitly configure it.

**Both RC78 and RC21/RC23 deadline-write sites also clamp to
`match_reset_ceiling`** when one is in scope (`CoptStopcomm`'s IS-MULTIPLE
ceiling, ADR-087). The RC21/23 branch additionally checks the ceiling
before its `CP_RC21/23RequestTime` sleep starts, on every chunk boundary
during it, and once more immediately after the sleep loop exits — the last
check catches the case where the loop's own final chunk is what crosses the
ceiling (exiting via the `while` condition rather than an in-loop `break`,
which the earlier per-chunk check alone would miss).

## Consequences

- Behavior change: the *per-0x78* reload window shrinks from every preset's
  former 25–30 s (inherited from `CP_RC78CompletionTimeout` via the
  now-removed sync) to 5 s (the new `CP_P2Star` default) unless a deployer
  explicitly sets `CP_P2Star`. The *total* ceiling is unchanged (still
  25–30 s per preset, now sourced correctly from `CP_RC78CompletionTimeout`
  instead of being read but discarded).
- A link with RC78 handling enabled and `CP_RC78CompletionTimeout` left at
  `0`/unset has no total ceiling on a chattering-0x78 ECU — spec-faithful,
  but a deployer wanting an anti-stall bound must set it explicitly.
- `tests/grpc_mock/rc_handling.rs`'s anchor test is rewritten to assert
  reload-per-occurrence against `CP_P2Star`, and a ceiling test is added
  against `CP_RC78CompletionTimeout`. `tests/grpc_mock/stopcomm_data_tx.rs`
  gains two tests proving the `match_reset_ceiling` clamp actually bounds
  both the RC78 and RC21/23 paths, including the RC21/23 sleep-and-retransmit
  case a first pass at this fix initially missed.
- `comparam_defaults.rs`'s two ADR-056 sync tests are replaced with one test
  confirming the sync no longer happens.
- ADR-057's `**Status:**` is amended to note this ADR narrows its
  ceiling-anchoring fix to RC21/RC23 only; ADR-056's "no longer drives any
  behavior" consequence bullet for `CP_RC78CompletionTimeout` is superseded
  by this ADR reinstating it.
- Scoped entirely to deadline computation, not frame attribution — no
  interaction with ADR-100/ADR-101's response-binding registry rework.

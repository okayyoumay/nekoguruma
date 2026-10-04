# ADR-057: RC-Completion-Timeout Ceiling Anchoring and Microsecond Units

**Date:** 2026-07-04
**Status:** Accepted (amended by ADR-102 — the RC78/0x78 anchor-once fix below now applies only via ADR-102's reinstated `CP_RC78CompletionTimeout` ceiling; RC21/RC23 anchor-once and the µs→ms conversion fix are unaffected)
**Affects:** `j2534-0404-service/src/service.rs` (`RcHandlingConfig`),
             `j2534-0404-service/src/service/events.rs`
             (`wait_for_expected_response`)

## Context

`CP_RC21CompletionTimeout`/`CP_RC23CompletionTimeout`/`CP_RC78CompletionTimeout`
are each the maximum total time allowed for the repeated wait-or-re-request
cycle triggered by that specific negative response code — a fixed ceiling
for the whole retry sequence, not a per-occurrence window. Two bugs in
ADR-018's original implementation contradicted this:

**1. The deadline reset on every occurrence, instead of anchoring once.**
`wait_for_expected_response` recomputed `deadline = now + completion_timeout`
every time a pending-RC code (0x78, 0x21, or 0x23) was detected — including
repeats of the *same* code. An ECU that kept sending 0x78 (or kept
triggering 0x21/0x23) shortly before each deadline could therefore extend
the wait indefinitely, with no actual ceiling on the total retry duration,
defeating the parameter's purpose.

**2. The stored value was read as milliseconds, not the microseconds every
other D-PDU timing ComParam uses.** `CP_P2Min`/`CP_P2Max` (`p2_max_timeout_ms`),
`N_Ar`/`N_As`/etc., and `CP_StMinOverride` are all stored in microseconds
with an explicit conversion at their read site. `RcHandlingConfig::from_params`
had no such conversion for `CP_RC{21,23,78}CompletionTimeout` or
`CP_RC{21,23}RequestTime` — it used the raw stored value directly as
milliseconds. Every existing preset's literal values for these ComParams
(e.g. `CP_RC78CompletionTimeout = 25_000_000`, `CP_RC21RequestTime =
200_000`) were authored in microseconds, matching the sibling P2-family
ComParams in the same file — read literally as milliseconds, these produce
absurd real timings (a 25,000-second RC78 ceiling; a 200-second sleep
before every RC21 re-request). ADR-018's own hardcoded fallback defaults
(5000 ms, 25 ms) are sensible *as milliseconds*, which is what made the
mismatch easy to miss: the fallback path was fine, but the moment any
preset's actual seeded value was in effect, the resulting duration was off
by a factor of 1000.

Neither bug had any test coverage before this ADR (`RC78`/`RC21`/`RC23` had
zero references anywhere in `tests/`), so both went uncaught.

## Decision

### Anchor at first occurrence, per code

`wait_for_expected_response` now tracks each code's ceiling independently
(`rc78_ceiling`, `rc21_ceiling`, `rc23_ceiling: Option<Instant>`) and
computes it only once, on that code's first occurrence in the COP:

```rust
deadline = *rc78_ceiling.get_or_insert_with(|| {
    tokio::time::Instant::now() + Duration::from_millis(rc_cfg.rc78_completion_timeout_ms as u64)
});
```

A repeat of the *same* code re-enters the poll/re-request loop without
touching the already-established ceiling. A *different* code occurring
later (e.g. 0x21 after 0x78) gets its own independent ceiling anchored at
its own first occurrence, using its own configured timeout — each
`CP_RC*CompletionTimeout` genuinely bounds only the handling of its own
code, per the parameter's per-code naming.

### Convert microseconds to milliseconds

`RcHandlingConfig::from_params` now reads `CP_RC{21,23,78}CompletionTimeout`
and `CP_RC{21,23}RequestTime` through the same microsecond→millisecond
conversion `p2_max_timeout_ms` already uses (`div_ceil(1000).max(1)`,
treating 0/absent as "use the millisecond-denominated fallback default"
rather than a real zero-length wait). This also applies to
`CP_P2Star` (ADR-056's replacement source for the RC78 ceiling) — its own
doc comment already said "in ms," which was itself part of the same
mistake; `CP_P2Star`, `CP_P2Min`, and `CP_P2Max` are all D-PDU timing
ComParams and are all stored in microseconds.

### ADR-018 §6's worked example was already stale

While verifying `CP_RCByteOffset` semantics for this fix's tests, ADR-018's
own documented example (`CP_RCByteOffset` relative to the raw 4-byte-CAN-ID
-prefixed frame) turned out to already be incorrect: ADR-051 (dated after
ADR-018) made RC-byte-offset detection run against the header-stripped
payload, not the raw frame — `implementation-notes.md` and ADR-051 itself
already documented this correctly, but ADR-018's own text was never
updated. ADR-018 §6 is corrected in place to point here and to the right
offset (payload-relative, not frame-relative).

## Consequences

- An ECU that repeatedly sends the same pending-RC code can no longer stall
  a COP indefinitely — each code's total handling time is capped at its own
  `CompletionTimeout`, measured from that code's first appearance.
- Every RC-family completion-timeout and request-time value now produces
  the duration its microsecond-denominated preset literal actually implies
  (e.g. `CP_RC78CompletionTimeout = 25_000_000` us → a 25 s ceiling, not
  25,000 s). This is a real behavior change from the previously-broken
  (1000x longer) durations, but matches every preset's evident intent and
  this codebase's established D-PDU timing-ComParam convention.
- New test coverage (`tests/grpc_mock/rc_handling.rs`) exercises the
  anchor-not-reset behavior end-to-end; unit tests in `service.rs` cover
  the unit conversion and its defaults. Both gaps had zero prior coverage.

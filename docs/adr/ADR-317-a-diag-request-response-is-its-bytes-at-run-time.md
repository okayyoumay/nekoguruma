# ADR-317: A `diag.request` Response Is Its Bytes at Run Time

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (`src/star/lower.rs`, `src/convert/tests.rs`),
`crates/diag-frontend/docs/starlark-subset.md` 3.2 and 3.4, design 8.4; ADR-313 item 5,
ADR-309 item 4

## Context

Subset 3.4 gives the result of `diag.request` as a dict with the keys `ok`, `nrc`, `fields` and
`raw`. The VM has no dict type: its values are integers, floats, booleans and bytes. ADR-313
item 5 therefore lowered the call as a statement and dropped the response that `ServiceRequest`
leaves on the stack. A procedure could not test whether a request succeeded.

The host returns the whole final response, negative ones included (design 8.2.4), so everything
the keys hold can be derived from those bytes. What makes a response negative belongs to the
application layer. ADR-309 item 4 moves that shape into the declaration part, through the IR
send-receive work, but the declaration part does not carry it yet.

## Decision

1. **The response stays as its bytes.** At run time a response is the `Bytes` the host
   returned. The lowering gives it a type of its own, so it can be assigned, passed and
   returned like any other value. Each key is lowered where it is read:
   - `raw` is the bytes themselves.
   - `ok` is true unless the first byte is `7F`. An empty response has no `7F`, so it reads as
     positive.
   - `nrc` is 0 for a positive response and the third byte for a negative one. A negative
     response shorter than three bytes stops the run with the VM's index error when `nrc` is
     read.
   - `fields` is the field-read item's work (ADR-316 gives it the instruction).
   No instruction is added, and `IR_SCHEMA_VERSION` does not change.
2. **UDS's negative-response shape for now.** The `7F` test and the NRC's position are UDS's
   (ISO 14229-1 (2026) 7.6: a negative response carries `7F`, then the SID of the request and
   then the response code). They give way to the shape the declaration part will carry under ADR-309 item 4.
3. **Only the four keys.** Any other string key is `STAR_UNSUPPORTED_SYNTAX`. A key that is not a
   string literal is `STAR_DYNAMIC_KEY_NOT_ALLOWED` (subset 4.1), also when the response comes
   from a function's return value or parameter, which the checker does not follow. Using a response as anything else is
   `STAR_NOT_LOWERABLE`: its truth value, which is always true in Starlark, an operator, or a
   built-in. Equality is rejected too, although Starlark defines it: a response compared with
   bytes would be silently false where `res["raw"]` was meant. `res["ok"]` says what a truth test would mean.
4. **ADR-313 item 5 is superseded.** The call's value is the response, and a call used as a
   statement drops it with the `Pop` every expression statement gets.

## Consequences

- Each read of `nrc` keeps the response in a temporary slot so that it can be read twice. The
  lowering already does this for floor division.
- A procedure that reads `nrc` from a malformed negative response fails instead of reading a
  made-up code.
- Reading the same key twice repeats its few instructions. The response is read from the slot
  it was stored in, so the request is never sent again.
- When ADR-309 item 4 lands, the `7F` test and the NRC's position have to come from the
  declaration part. Only `response_ok` and the `nrc` arm in `src/star/lower.rs` read them.

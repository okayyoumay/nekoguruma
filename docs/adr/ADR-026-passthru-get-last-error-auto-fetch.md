# ADR-026: Automatic PassThruGetLastError Fetch on J2534 API Failure

**Date:** 2026-07-01
**Status:** Accepted
**Affects:** `j2534-0404/src/error.rs`, `j2534-0404/src/lib.rs`, `j2534-0404/tests/live_channel.rs`, `j2534-0404/tests/live_iso15765.rs`, `j2534-0404-service/src/service/events.rs`

## Context

`PassThruGetLastError` was already FFI-bound and exposed via a public
`J2534Api0404::last_error_text()` method, but no call site invoked it
automatically when a PassThru* call failed. `Error::ApiStatus` carried only
the raw numeric status code, so callers never saw the vendor DLL's
human-readable failure detail — including `j2534-0404-service`, which builds
every gRPC `Status::internal(...)` error message from `Error`'s `Display`
output (e.g. `Status::internal(format!("PassThruOpen failed: {err}"))`).

## Decision

`Error::ApiStatus` changed from a tuple variant to a struct variant:
`ApiStatus { code: StatusCode, description: Option<String> }`.

A new private `J2534Api0404::check()` method wraps the existing
`error::check()` free function and, on failure, makes a best-effort call to
`self.last_error_text()` to populate `description`. All 20 PassThru* call
sites in `lib.rs` were switched from `error::check(...)` to `self.check(...)`.

One call site was deliberately **not** switched: the `PassThruGetLastError`
call inside `last_error_text()` itself still uses the raw `error::check()`.
Routing it through `self.check()` would recurse — calling
`PassThruGetLastError` to describe why `PassThruGetLastError` failed.

The description fetch is best-effort and never masks the original error: if
`last_error_text()` itself fails, or returns an empty string, `description`
is simply `None` rather than the whole operation failing differently than
before.

`Display` for `Error::ApiStatus` appends the description in parentheses when
present. Because `j2534-0404-service` already converts `j2534_0404::Error`
to gRPC `Status` via `Display` (`format!("... failed: {err}")`) at every
call site, this enrichment reaches gRPC clients automatically — no changes
were needed in the service layer itself.

## Consequences

- Breaking change to `j2534_0404::Error`'s public shape: the four call sites
  that pattern-matched `Error::ApiStatus(code)` (two `live_*` integration
  tests in `j2534-0404`, two buffer-empty checks in
  `j2534-0404-service/src/service/events.rs`) were updated to
  `Error::ApiStatus { code, .. }`.
- Every failing PassThru* call now makes one additional FFI call
  (`PassThruGetLastError`). This is acceptable: these are already
  exceptional/error paths, not the message-throughput hot path.
- `j2534-0500` has a structurally identical `ApiStatus`/`check()` pattern
  (its own copy of the same wrapper design for the v05.00 spec) but was
  **not** updated, since it has no consuming service yet
  (`j2534-0500-service` does not exist as a workspace member). Apply the
  same change there if/when that service is built.
- No change was made to the D-PDU-style `PduErrorEvent`/`last_error` fields
  tracked per-module/per-CLL in `j2534-0404-service` (see ADR-004); those
  store structured error *codes*, not text, mirroring ISO22900's
  `PDUGetLastError` semantics, and are a separate concern from this
  text-description enrichment at the `j2534-0404` wrapper layer.

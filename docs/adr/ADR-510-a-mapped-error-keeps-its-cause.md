# ADR-510: A Mapped Error Keeps Its Cause

**Date:** 2026-11-26
**Status:** Accepted
**Affects:** `Cargo.toml` (`[workspace.lints.clippy]`), the error types of every crate that mapped an error without its cause, `CLAUDE.md` ("Rust code"), `.github/instructions/rust.instructions.md`, `diag-ir` (`VmError`), `agent` (`src/journal.rs`, `src/oplog.rs`), ADR-288

## Context

ADR-502 item 5 plans `clippy::map_err_ignore` for stage 3. The lint fires on
`map_err(|_| ...)`, a closure that throws away the error it maps. When the lint was added, it
fired at 230 sites. The causes thrown away there fall into three groups:

- causes that would tell a reader where a failure came from: an I/O or HTTP error, a task that
  panicked or was cancelled, a record that did not decode;
- causes that carry little more than the new error already says: an integer that did not fit,
  bytes that were not UTF-8, a timeout, an opaque cryptographic error;
- refusals at a trust boundary, where the answer to the other side must not say which check
  failed.

The lint cannot tell these groups apart. The workspace needs a rule for when dropping the cause
is right. Without one, each site would be decided on its own, and an override would be the
cheapest way through.

## Decision

1. **The lint is adopted** workspace-wide (`map_err_ignore = "warn"`) with no crate-level
   opt-out.
2. **The cause is kept by default**, including a cause that carries little information. That
   the new error already describes the failure is not a reason to drop the source: a chain that
   ends early hides which call failed.
   - A Rust error type holds the cause as its source: a `#[source]` field, or `source()`.
   - An error that is text (`String`) carries the cause in the text.
3. **A type that cannot hold a non-comparable cause gives up its comparison.** An error type
   that derived `PartialEq`, `Eq`, `Clone` or `Copy` and must now hold an `io::Error`, a
   `reqwest::Error` or the like loses those derives. Tests match on the variant instead of
   comparing the whole value. A comparable stand-in for the cause (its kind, its text) would
   end the `source()` chain, and is not used.
4. **An error sent to another process carries the cause in its text.** This covers a worker's
   gRPC `Status`, the launcher's JSON-RPC error and the server's HTTP answers. The text says
   what was wrong with the request, so the cause is appended to the fixed text.
5. **At a trust boundary the answer stays the same, and the cause is logged on the refusing
   side.** This covers verification of a signature, proof or MAC; parsing a key, certificate or
   token received from the other side; and a registration or session refusal.
   - The local error keeps the cause as its source.
   - Where the refusal becomes the answer, the refusing process records the cause with
     `tracing`. The log does not cross the boundary.
   - The answer itself has the same text and code as before.
   - A library without logging keeps the cause as a source, and its caller logs it.
6. **An override is written only where the cause cannot be kept.** It is
   `#[expect(clippy::map_err_ignore, reason = "...")]` on the narrowest item. The reason says
   why the cause cannot be kept. Examples:
   - a C-ABI function of a mock or the simulator, which returns a status code or `()` across
     the C boundary and has no other place to record the cause;
   - a lock's poison error, which borrows its guard and cannot be stored, where the error type
     holds no text;
   - an error whose text or value may hold a secret: a refused database URL, whose parse error
     may repeat the password, or a command-line argument that is not UTF-8, whose error is the
     argument itself;
   - a library error with no content, such as a unit `()` error.
   A closure parameter named only to silence the lint (`|_e|`) is not used.
7. **A value that only selects an outcome may log the cause instead of holding it.** A worker's
   transmit failure becomes an error event of the D-PDU API, built from a value made in many
   places. Where the failure is mapped to that value, the worker logs the cause, and the value
   stays as it was.

8. **A journaled error carries its cause as text.** `VmError` is part of the agent's journal
   format (ADR-288), so its variants hold the cause's text rather than the error itself. The
   journal's format version is 2; a journal of version 1 is refused, as any other version is.
   The operation log shows the variants' fixed text without the cause, since a decode error's
   text holds values read from the vehicle.

## Consequences

- A failure report shows its whole chain: the server's and the agent's logs say which call
  failed beneath a mapped error.
- A worker's `Status` and the launcher's JSON-RPC errors gain a cause after their fixed text.
  Clients that match on the whole text, rather than the code, see a longer message.
- Error types that held only `Copy` values are larger and no longer comparable. Their tests
  match on variants.
- A refusal at a trust boundary costs one log record on the refusing side.

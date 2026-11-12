# ADR-467: The Session Route Takes the Challenge Before the Database, and Challenge Issuing Is Capped at Sixty a Minute

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 6.4, 14.4; `crates/server` (`session`, `register::Limiter`, `db::failure`), `crates/server/docs/ngr-server.md`, `api/openapi.yaml` (`/v1/agent/challenge`, `/v1/agent/session`); sets the numbers that ADR-466 item 3 left to the server work and restates one consequence of ADR-466

## Context

ADR-466 fixes the policy of the proof exchange: a challenge is removed at its first presentation
whatever the outcome (item 2), a 401 is never counted nor turned into a 429 (item 3), and issuing
is limited by a cap whose numbers the server work sets. Building the server's side left four
points open. The agent's public key comes from the database here (registration takes it from the
request), so every 403 costs one row read and at most one signature verification, and the order of
the checks decides what an unauthenticated caller can make the server do. The cap's number has to
be chosen against the store's bound of 1024. The stores need a clock a test can move. And a
consequence in ADR-466 does not hold with any cap at or below the bound: with at most 1024 admitted
in a window as long as a challenge's life, the oldest outstanding challenge is always expired
before the store could be full, so a caller cannot push one out on one server.

## Decision

1. **The session route is judged in this order, and the first step that applies answers:** the
   request guard, the method (405), the body (413, 408), the shape (400), the challenge (401), the
   agent's row (503, 500), the key and the proof (403, or 429 past ten refusals within a minute),
   then the token (200). The shape comes first and consumes nothing, so a well-formed retry within
   the challenge's life still works; the store lookup is the presentation. The challenge comes
   before the database, so a stale or unknown challenge is a 401 with no database access, whatever
   else is wrong with the request, and a caller without a live challenge cannot make the server
   read a row. A 503 or a 500 after the take leaves the challenge consumed and counts nothing.
   `api/openapi.yaml` already wrote this order. *Rejected:* the row before the challenge, because a
   stale challenge with an unknown agent would then be a counted 403, against ADR-466 item 3.
2. **Challenge issuing is capped at 60 per sliding 60 seconds per router.** Past it the route
   answers 429 with `Retry-After` (1 to 60 seconds), records nothing, draws no random bytes and
   leaves the store and the session route untouched. A request that passes the body rule (the
   body must be empty; the route has no shape rules) counts even if the random source then fails. The cap bounds the unauthenticated row reads
   and signature verifications of the session route to 60 a minute, is far above honest use (one
   challenge per connection attempt, one more after a 401, an agent's backoff of 1 to 60 seconds),
   and keeps the live challenges well below the store's bound. The store keeps its bound of 1024
   with the oldest dropped as its own invariant, tested directly, for a later change of the
   numbers. The cap is global to the route: there is no caller identity on the local profile.
   *Rejected:* 10, registration's number, which two agents in a retry loop would exceed; a cap
   above 1024 to keep the pushed-out wording true, which buys nothing.
3. **Tokens: many per agent, no bound of their own.** Each token lives its own 300 seconds and
   nothing replaces an earlier one, since ADR-466 item 2 makes it a multi-use bearer and a later
   data channel or long poll would otherwise lose its token to a reconnect. Every token costs one
   challenge, so at most 360 are live (60 a minute over the 300 seconds, plus the 60 seconds a
   challenge may wait). The table is keyed by the SHA-256 of the token's bytes, drops expired
   entries at each issue, and offers `Sessions::authenticate` and `Sessions::revoke_agent` for the
   control channel. *Rejected:* one token per agent.
4. **The stores read tokio's clock** (`tokio::time::Instant`) and take the instant as a parameter,
   so a unit test passes explicit instants and the route test moves the clock with
   `tokio::time::pause`, `advance` and `resume`, never with `start_paused`, which would turn
   pending socket waits into instant timeouts. The server's tests enable tokio's `test-util`.
   *Rejected:* an injected clock object in the shared state, which is plumbing for one test.
5. **Three log words, and an unknown or revoked agent is not signature-checked.** A refusal is
   logged as `unknown_agent`, `revoked` or `proof`, each through its own 60-second gate. A proof
   for another audience, another key, another agent ID, another challenge or nonce, and a signature
   that is not low-S are all `proof`: the server builds the transcript with its own audience, so a
   wrong audience cannot be told from a bad signature. The 403 is byte-identical for the three; its
   timing is not, since the first two skip the verification. Agent IDs are not secrets, and
   registration's classifying read differs in time as well. A stored public key that does not
   decode is the server's own fault: 500 with a gated error line, not counted.
   *Rejected:* a dummy verification to even out the time.

## Consequences

- A caller that uses the whole cap can deny challenges to honest agents for up to a minute. On the
  local profile such a caller could hold the port anyway. The agent meets a 429 from the challenge
  route and waits for `Retry-After`, within its backoff cap.
- ADR-466's consequence that a caller can push the oldest outstanding challenges out of the 1024 is
  restated: on one server the cap keeps that from happening, and the bound is a safeguard.
- The 403 does not hide a difference in time between an unknown or revoked agent and a bad proof.
- Session state is per process (ADR-466): several server instances need the shared design that the
  multi-instance work brings.
- The server's tests gain tokio's `test-util`, which changes nothing until a test pauses the clock.

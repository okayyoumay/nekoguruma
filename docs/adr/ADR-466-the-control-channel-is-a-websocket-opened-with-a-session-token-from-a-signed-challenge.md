# ADR-466: The Control Channel Is a WebSocket Opened With a Session Token From a Signed Challenge

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 5.2, 5.3, 5.7, 6.4, 13.3, 14.1, 14.4; `api/asyncapi.yaml` (`agentControl`), `api/openapi.yaml` (`/v1/agent/challenge`, `/v1/agent/session`); `crates/shared-proto` (`control`), `crates/shared-crypto` (`proof::Audience`); the server's proof exchange, WebSocket and hub, and the agent's session library and `connect` command (later work); fixes the transport that ADR-459 item 3 left open; extends ADR-456 item 9 and ADR-461 items 9 and 10; relies on ADR-463 item 13

## Context

Design 5.2 names a WSS control channel for commands and events, 6.4 and ADR-459 item 3 name a
clock-free proof of the agent's key, and ADR-461 and ADR-463 fixed the job instruction and
registration. Nothing fixed how the proof reaches the server, what the connection carries, how
it ends, or what each message settles. Reading the repository for the answer showed that some
fixed points constrain it:

- ADR-456 item 8 and its Consequences: the server is a Host-and-Origin guarded HTTP/1 loop, a
  GET never changes state, and an agent pointed at `localhost` meets a 307 it must treat as an
  error (ADR-463 item 13).
- The device clock is untrusted (16.3), so nothing the agent reports about time can set a
  lifetime.
- `api/asyncapi.yaml` described payloads without a discriminator and with an `agentId` in them,
  and `shared_proto::AgentEvent` serialises in serde's default shape, not the spec's flat one.
- The local profile is one server process (ADR-456 item 1), and has no TLS.

## Decision

1. **The exchange is two HTTP routes, then the upgrade.** `POST /api/v1/agent/challenge` (empty
   body) answers `{challenge}`. `POST /api/v1/agent/session` takes `{agentId, challenge, nonce,
   proof}` and answers `{token, expiresInSecs}`. The WebSocket is `GET /api/v1/agent/control`.
   Both POST routes take bodies of at most 4 KiB read within 5 seconds, a strict object, and
   answer 400 on shape alone before any lookup, as registration does (ADR-463 item 22). The
   singular `agent` keeps the routes clear of `/v1/agents/{agentId}`. *Rejected:* the proof in
   the upgrade request itself, because ADR-459 item 3 fixes an HTTP exchange that also serves
   long polling and the data channel; a challenge fetched with GET, because a GET must not
   change state (ADR-456 Consequences) and issuing a challenge does.
2. **Challenges and tokens live in server memory.** A challenge is 32 random bytes valid for 60
   seconds and removed at its first presentation whatever the outcome; at most 1024 are
   outstanding and the oldest is dropped beyond that. A token is 32 random bytes of which only
   the SHA-256 is kept, a multi-use bearer for 300 seconds, reported as a relative
   `expiresInSecs`, because the device clock is untrusted. There is no migration. *Rejected:* a
   table, because the local profile is one process, a restart drops every connection anyway,
   and a table would let unauthenticated callers write rows. Verification across several server
   instances is later work (design 15.2 point 1).
3. **Two refusal classes.** 401 for a missing, expired or reused challenge: the agent fetches a
   new one at once. One uniform 403 for an unknown agent, a revoked row, a bad signature and a
   wrong audience; the reason goes to the server's log only, rate-bounded. Refusals are limited
   as registration's are (ten within a sliding minute, then 429), and a valid proof is never
   limited. *Rejected:* a distinct answer for a revoked key: it would be unauthenticated on the
   local profile, and design 5.5 ties erasure of the agent's data to detected revocation, so it
   waits for the work that authenticates the server.
4. **One canonical audience.** `scheme://host[:port]`, lower case, the scheme's default port
   left out, no path, no trailing slash, query, fragment or user information. It is
   canonicalised once, when the registration record is written (one trailing slash and an
   explicit default port are accepted there and written away), and signed byte for byte from the
   record. The server builds it from its own `pinned_origin(port)` and never reads an audience
   from a request. `shared_crypto::proof::Audience` is the one type for both sides; it accepts
   `http` and `https` syntactically, and the agent refuses `https` separately until the
   standard profile (ADR-463 item 13). `localhost` is refused, never rewritten to `127.0.0.1`:
   ADR-456 answers it with a redirect that an agent treats as an error. *Rejected:* normalising
   on each connection (two sides could then disagree on a string neither wrote down), and
   accepting `localhost` as an alias (it would sign a name the server never serves).
5. **A pass closes connections of revoked agents.** Every 30 seconds the server checks the IDs
   of connected agents against the database, closes the revoked ones and drops their tokens.
   Registering a key again revokes its earlier row (ADR-463 item 14) while the old process may
   still be connected. *Rejected:* checking only at connect (a revoked agent would stay
   connected for as long as it likes); a push from the revoking transaction (it ties revocation
   to one instance, and a database pass also suits several).
6. **WebSocket stack.** The server upgrades with axum's `WebSocketUpgrade` on the existing
   hyper HTTP/1 loop. The agent uses `tokio-tungstenite` (`default-features = false`, feature
   `handshake`) with `client_async` over a TCP stream it opens itself: no name resolution, no
   redirect following, no TLS until the standard profile. Both crates are already in
   `Cargo.lock`. Two consequences are part of the decision. hyper-util's graceful watch is
   implemented for the plain HTTP/1 connection but not for the one that allows upgrades, so the
   server's loop needs its own watch that calls the connection's graceful shutdown. And axum
   runs the socket callback on a detached task, so at a stop the server sends close 1001 to
   every connection, waits within `shutdown_grace_secs`, and aborts the rest before the
   database closes. *Rejected:* a client that follows redirects or resolves names (it would
   follow the 307 that ADR-456 intends as an error).
7. **The token is an `Authorization: Bearer` header,** answered 401 before any upgrade. The
   request guard is unchanged: the agent sends no `Origin`, and a browser cannot set this
   header on a handshake. *Rejected:* the token in the query string (it ends up in logs) or as
   a subprotocol value (it blurs the version negotiation); an extra Origin rule for agent
   routes (the guard already refuses a foreign Origin, and nothing is gained).
8. **Framing.** One JSON object per text frame, internally tagged `type` equal to the asyncapi
   message name, other properties camelCase. Binary frames are refused. A message is at most
   128 KiB in both directions (a full job command is 87,512 base64 characters for a 65,633 byte
   envelope, plus a few dozen bytes). Nothing is compressed. The version is the subprotocol
   `ngr-control.v1`; an upgrade without it is refused, and a message of an unknown `type`
   closes the connection (16.3). The server's first frame is `hello {heartbeatSecs}` and the
   agent handles nothing before it. The wire types in `shared-proto` have explicit shapes,
   separate from the in-process `AgentEvent`. *Rejected:* serde's default externally tagged
   shape (it does not match the spec and a reader could not tell the type from the keys);
   compression (a size limit and a trusted peer make it a risk and not a need).
9. **No identity in payloads.** `heartbeat`, `agentEvent` and `reconcile` no longer carry an
   `agentId`; the server takes the agent ID from the session only. *Rejected:* keeping it and
   checking it against the session: a field that must always equal something already known
   only adds a way to get it wrong.
10. **Settlement is explicit.** The agent answers a `jobCommand` with `commandAck {commandId}`
    or with `commandRejected`. `eventAck {seq}`, cumulative, is reserved for the outbox work.
    `commandId` is unique per delivery and scoped to the connection, never the job ID, so a
    redelivery is a new delivery of the same job. *Rejected:* treating the next event as the
    ACK (a command that starts nothing would never be settled); acknowledging by job ID (two
    deliveries of one job would be indistinguishable).
11. **Keepalive is an application heartbeat.** The agent sends `heartbeat` every
    `heartbeatSecs` (20 by default), the server echoes it, and either side closes after three
    intervals without any frame. The payload is empty: the device clock is untrusted, so a
    `sentAt` would carry nothing. `agents.last_seen_at` is written at connect and at
    disconnect only (design 15.1). *Rejected:* WebSocket ping frames alone (a load balancer or
    proxy may answer them for the peer); writing the row on every heartbeat (a write per agent
    per interval).
12. **One connection per agent.** A new connection replaces the old one; the server closes the
    old one with the application close code 4001. The agent holds an exclusive lock in its state
    directory while connected, so two processes of one agent do not fight, and backs off to its
    longest delay when it is told it was replaced. *Rejected:* refusing the new connection (a
    half-open old one would lock the agent out until three missed intervals).
13. **`commandRejected` settles the delivery.** The server stops redelivering and ends the job
    as failed by its own decision, and the queue item names the code (ADR-461 item 10 rules out
    `SignatureInvalid`). The rejection is sent on the connection the command arrived on and
    never goes through the outbox, since it belongs to a delivery that exists only there. A
    reason may join `commandRejected` only if the envelope bytes and the registration record
    alone decide it; expiry and the operator match are job states (design 5.6). *Rejected:*
    redelivering a refused command (the same bytes would be refused again, and a forged flood
    would be amplified); a rejection through the outbox (it would outlive the connection that
    can explain it).

## Consequences

- Session state is per process until the multi-instance work: a second server instance cannot
  verify a token the first issued, and a restart drops every token. Agents fetch a fresh
  exchange before every upgrade, so a restart costs one round trip.
- A token is a bearer for its lifetime (300 seconds): a process that obtains one can open the
  channel as the agent, though it cannot issue jobs, which are signed (design 11.1).
- The proof exchange authenticates the agent, not the server. On the local profile a process
  that holds the port could answer both routes. Server authentication must prove the server on
  the WebSocket itself (the `hello` frame and the fresh exchange are its hooks), not only on the
  HTTP exchange.
- Long polling for proxy environments (design 5.2) reuses the same token and the same messages;
  it is deferred to M8 and the data channel is its own work.
- An offline package refused by the gate is answered to the request that presented it, since it
  has no connection and no `commandId` (design 5.7).
- The server's accept loop and its graceful stop change when the WebSocket lands (item 6).

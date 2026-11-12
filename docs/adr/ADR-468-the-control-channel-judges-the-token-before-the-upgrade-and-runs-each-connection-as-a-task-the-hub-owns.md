# ADR-468: The Control Channel Judges the Token Before the Upgrade and Runs Each Connection as a Task the Hub Owns

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 5.2, 13.3, 14.1, 15.1; `api/asyncapi.yaml` (`agentControl`); `crates/server` (`lib`, `control`, `hub`), `crates/shared-proto` (`control`), `crates/server/docs/ngr-server.md`; settles what ADR-466 left to the server work and extends its items 5 and 6; details ADR-456 item 9's graceful stop for upgraded connections

## Context

ADR-466 fixed the control channel: the transport, the framing, the close codes, the heartbeat, one
connection per agent, and a pass that closes the connections of revoked agents. It left to the
server work the order of the refusals at the upgrade, what counts as a frame for the idle rule,
whether the token matters after the upgrade, how a socket that axum starts on a detached task can
be stopped, what `last_seen_at` does on replacement and on a database failure, and the shape of
the hub's interface. Two facts of the stack constrain the answers. hyper-util cannot watch an
upgradeable HTTP/1 connection, and after a read error the WebSocket layer allows no further read
(it writes still work).

The server's side is built in two steps that meet at the database. The first has no SQL: the
route, the connection task, replacement and the stop. The second adds what needs a query or a
public sending interface: `last_seen_at`, the revocation pass, and the sending of commands.

## Decision

1. **The route is judged in this order, and the first step that applies answers:** the request
   guard (unchanged, before routing), the method (405 with `Allow: GET`; the route takes every method and the handler
   refuses all but GET itself, HEAD included, because the router's own 405 has no body), the token (401 with
   `WWW-Authenticate: Bearer`), the WebSocket request (400), the subprotocol (400), then the
   upgrade (101, naming the subprotocol, no extension). Every refusal is the server's problem
   document. A caller with no token learns nothing else about the route, and the token costs
   one SHA-256 and a map lookup, so it comes first. *Rejected:* axum's extractor as a handler
   argument, whose rejection would answer 400 before the token is looked at; a 426 for a request
   that cannot be upgraded, which the asyncapi file does not name.
2. **The `Authorization` header** must be present once (two headers are refused, even identical),
   visible ASCII, with the scheme `Bearer` compared ignoring case, one or more spaces, and a
   non-empty token text. The text goes to `Sessions::authenticate` unchanged, so a comma, a second
   word, another alphabet or a wrong length is a 401 like an unknown or expired token. One body
   for every reason, no limiter, one gated debug line, and the token is never logged.
3. **The token is judged at the upgrade only.** It is not consumed by the upgrade (it is a
   multi-use bearer until it expires, ADR-466 item 2), not bound to the connection, and not looked
   at again: a token that expires while the connection is open ends nothing, and design 14.1 keeps
   the connection open with heartbeats. A second upgrade with the same token is accepted and
   replaces the first connection, because replacement is per agent. A 400 after a valid token
   leaves the token usable. The "session ended" of close code 4002 means the revocation pass
   (item 11) dropping the agent's tokens, not an expiry. *Rejected:* closing at expiry: the
   agent's fresh exchange comes before every upgrade, and a close every five minutes would be a
   reconnection storm for no gain.
4. **The subprotocol** is selected as axum selects it: every `Sec-WebSocket-Protocol` header is
   read, each value split at commas and trimmed of white space, then compared byte for byte with
   `ngr-control.v1`. Any other case, or none, is 400.
5. **The accept loop watches its own stop signal.** hyper-util's graceful watch is not
   implemented for `http1::UpgradeableConnection`, and the HTTP/2-capable builder that has one is
   no way out (ADR-456 item 6). The loop serves each connection with `with_upgrades()` as a task
   in a `JoinSet`; the task selects on the connection and on a `watch` that `run` sets, and
   calls the connection's graceful shutdown (a no-op once upgraded). hyper-util's
   `server-graceful` feature is no longer used. The signal's clone and its send both happen on
   the `run` task, so no connection can miss it.
6. **The hub owns the sessions.** axum runs the socket callback on a task it detaches, so the
   callback only hands the session to `Hub::adopt`, which spawns it into a `JoinSet` the hub
   owns. At a stop the order is: stop accepting, signal the HTTP connections, begin the hub's
   stop (every session closes with 1001), wait for both sets within `shutdown_grace_secs` (or
   until the second signal), abort the HTTP connections (a pending upgrade then fails), abort the
   sessions, close the pool. An upgrade that completes after the stop began is dropped without a
   frame (no 503 exists in the asyncapi file for it); a session that was adopted before the stop
   and registers after it closes with 1001 like the others. A peer that answers the Close ends
   its task at once; one that never answers is waited for 5 seconds after its Close is written
   (the write itself may take up to the 10-second write limit), or aborted at the end of the
   grace period if that is shorter. *Rejected:* running
   the session inside axum's callback and tracking a completion guard (aborting would then need
   a second mechanism); splitting the socket into reader and writer tasks (two tasks to stop and
   a lock between them, with one writer anyway).
7. **One task and one writer per connection.** It registers (becoming the agent's current
   connection), writes `hello`, and then selects on the hub's close instruction, the stop, the
   next message and the idle deadline. The agent ID is the session's only. The ordinary close
   codes are those of the asyncapi table. Reason texts are a fixed word per code and never
   anything the peer sent. One info line at the connect and one at the end carry the agent ID,
   a connection number and that word, and no message text.
8. **What the connection does with what it reads.** A complete text message that decodes to
   `heartbeat` is answered with the echo within 10 seconds. Any other text that decodes is
   handed on (the second step builds that; until then it is read and dropped). A text that does
   not decode closes with 1002, or 1009 when `decode` reports the size (not reachable behind
   equal transport limits, and mapped all the same). A binary message closes with 1003. A Close
   from the agent is answered by the stack on one more read, then the task ends. Both transport
   limits (message and frame) are 128 KiB, so the stack refuses an oversized frame on its
   header, before it buffers the payload; the limits are strict, 131072 bytes pass and 131073
   fail (RFC 6455 section 5.2 and 5.4 describe the framing). A read error is classified by the
   stack's own error: a capacity error is 1009; invalid UTF-8 or a framing violation is 1002; a
   reset without a closing handshake, an I/O error or a closed connection get no frame. After a
   clean close the task *drains* (reads and discards until the peer's reply, at most 5 seconds).
   After a read error no read is possible, so the task *lingers* (waits without reading, at most
   5 seconds or until the stop) before it drops the socket: dropping at once with unread input
   can reset the connection and lose the Close frame at the peer. This needs `tungstenite` as a
   direct dependency of `server`, for its error types only, at the version axum uses.
9. **The idle rule.** Any complete text message, ping or pong resets the idle clock, which starts
   when `hello` has been written; three heartbeat intervals (60 seconds, the interval being the
   default of 20 with no configuration key) without one end the connection with 4003. A partial
   frame, or the fragments of a message that never completes, deliver nothing and reset nothing.
   The asyncapi file says "no frame of any kind", which this follows for complete messages; ADR-466
   item 11 distrusts pings as proof of life, so this is to be looked at again when the standard
   profile puts a proxy in front of the server. Time spent waiting for a full inbox (item 12) is
   not idle time.
10. **Replacement is one critical section.** The new connection becomes current and the displaced
    one is told to close with 4001 under the same lock, so there is no moment with two current
    connections or with none. A replaced connection removes nothing when it ends, because the
    entry it finds is its successor's (connections carry a generation). Whether a connection is
    "the agent's current one" is what `leave` returns. A connection ended by replacement is not a
    disconnect for `last_seen_at` (item 13): the agent did not disconnect, and the newer
    connection stamped the row at its connect.
11. **`last_seen_at` and the revocation pass (built in the second step).** One statement stamps
    the row, `UPDATE agents SET last_seen_at = GREATEST(last_seen_at, now()) WHERE id = $1`
    returning whether the row is revoked, inside a transaction with the usual statement timeouts
    and a 5 second bound. It runs at the connect of a connection, after the 101 and before it
    registers and sends `hello`, and at the disconnect of a connection that was the agent's
    current one, after the Close frame and before the drain; a stop does the same for every open
    connection inside the grace period, and one that is aborted writes nothing. It never runs on
    a heartbeat, on any message, on a send, or at a refused upgrade. A database failure or timeout
    at connect is logged and the connection goes on: no close code exists for it, liveness lives
    in memory (design 15.1), and the session route already refuses new sessions with 503 while
    the database is down. **A revoked row, or none, at connect** closes with 4002 as the first
    frame, drops the agent's tokens and ends the task before it registers, so no earlier
    connection is displaced and no disconnect is written; this extends item 5 of ADR-466, which
    names the 30 second pass only, since otherwise a revoked agent holding a live token would be
    connected for up to 30 seconds and the pass never drops the tokens of an agent that is not
    connected. The pass itself runs every 30 seconds (none at start, none in a burst after a clock
    jump) over the connected agent IDs, with no query when nobody is connected: it reads which of
    them are not revoked, closes the others (a missing row too) with 4002 after dropping their
    tokens, and closes nobody when its read fails, since a failed read is no evidence of
    revocation. It runs as a task of the hub's set, so the stop aborts it before the pool closes.
12. **The interface to the queue (built in the second step).** `Hub::send_job_command(agent,
    signed_job)` is not async and does not wait: it draws a `commandId` from a hub-wide counter
    (unique per delivery, meaningful with the connection it went to), encodes the frame outside
    the lock and queues it on the agent's current connection, whose queue holds 16 frames. It
    returns the connection's number and the ID, which mean queued and not written; only the
    agent's answer settles a delivery. Its errors are `NotConnected` (no current connection, or
    it is closing; the job stays queued, design 14.1), `Busy` (16 frames wait, so one slow agent
    never blocks the caller) and `TooLarge` (over 128 KiB; the connection is untouched). The
    outcomes and events the agent sends (`commandAck`, `commandRejected`, `agentEvent`) go to one
    bounded inbox of 256 that the queue's work takes the receiver of once, in the order they were
    read, with the agent and the connection that sent them. When it is full the connection's
    task waits (still watching its close instruction and the stop) and reads nothing, so the
    agent sees no echo, closes after three intervals and resends from its outbox; nothing that
    must be delivered is lost. A message nobody takes is validated and dropped with a debug line.
    The hub keeps no delivery table: it writes no table of its own and holds no job state, and
    the consumer matches an acknowledgement by connection and ID. *Rejected:* three channels for
    the three message types (they lose the order between an acknowledgement and the event after
    it); dropping at a full inbox (a lost must-deliver event is resent only after a reconnect);
    refusing the connection when the stamp fails (no close code for it, and the agent's retries
    would load the exchange while the database is the problem); one batch statement at a stop for
    `last_seen_at` (a second write path beside the per-connection one).
13. **The stamp before the 101** was rejected: it needs a status the asyncapi file does not have
    (a 503 at the upgrade) and writes for an upgrade that may never complete.

## Consequences

- `server` names `tungstenite` directly and must keep axum's version. A test of the 1009 close
  fails if a later axum moves to another one.
- A stalled consumer of the inbox makes agents reconnect instead of losing events.
- During a database outage a connected agent stays connected and its row is stale until the next
  disconnect. The column comment in the initial migration still says the column is written in
  batches; a merged migration cannot change (ADR-458 item 6), so `db/README.md` notes it.
- A middlebox that originates pings could hold a dead agent's connection open until the agent's
  next connection replaces it.
- An upgrade that completes during a stop ends without a close frame.
- The queue's work gets `NotConnected` and `Busy` and must poll, or be given connect
  notifications by its own change.
- ADR-466's status line gains "item 5 extended, and item 6's stop detailed, by ADR-468".

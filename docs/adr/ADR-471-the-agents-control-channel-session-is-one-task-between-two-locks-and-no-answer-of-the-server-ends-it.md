# ADR-471: The Agent's Control Channel Session Is One Task Between Two Locks, and No Answer of the Server Ends It

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 5.2, 5.3, 6.4, 14.1; `api/asyncapi.yaml` (`agentControl`: the 1001 row, the idle rule), `api/openapi.yaml` (`ConnectionChallenge`, `AgentSession`); `crates/agent` (`control`, `server_http`, `registration`), `crates/shared-proto` (`control`: the close codes), `crates/server` (`control`), `crates/agent/docs/ngr-agent.md`; details ADR-466 items 1 to 4 and 6 to 13 for the agent's side and settles ADR-470's Consequence on `register` during a session

## Context

ADR-466 fixes the control channel's wire, and ADR-467 and ADR-468 fix the server's side of the
exchange and the WebSocket. What the agent does is left as rules: a fresh exchange before every
upgrade, one new challenge after a 401, a retry at the cap after a 403, a delay from 1 to 60
seconds reset after a session that lived 60 seconds, the closes, and the back-off after a
replacement. It leaves open how the library is shaped and started, how long it holds its
locks, which events end it, what "lived" is measured from, what the agent sends when it is
itself stopped, and how a 429 or a Close 4002 is met. ADR-470 left open whether `ngr-agent
register --replace` may run while a session is connected; the maintainer decided it may not.

Two facts constrain the answers. The server is not authenticated to the agent on the local
profile (ADR-459 item 3, ADR-466 Consequences), so nothing it answers can be trusted to stop the
agent or to erase its state. And tokio's paused clock advances to the next timer whenever no task
is woken, which races any wait on a real socket while a timer is pending (ADR-467 item 4).

## Decision

1. **Two steps, the key as a parameter.** `agent::control::open` takes the connection lock
   (`<state>/control/connection.lock`), then the registration lock
   (`<state>/registration/registration.lock`, ADR-470), and reads the registration record with
   its strict reader; each lock is tried once. `start` takes the loaded agent key, refuses one
   whose fingerprint is not the record's, and spawns the session's one task, which returns a
   handle to stop it and to watch its state. The library never touches the key store, so the
   command that starts it can say "not registered" before the store is read.
2. **Both locks for the whole run.** The task holds both locks from `open` to its end, waits
   included, and reads the record once. A `register`, with or without `--replace`, exits 3
   while a session runs, so a registration cannot revoke the running session's server row and
   leave it on a stale identity. Every exit path releases both locks; the files are never
   deleted.
3. **Only local facts end the session.** A lock that is held, a record that is absent or
   invalid, a key that is not the record's, and a stop end it. Every answer of the server is
   retried for as long as the session runs, with one warning per kind of failure since the last
   `hello` and debug lines after it: a refusal of the proof (403), a 429 on the session route
   (a valid proof is never limited, ADR-466 item 3) and a Close 4002 wait the full 60 seconds
   and erase nothing; a 400, 421, redirect or wrong subprotocol is retried like any failure.
4. **The delay.** 1 second before the first retry, doubled after every wait up to 60 seconds.
   It returns to 1 second only when a connection ends whose last frame came at least 60 seconds
   after `hello`, so a silent server cannot reset it through the idle close. 4001 and 4002 set
   it to 60 seconds whatever the connection lived. A 429 from the challenge route with a usable
   `Retry-After` waits the longer of the two and then doubles (ADR-467). A 401 on the session
   route is answered at once with one new challenge, as ADR-466 says; a 401 on the upgrade is an
   ordinary failure. The first attempt does not wait. Jitter waits for the reconnection work.
5. **One task, one writer.** The task runs the challenge, the session, the upgrade, `hello`,
   the keepalive and the wait in turn, with one socket at a time. While connected it selects,
   in this order, the stop, the idle deadline, the heartbeat tick and the next frame. A
   `heartbeat` the server sends is never answered and never matched to one the agent sent; any
   complete message, ping or pong counts as a frame for the agent's idle rule, as for the
   server's (ADR-468). A job command is decoded and left unanswered, with no job state and no
   link, until the job-command work adds the gate.
6. **The agent's closes.** 1002, 1003, 1009 and 4003 as the close table gives them, and 1001
   when the agent is itself stopped: the table's 1001 row becomes "either side is stopping". A
   stop that comes during a stalled write drops the connection instead, since the Close would
   stall behind that write.
   After its Close the agent reads until the peer's answer or 5 seconds; after a read error it
   waits 5 seconds without reading, as the server does (ADR-468). The close codes and their
   reason words live in `shared_proto::control`, so both sides use one table.
7. **Time is tokio's, and the tests use in-memory streams.** Every instant is tokio's; the wall
   clock is never read. The socket I/O goes through one seam: production opens a TCP stream to
   `127.0.0.1:<port>`, and the tests on a paused clock run the session against an in-process
   fake server over in-memory streams, so every wake is inside the runtime and virtual time is
   exact. Two tests run over real loopback in real time.

## Consequences

- An operator fixes a refused agent by stopping it, registering again and starting it; a key
  reset must take the same locks.
- A program of another user that answers on the server's port makes the agent retry at the cap,
  never exit.
- A plain `register` during a session exits 3 (another registration or a session is running),
  not 5.
- The reconnection, outbox and job-command work plug into the same loop.

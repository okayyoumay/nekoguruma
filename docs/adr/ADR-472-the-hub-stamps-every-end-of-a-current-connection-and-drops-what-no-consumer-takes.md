# ADR-472: The Hub Stamps Every End of a Current Connection and Drops What No Consumer Takes

**Date:** 2026-11-13
**Status:** Accepted
**Affects:** design 14.1, 14.4; `api/asyncapi.yaml` (`agentControl`: the 4002 row); `crates/server` (`hub`, `control`, `lib`), `crates/server/docs/ngr-server.md`, `db/README.md`; details ADR-468 items 8, 11 and 12

## Context

ADR-468 items 11 and 12 fix the hub's last-seen write, its revocation pass and its interface to
the job queue: the statement and its bound, where the write runs, the pass's cadence and its
rule for a failed read, the send's errors and queue size, and the inbox's size and wait. Building
them leaves points the text does not state: whether a revoked row is stamped, where the write
falls for the ends that send no Close of the server's, how the drain is timed after a slow write,
how the pass meets the stop, what an inbox that nobody has taken does, which inbound types go to
it, and what the error order of a send is.

## Decision

1. **One stamp, committed.** The connect and disconnect writes share one routine: a transaction
   with the usual statement timeouts, the statement of ADR-468 item 11, a commit, all inside 5
   seconds. A revoked row is stamped as well, since the statement has no revoked filter and a
   connection that the pass closed stamps the row at its disconnect anyway; a commit that fails
   counts as a database failure. Only the pass's read is dropped uncommitted.
2. **Every end of the agent's current connection writes, at a fixed place.** The task asks the
   hub whether it was the current connection (`leave`) as soon as its loop ends, and writes only
   then. After a Close the server sends, the write comes before the drain or the linger; after a
   Close the agent sent, after the one read that answers it; after an end with no frame (a
   reset, an EOF, a failed write), at once. The drain's 5 seconds run from the end of the write.
   A connection that never registered (a revoked or missing row at connect, a stop during the
   connect write) writes no disconnect.
3. **The pass ends when the stop begins.** It waits for its tick and runs its read while watching
   the stop, so a stop does not wait for the grace period on its account; the abort of ADR-468
   item 11 stays as the backstop. A clock jump of several periods gives one pass.
4. **An inbox nobody has taken drops.** Until the consumer takes the receiver, and after it drops
   it, every message is validated and dropped with a debug line and the task never waits. A
   connection that registered before the receiver was taken delivers its later messages, since
   the task asks the hub per message.
5. **`reconcile` goes to the inbox too.** The inbox carries `commandAck`, `commandRejected`,
   `agentEvent` and `reconcile`; only `heartbeat` stays with the connection.
6. **The send's order.** The send draws its ID first, so a failed call consumes one and the IDs
   have gaps; it checks the size next, so `TooLarge` comes before `NotConnected`; then it looks
   at the current connection. Frames queued for a connection that ends or is replaced are
   dropped without a report. While the task waits on a full inbox it writes no queued command.
7. **`Hub::connected()` lists pairs.** It returns each connected agent with its connection
   number, which the pass reads and the queue's work can compare with a delivery.

## Consequences

- A revoked agent's row shows the time of its last refused attempt or of its closing.
- A consumer that stalls holds the task of an agent that has gone until a successor or the stop
  ends it, and that disconnect write waits as long.
- A stop with more open connections than the pool has connections may let some writes time out;
  each has its own 5 seconds, as ADR-468 item 12 accepts by rejecting a batched write.
- ADR-468's status gains "items 8, 11 and 12 detailed by ADR-472".

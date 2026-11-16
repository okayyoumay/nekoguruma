# ADR-479: The Server Stores an Agent's Events in One Transaction per Agent and Answers on the Connection They Came On

**Date:** 2026-11-15
**Status:** Accepted (item 7 detailed by ADR-481)
**Affects:** design 5.3, 5.4, 14.1; `db/migrations/0009_agent_events.sql`, `db/README.md`; `api/asyncapi.yaml` (`eventAck`, the close table); `crates/server` (`agent_events`, `hub`, `control`, `job_queue`), `crates/server/docs/ngr-server.md`; details ADR-478 item 6 and supersedes ADR-476 item 5's dropping of `agentEvent`

## Context

ADR-478 item 6 fixed what the server does with a numbered event (store at the cursor's next
number, answer a repeat with the cursor, close on a gap) and left the table, the transaction, the
meaning of a repeat whose text differs, the way the answer reaches the connection and the queue's
handling of failures open. The delivery queue still dropped every `agentEvent` (ADR-476 item 5),
nothing read `agent_event_cursor`, and the hub could not write an `eventAck`. The agent resends
its whole unacknowledged outbox at every connection, so with no server that stored and
acknowledged events the outbox only grew.

## Decision

1. **One row per event, holding the frame's exact text.** `agent_events` has the primary key
   (tenant, agent, `seq`), `seq` at least 1, `kind`, `job_id`, `body`, `occurred_at_agent` and
   `received_at`. `body` is the text of the agent's frame as it was sent, in a `text` column, so a
   repeat can be compared byte for byte (not `jsonb`, which refuses the `\u0000` escape and does
   not keep the text). `kind` is read from the frame and has no CHECK, so another numbered kind
   needs no migration. `job_id` is set only when the frame's job ID is a canonical UUID
   (`is_canonical_uuid`) and is otherwise null; it has no foreign key, so an event for a job the
   server does not know, or one that belongs to another agent, is stored under its sender.
   `occurred_at_agent` is set only when a strict check of the text accepts it (a real calendar
   date, `T` and `Z` in capitals, an optional fraction of at most nine digits, an offset of at
   most 15:59, the range PostgreSQL reads), so that no frame can make an insert fail for good;
   it is reference only (design 16.3). `received_at` is the database's clock and the
   authoritative time. Two indexes serve a retention pass and a VIN deletion: `(tenant_id,
   job_id)` where the job ID is not null, and `(tenant_id, received_at)`. The table is not
   partitioned.
2. **One transaction per agent and round, serialised by the cursor row.** The queue hands an
   agent's events, in the order they were read, to one transaction: it takes the tenant from the
   agent's row when that is not revoked (a missing or revoked agent gets nothing stored and
   nothing answered), inserts the cursor row if there is none (`ON CONFLICT DO NOTHING`), locks it
   with `FOR UPDATE`, applies the rule below to each event, moves the cursor to the highest number
   stored and commits. The lock makes two server instances serve one agent in turn and makes
   concurrent first stores safe. The statements run under the 5-second lock and statement
   timeouts of the other server transactions; a timeout is a failure (item 5).
3. **The rule for each event, given the cursor `c` and the numbers stored earlier in the same
   transaction.** A number at or below `c` is a repeat and is never stored: its text is compared
   with the stored row's in SQL. Equal, or no row (it was deleted), is answered with the cursor.
   A text that differs stores nothing, acknowledges nothing, closes that connection with 1002 and
   ends the connection's later events in the round; it logs one gated warning with the agent ID and
   the number, never the text. The number `c + 1` is stored. Any other number, and any number no
   `bigint` holds, is a gap: nothing is stored or acknowledged, the connection closes with 1002
   and its later events in the round are not looked at. A close ends only the connection that
   sent the event; the agent's other connections and other agents are unaffected.
4. **The answer comes after the commit, to the connection the event came on.** The hub holds,
   for each connection, a one-value slot with the highest number to acknowledge
   (`Hub::ack_events`). The call succeeds only for the agent's current connection that has not been
   told to close and while no stop has begun, and every call wakes the connection's task, so a
   repeat is answered again and several calls before the task runs give one `eventAck {latest}`.
   The task never writes it after a close instruction or a stop, and does not poll for it while it
   waits on a full inbox. A repeat is answered with the cursor, a stored event with its own number.
   `Hub::close_connection` gives the close instruction to one connection by its number, and not to
   a successor that replaced it. Events of a connection that has ended or been replaced before
   the commit are stored and not acknowledged: the agent sends them again on its next connection
   and they are repeats. Nothing is acknowledged at connect, since the agent resends from its first
   unacknowledged event.
5. **Failures keep the events and the queue retries on its tick.** The queue splits numbered
   events out of the inbox before it applies the iteration's answers, writes the answers first
   and then stores each agent's events, at most 64 agents per round. The first failure ends the
   round; the failed agent's events and those not yet tried are kept, counted with the buffered
   messages against the 4096 bound, past which the inbox is not read. A commit that went through
   though it was reported as failed leaves rows that the retry finds as repeats of the same text.
   `progress` and `reconcile` messages are still dropped. Counters of the events seen, stored and
   answered as repeats are kept in the queue's settings for tests.
6. **The table is shaped for retention and VIN deletion; deleting rows does not lower the
   cursor.** The detail an event carries is vehicle data (design 4.3, 15.2). A repeat of a
   deleted row is answered with the cursor, so a deletion never makes an agent resend from the
   start or the server store a deleted event again.
7. **`jobs` is not changed.** An event is stored whatever job it names and affects no job row;
   how a stored state is applied to the job is not decided here; when it is, it belongs in the
   storing transaction and only for an event stored as the next number.

## Rejected

- **No comparison of a repeat's text.** A repeat could then hide a different event under a number
  already used, as when an outbox file was replaced and numbering restarted, and the server
  would acknowledge events it never stored.
- **Closing silently or marking the agent on a differing repeat.** The close is the whole
  reaction; a state the operator can see for an agent the server keeps closing is not
  part of this decision.
- **`jsonb` for the body, or both.** The escape for U+0000 is refused by `jsonb`, an event could
  then not be stored at all, and the exact text would be lost.
- **One transaction per event.** It multiplies commits for a backlog, and the agent's order
  within a round is lost to concurrent transactions.
- **A foreign key on `job_id`, a CHECK on `kind`.** A job the server does not know, or a kind
  added later, would make an insert fail and block every later event of the agent.
- **Acknowledging at connect, or per connection of the same agent in one slot.** The cursor says
  what is stored, not what a connection sent, and a successor would be told about its predecessor.

## Consequences

- A cursor that is behind the numbers an agent still holds (a database restored from a backup,
  design 14.5) makes every connection of that agent close with 1002 until it registers again.
- An agent whose connection the server keeps closing stays the delivery queue's target, since
  nothing marks it.
- The queue's buffers count messages, not bytes: 4096 events of 128 KiB are held in memory.
- An event read before a `commandAck` in the same batch is stored after that answer is written
  (answers first), which the later job-state work must take into account.
- A statement that fails for good (an inconsistent table) fails every round at that agent, and
  the queue keeps trying; the failed agent goes behind the others, so only that agent waits.
  Only a transient failure was designed for.

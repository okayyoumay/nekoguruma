# ADR-478: The Agent Numbers Guaranteed Events in a Durable Outbox, and the Server Stores Them in Order by That Number

**Date:** 2026-11-14
**Status:** Accepted
**Affects:** design 5.3, 5.4; `api/asyncapi.yaml` (`agentEvent`, `eventAck`); `crates/shared-proto` (`control`); `crates/agent` (`outbox`, `state`, later `control`), `crates/agent/docs/ngr-agent.md`; the server's event storage and `db/migrations` (later work); details ADR-466 item 10 and ADR-473 item 1

## Context

ADR-466 item 10 reserved a cumulative `eventAck` for the outbox work, and ADR-473 left the
numbering and acknowledgement scheme open: an agent answers a job command with `commandAck`
alone, so a `received` event, a state change or an HMI request has no way to reach the server
that survives a lost connection or a restart. The wire carried a `seq` that was only "increasing"
and a `delivery` property that marked an event guaranteed or lossy, the examples numbered a
lossy progress event between guaranteed ones, and the server read no event: the job queue
dropped it, and the cursor row each agent has (`agent_event_cursor`) was read by nothing. A
cumulative acknowledgement is sound only if the numbers have no gaps, since the server could
otherwise not tell a lost event from a number never used, and only if the counter never goes
backwards under one agent ID, since the server would drop the repeated number as a duplicate.

## Decision

1. **One contiguous counter per agent ID, acknowledged cumulatively.** Each guaranteed event
   (`jobState`, `hmiRequest`) carries a `seq` of at least 1 that is the previous number plus one;
   the first event of an agent ID is 1. The number is assigned inside the synced append to the
   outbox, so it is the event's position in the outbox file, and nothing is sent before the
   append returns. A frame a crash tore is cut off when the outbox is opened and its number is
   used again. A re-registration gives a new agent ID, hence a new outbox and a new cursor row.
   The server's `eventAck {seq}` confirms every event up to and including `seq`.
2. **`progress` is outside the scheme.** A `progress` event carries no `seq`, is never stored,
   acknowledged or resent, and may be thinned (5.4). The `delivery` property is removed: the kind
   says which events are numbered. The control codec requires `seq` of at least 1 on the numbered
   kinds, refuses it on `progress`, and refuses an unknown `delivery` like any unknown property.
3. **One file per agent ID.** `<state>/outbox/<agent-id>.outbox` uses the journal's frame format
   (length, CRC-32, payload) with a header (format version, agent ID, protection, floor) and one
   frame per event holding its number and the exact text of its control frame, so a resend is
   byte-identical. Every append is written and synced before it returns; an exclusive lock on a
   sidecar file makes one outbox the only writer; acknowledgements change memory only. The
   acknowledged prefix leaves the file by a rewrite (a temporary file, synced, renamed over the old
   file, the directory synced; the same routine creates the file) when nothing is pending or
   64 KiB are acknowledged, and the header's floor keeps the counter across an empty file. After a
   restart an acknowledged event not yet rewritten away is pending again and is sent again; the
   server deduplicates. Files of other agent IDs are never opened and cost one warning with their count.
4. **Bounds and failure.** The file is capped at 16 MiB. Nothing is dropped for room: an append
   that would pass the cap is refused (`Full`), the producer keeps the fact in its journal and
   reports it later, and a `received` event is refused above 8 MiB so that the events that finish
   accepted jobs keep room. An event text over 128 KiB (a control message's limit) is refused. A
   failed write poisons the outbox until it is opened again, since a failed sync may lose data
   the OS accepted. A file that does not read back (another agent ID in its header, an unknown
   version or protection, a bad checksum before the last frame, numbers that are not contiguous)
   is corrupt: it is not repaired or set aside, `connect` exits with an error and the owner
   registers the agent again.
5. **The channel records `received` before it acknowledges.** After the intake takes a verified
   command and before the channel sends `commandAck`, the channel appends a `received` event on a
   blocking task raced with the session's stop. A failed append withholds the acknowledgement, so
   the server delivers the job again after its lease, which the intake already has to expect
   (ADR-473 item 4). Until the intake is idempotent on the job ID, a redelivery appends a second
   `received` event.
6. **The server stores guaranteed events in order, in the transaction that moves the cursor.** A
   new table holds one row per event (tenant, agent, `seq`, kind, job ID, body, the device time as
   reference, the receive time as authority). For an event of agent `a` with cursor `c`: `seq <= c`
   is not stored and is answered with `eventAck {c}`; `seq = c + 1` is stored and the cursor
   advanced in one transaction, and `eventAck {seq}` is sent after the commit; `seq > c + 1`
   is neither stored nor acknowledged and the connection closes with 1002, so the agent opens a
   new one and sends from its first pending event in order. The detail is vehicle data, so the
   table follows the retention and VIN-deletion rules (4.3, 15.2).
7. **The range rule for an acknowledgement.** An `eventAck` at or below the highest number the
   agent has seen acknowledged changes nothing. One above the highest number the agent has
   assigned is a protocol violation: the agent closes the connection with 1002 and drops
   nothing.

## Rejected

- **A UUIDv7 per event, acknowledged per event.** It needs no counter, but each event needs its
  own acknowledgement and a set of acknowledged identifiers, its order rests on the device
   clock, which is untrusted (16.3), and the cursor row could not express "stored up to here".
- **An embedded store (SQLite or a key-value library) for the outbox.** A dependency and a second
  durability story for a queue the journal's frame format and sync discipline already fit.
- **One file per event.** A directory scan and a sync of the directory per event; the order is
  implicit in names.
- **A random stream ID in the header** (deduplicating by agent, stream and number), which would
  let a lost outbox restart under the same agent ID. Kept for later if an outbox that is lost
  without a re-registration proves common.
- **Folding the events into the state storage** of the job (too large a change for this step),
  **the cursor alone** (the detail would be lost), **only the intake appending** (no event
  before the runner exists) and **acknowledging even when the append failed** (a `received`
  event could be lost silently).

## Consequences

- An outbox that is lost or corrupt under one agent ID needs a re-registration; the counter must
  never go backwards under an ID.
- A `received` event repeats with every redelivery until the intake is idempotent on the job ID.
- The server needs a table with retention and VIN deletion before it stores events.
- An event that is acknowledged on a live connection can only be sent again after a
  reconnection; a single lost frame on an open connection is repaired at the next connect.
- A forged `eventAck` from an unauthenticated server (the local profile) makes the agent
  forget events the real server never stored; it is bounded by the range rule only.
- Work after this decision: the agent's sender and acknowledgement handling and the `received`
  append (the channel); the server's migration, the planner for the cases of item 6, the
  acknowledgement and the retention of the table; thinning of `progress`.

# ADR-476: The Delivery Queue Takes a Job Once and Delivers It at Least Once, From the Stored Instruction

**Date:** 2026-11-14
**Status:** Accepted
**Affects:** design 5.3, 12, 14.1; `db/migrations/0007_job_delivery.sql`, `db/README.md`; `api/asyncapi.yaml` (`failureCode`); `crates/server` (`job_queue`, `hub`, `lib`), `crates/server/docs/ngr-server.md`; `crates/shared-proto` (`FailureCode`); details ADR-466 items 10 and 13, ADR-468 item 12 and ADR-472 item 4 for the server's side, and ADR-461 item 8 and ADR-462 for a retired key

## Context

The hub puts a job command on an agent's connection without blocking and gives the agent's answers
to one inbox (ADR-468 item 12), but nothing yet reads the `jobs` table, sends a stored
instruction or settles a delivery. Design 5.3 and 14.1 fix the shape: the instruction is signed
once and a resend carries the same bytes; an agent's answer settles a delivery by connection and
command ID; with several instances each polls the database and sends only to the agents connected
to it. Open points were how two instances avoid sending one row twice, where the progress of a
delivery is recorded, what happens to an unanswered delivery, what a rejection or a retired key
does to the job, and how the pieces are tested.

## Decision

1. **One task owns the queue.** A task of the hub's set, started by `Server::start` (a server
   made by `Server::bind` leaves it off unless a test turns it on), is the only consumer of the
   hub's inbox and runs the polling pass on a tick (2 seconds). Each iteration takes the hub's
   connection snapshot first, then drains the inbox, then releases the deliveries whose
   connection is not current, then runs the pass if a tick is due. The stop ends the task in its
   wait and in the pass's read; a pass that has queued a frame finishes its mark.
2. **A pass is one transaction, and the take is exactly once.** It ends the jobs of a retired key
   (item 6; it skips the rows another pass holds), then selects, for each agent that is
   connected to this instance and not revoked (a lateral select from `agents`, so that the planner
   reaches the rows through `jobs_pending`), at most the outbound queue's 16 rows in creation
   order, each numbered within its agent (its rank, 1 for the agent's oldest), and keeps the
   first 64 of all by (rank, creation time, ID): every agent with work gets its oldest job into
   the pass before any agent gets its second, an idle agent takes no place, the transaction stays
   short, and no state is kept between passes (an earlier design took a window of four agents
   from a rotating cursor, which an agent that connects and leaves could upset). Neither step
   locks, as a window function cannot be combined with `FOR UPDATE` and locking before the limit
   would hold rows the limit drops; a last step locks the chosen rows with `FOR UPDATE SKIP
   LOCKED` and checks their conditions again. A row another pass holds is skipped, and that pass
   commits, so two instances cannot skip each other's rows for good; the rows are those whose state is `received`, which are neither
   finished nor delivered, whose start deadline has not passed, whose key is the active one and
   which have no mark or an expired one. It queues a frame for each through the hub while
   holding the row lock, and marks the rows it queued in the same transaction, with the time of
   the mark statement (not of the transaction), which is returned and kept with the delivery.
   Two instances therefore never hold one row at once; if a mark's commit fails after the frame
   was queued, the row is as it was before the send (no mark, or an expired one) and can be sent again
   (item 4), or, when the commit went through though it was reported as failed, is at the new
   mark; the take returns the earlier mark, and the mark statement the new one, so that the delivery
   keeps both until its commit is known (item 5). A row that cannot be queued (full queue, no connection) stays as it was for the next pass:
   an agent whose queue is full or which has left gets nothing more in this pass, so its later
   jobs keep their order. A frame over the limit can never be sent, unlike a full queue or a missing
   connection, which retry: it ends the job as failed
   with `internal` (the server is part of the diagnostic system); whether a frame fits is judged
   with the widest command ID the counter can produce, so that it does not depend on the
   counter's value.
3. **Three columns record the delivery; `state` stays the 5.6 state.** `delivery_attempts`,
   `delivery_sent_at` and `delivered_at` (migration 0007). A job is `received` until the agent
   reports a state, whatever its delivery. An acknowledgement sets `delivered_at`, and the job is
   never sent again. A job whose start deadline has passed is not taken; ending it is not the
   queue's decision.
4. **Delivery is at least once.** A mark that is not acknowledged is released (conditional on the
   attempt number, the mark's time and no acknowledgement) when its connection is no longer current, so the
   next connection gets the job, and expires after a lease of 90 seconds, so a job whose server
   went away is taken by another. A stop of the server releases nothing. A resend carries the
   same stored bytes under a new command ID with the attempt number raised; the agent's intake
   already tolerates seeing a job twice (ADR-473). The queue also remembers the deliveries in
   flight in this process, so that a mark whose commit failed after the frame was queued does not
   cause a second frame here, and an answer still settles it. An answer read while a write waited
   for the database is applied after that write, and at the latest on the next tick. An answer that was read after the
   snapshot by a connection that has since been replaced matches nothing and costs one duplicate,
   which the agent's tolerance of a repeated job accepts (design 5.3).
5. **An answer settles with conditional writes.** An acknowledgement, a rejection or a release
   writes only while the job is neither finished nor delivered and the row carries the mark of
   the delivery the answer is about. A delivery keeps the mark the row had when it was taken (null or
   an expired one; the comparison treats null as equal to null) and the mark its statement
   returned. After a commit that succeeded only the returned mark counts; after one that failed,
   which may still have gone through, either does. The attempt number is checked with the mark: the
   earlier mark goes with the attempts the row had before the delivery (n), the returned mark with
   n + 1, because a failed commit leaves one pair or the other and a row another delivery has since
   marked matches neither. So the answer of a delivery whose commit failed cannot clear or
   end the live delivery of another instance, and an answer for a job that is
   already finished or delivered writes nothing, and an answer is matched by the connection and
   the command ID of the agent it came from; an unknown, repeated or foreign one is ignored. A
   rejection (whatever its coarse reason) ends the job as failed with the new code
   `instructionRejected`, the agent's reason as the detail and `finished_at` set, and the job is
   not sent again (ADR-466 item 13); it is never `signatureInvalid`, which names a failure the agent
   reports itself. `agentEvent` and `reconcile` messages in the inbox are dropped
   with a debug line (ADR-472 item 4) until the outbox work.
6. **The active key is the database's.** The key a pass compares with is the active row of
   `instruction_keys`, not a value held in memory, so that two instances agree. A job whose
   `signing_key_id` is not that key, and whose instruction is not outstanding, is ended as failed
   with the new code `instructionKeyRetired`, connected agent or not, and is never re-signed
   (ADR-462, ADR-461 item 8). With no active row nothing is ended and nothing is taken.
7. **Failures are quiet and retried.** A failed pass sends nothing, logs one gated warning (at
   most one a minute) and is retried at the next tick; the inbox is drained either way, and also
   while the pass or a write waits for the database (the messages are buffered, up to 4096, and
   applied after it), so that the database never fills the inbox. A write of answers that fails
   keeps its step, which is written again at the next tick (a backoff: not on every message), and
   the take leaves those jobs out meanwhile; what was buffered during a write is applied right after
   it.
8. **Testing.** The dispatch (full queue, no connection, frame too large, the stop between two
   sends) and the settling of answers (snapshot before drain, unknown, repeated and foreign
   answers, a flight whose commit failed) are functions that need no database and are unit
   tested. The rest runs against PostgreSQL with WebSocket clients as agents and a second server
   for the two-instance cases, and the agent binary's own gate refuses a stored envelope with a
   flipped byte.

## Consequences

- A job is delivered a few seconds after the agent connects or the row appears: the pass is a
  poll. Waking the queue on a connect or a new row (design 14.1's note on `LISTEN/NOTIFY`) is
  left for later.
- A job on a connected agent that never answers is sent again every 90 seconds while the
  connection lives, since the lease expires and nothing says the agent has it; the agent's
  duplicate check absorbs it.
- Command IDs come from a counter of the hub, so two instances can use the same ID on different
  connections; an answer is matched by connection as well, so this does not matter.
- The job creation route, which writes the rows this queue reads, and the states the agent
  reports after `received` are separate work. Until the agent's events are consumed, a delivered
  job stays `received` in the table.
- The codes `instructionRejected` and `instructionKeyRetired` are the server's own and are added
  to the list of failure codes (design 5.6, `api/asyncapi.yaml`).

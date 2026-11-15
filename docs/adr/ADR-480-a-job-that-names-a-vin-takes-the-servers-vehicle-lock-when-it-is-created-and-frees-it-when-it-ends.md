# ADR-480: A Job That Names a VIN Takes the Server's Vehicle Lock When It Is Created and Frees It When It Ends

**Date:** 2026-11-15
**Status:** Accepted
**Affects:** design 5.1, 5.3, 8.8, 12; `db/README.md`; `api/openapi.yaml` (`POST /v1/jobs`); `crates/server` (`jobs`, `job_queue`, `problem`), `crates/server/docs/ngr-server.md`; `docs/glossary.md`; annotates ADR-477 item 7 and ADR-476

## Context

Design 8.8 lists a per-VIN soft lock on the server as one layer of exclusive control, and the first
schema has a table for it (`vehicle_locks`, one row per tenant and VIN, with an expiry), but
nothing wrote a row. `POST /api/v1/jobs` (ADR-477) said it took no lock and named where the lock
would go: inside the creation's transaction once the insert wins, never on a replay. Open were what
a second job for a locked vehicle gets, how long a lock lives for each kind of job, what a job with
no VIN does, which statement takes the lock and where in the order, and which statements release
it. Design 8.8 said the server records running jobs and warns.

## Decision

1. **A second job for a locked vehicle is refused, not warned about.** The answer is 409 with a
   Problem whose `code` is `VEHICLE_LOCKED` and which carries nothing else about the lock: no
   holder, no expiry, no VIN, no `Retry-After`. Nothing is created and the idempotency key stays
   unbound, so the same request can be sent again once the vehicle is free. There is no override.
   Design 8.8's server row says so.
2. **One holder per tenant and VIN.** The table's primary key already says it; two tenants may
   hold the same VIN.
3. **A job with no VIN takes no lock.** The server cannot name what it would lock; promoting a lock
   when an agent later reports the VIN is later work. A job that does name a VIN is held to the
   rule whatever its kind.
4. **The expiry is the start deadline plus an allowance for the kind**: 600 seconds for
   acquisition and 1800 seconds for a configuration write, constants in an exhaustive `match`
   beside the kind's confirmation level (`Kind::lock_secs`), so a kind the route learns to create
   cannot be added without choosing its allowance. Counted from the creation, a lock lives 25 and
   45 minutes at most. The column gets no default and no migration is needed.
5. **The take is one statement, after the job insert wins and before the agent and key locks**:
   `INSERT INTO vehicle_locks ... ON CONFLICT (tenant_id, vin) DO UPDATE ... WHERE
   l.expires_at <= now() RETURNING 1`. A new row, or an expired one replaced, returns a row; no
   returned row means a live lock of another job, and the transaction rolls back and answers 409.
   The replace condition reads only the lock row, so a concurrent take of the same VIN waits for
   the other transaction and re-checks what it left, and the one statement is the whole decision
   (no read, then write). Taking it before the `FOR SHARE` locks keeps a wait on another take from
   holding the agent's row, so the agent's connect stamp is not blocked. A wait past the 5 second
   lock timeout is 503. A replay answers 202 before any lock is read (ADR-477 item 4), so a job
   whose lock has expired or been taken over still replays. ADR-477 item 7's order gains this step
   between the insert and the two locks.
6. **A job's end deletes its own lock row inside the statement that ends it.** Each statement in
   the delivery queue that sets `jobs.finished_at` (the sweep of a retired key, the end of an
   instruction too large to send, a rejection) is `WITH ended AS (UPDATE ... RETURNING tenant_id,
   id, vin) DELETE FROM vehicle_locks l USING ended e WHERE l.tenant_id = e.tenant_id AND
   l.vin = e.vin AND l.job_id = e.id`, so the lock goes in the transaction that ends the job and
   only when the row names that job: another job's lock on the same VIN stays, and an answer that
   ends nothing frees nothing. The sweep counts the jobs it ended, not the rows it deleted.
7. **No sweeper.** An expired row is replaced by the next take for its VIN, which is the only
   place a lock is read. A stale row stays until then and keeps its VIN.
8. **The refusal is logged without the VIN or the holder**, through its own once-a-minute gate.

## Consequences

- Until the server also frees the lock when it is told that a job reached a terminal state
  (the job's reported state), a job that finished on the agent holds its vehicle until its expiry.
- A job that outlives its allowance loses its lock to the next job; nothing renews it.
- A stale row keeps a VIN, so a deletion by VIN (design 4.3, 15.2) has to cover `vehicle_locks`.
- The lock does not cover a job with no VIN, offers no override and no view of the holder.
- ADR-477's consequence that the route takes no lock is replaced by this decision; its other
  consequences stand. ADR-476's ends also release the lock.

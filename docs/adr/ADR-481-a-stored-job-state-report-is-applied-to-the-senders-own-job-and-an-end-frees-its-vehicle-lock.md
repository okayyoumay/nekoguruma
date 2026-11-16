# ADR-481: A Stored Job-State Report Is Applied to the Sender's Own Job, and an End Frees Its Vehicle Lock

**Date:** 2026-11-16
**Status:** Accepted
**Affects:** design 4.3, 5.1, 5.3, 5.6, 8.8; `db/migrations/0010_job_procedure_code.sql`, `db/README.md`; `api/openapi.yaml` (`Job`, `POST /v1/jobs`), `api/asyncapi.yaml` (`agentEvent`); `crates/server` (`agent_events`, `job_state`, `job_queue`, `jobs`), `crates/server/docs/ngr-server.md`; `crates/shared-proto` (`JobState`, `FailureCode`); `docs/glossary.md`; details ADR-479 item 7, replaces ADR-480's first consequence and ADR-476's consequence that a delivered job stays `received`

## Context

The server stores each `jobState` event an agent sends (ADR-479) and changes no job: a job stays
`received` whatever the agent reports, so no job ever ends on the server's side except by the
server's own ends, and a vehicle lock (ADR-480) outlives a job that finished on the agent until
it expires. ADR-479 left open how a stored state is applied, and said only that it belongs in
the storing transaction and only for an event stored as the next number. Open were which reports
change which job, what a report that is not applied does, which columns an end writes and how
the agent's text is kept safely in them, whether the job's lock is freed, which time the row
takes, how this meets the answers to a delivered command that are written before the events, and
what the operator's `Job` shows of a failure.

## Decision

1. **A report is applied in the transaction that stores it, to the sender's own job only.**
   After the event's insert, the transaction locks the job row named by the event's job ID with
   the same tenant and `agent_id` equal to the sender (`FOR UPDATE`), applies one write chosen
   by a pure function of the row's state and the report, and goes on. A job that does not exist,
   belongs to another agent or tenant, or whose ID in the event is not a canonical UUID is
   not touched. A repeat is never applied again, since only the next number is stored.
2. **The server keeps no copy of the 5.6 transition table.** The order the agent's outbox
   gives is the order of events; while a job is open, the last report wins; the first end is
   final; and the agent's `needsOnSiteRecovery` is the one end a later report may replace.
3. **The writes.**
   - `received` stamps `last_reported_at` and, when it is null, `delivered_at`, and never
     moves the state: the agent sends one for every command it accepts, so it is a proof of
     delivery and not a state of the job.
   - An open job that is reported in another non-terminal state takes that state and both
     stamps; in the state it already has, only `last_reported_at` moves.
   - An open job reported in an end (completed, failed, cancelled, expired,
     preconditionFailed, needsOnSiteRecovery) takes the state, the failure columns, the end time
     `finished_at` and both stamps, and the statement deletes the job's own vehicle lock in the
     same statement (the shape of ADR-480's ends: the lock row with this job's ID, and no other).
   - A job that ended as needsOnSiteRecovery accepts any later report but `received` (O1): a
     non-terminal state reopens it (the end time and the failure columns are cleared, the lock
     is not taken again), and an end replaces the earlier end. An end of every other kind,
     including every end the server wrote itself, is final: a later report is stored, not applied,
     and logged.
   - A state name that is not one of 5.6's is not applied.
4. **A report that is not applied is still stored and acknowledged, and the cursor moves** (O2).
   The agent must not resend it for ever. The queue logs a gated warning after the commit with
   the agent ID, the job ID, the number and the two states' names (the job's and the reported
   one, from the enumeration, `unknown` for a name that is none); never a frame's text or a
   detail. A late `received` is routine and not logged.
5. **Field rules**, so that nothing an agent sends can make the transaction fail for good.
   `failure_code` is written only for failed and preconditionFailed; a code the agent may send
   is kept, and an absent, unknown or server-only code (`instructionRejected`,
   `instructionKeyRetired`) is `other`. `procedure_code` (a new column) is kept only with the
   failure code `procedure`, 1 to 128 bytes without U+0000, and is otherwise null, never cut.
   `failure_detail` is kept only for failed, preconditionFailed and needsOnSiteRecovery: U+0000,
   which `text` refuses, becomes U+FFFD, the text is cut to 1024 bytes on a character boundary,
   and an empty one is null. No CHECK constraint is put on these columns: a bound in the column
   would turn a bad report into a failure of the agent's whole transaction. `progress_permille`
   is not touched.
6. **The times are the server's.** `last_reported_at`, `delivered_at` and `finished_at` are the
   transaction's `now()`; `occurredAtAgent` is not used for the job (O4, design 16.3).
7. **Answers stay before events, and no order between them is relied on.** The writes a
   delivery's answer makes (acknowledgement, rejection) are conditional on the row (delivered_at
   null, state `received`, not finished), and the report's writes are conditional on the job's
   state, so each order of a report and an answer to the same command leaves one consistent row.
   A `received` report settles the delivery without a `commandAck`; the queue's guards then
   take the job out of the queue, and a late `commandAck` writes nothing.
8. **The operator's `Job` shows the failure code and the procedure code, not the detail**
   (O3): `failure: {code, procedureCode}` when the job has a failure code. The detail is
   vehicle data (design 4.3), and the route replays without the permission check the result
   needs (ADR-477 item 5); it stays in the row for a read route.
9. **Nothing is replayed.** Events stored before this change changed no job and are not
   applied afterwards. No audit row is written for the change.

## Rejected

- **The agent's times for the job.** They are for reference only (design 16.3); a clock that is
  wrong would show as a job that ended before it began.
- **A copy of the 5.6 transition table at the server.** The agent is the one that knows the
  procedure; a second table has to be kept in step and rejects reports the agent must be
  believed on.
- **Closing the connection for a report that cannot be applied.** The report was
  numbered and stored; a close would make the agent resend it, to be refused again, and would
  cut the agent off from the events behind it.
- **A CHECK on `failure_code`, `procedure_code` or `failure_detail`.** See item 5.
- **The detail in the operator's `Job`.** See item 8.

## Consequences

- A job that finished on the agent now frees its vehicle at the report, not at the lock's
  expiry; this replaces the first consequence of ADR-480. A job the server ended itself, or
  one with no VIN, is as before. A reopened job runs with no lock of the server's; a lock that
  another job took in the meantime stays, since an end deletes only its own row.
- A delivered job no longer stays `received` in the table: the agent's reports move it
  (ADR-476's consequence replaced). A job whose `received` report never arrives is still sent
  again after its lease.
- A job the server has ended (rejection of a command, a retired key) that its agent later
  reports on is only logged; the two views then differ, and nothing reconciles them yet.
- A reopened needsOnSiteRecovery job has no lock and no deadline of the server's.
- The retention pass and the VIN deletion (design 4.3, 15.2) have to cover
  `jobs.failure_detail` and `jobs.vin` together with the stored events.
- Two server instances serve one agent in turn (the cursor lock) and the job row lock follows it,
  so no two instances lock these rows in opposite orders; a lock wait that times out fails the
  store, and the queue retries on its tick.

# ADR-477: A Job Is Created Once per Idempotency Key, With the Key Judged Before Permission and the Server Making the ID

**Date:** 2026-11-14
**Status:** Accepted
**Affects:** design 4.4, 5.1, 12; `db/migrations/0008_job_creation.sql`, `db/README.md`; `api/openapi.yaml` (`POST /v1/jobs`, `JobRequest`); `crates/server` (`jobs`, `register`, `lib`), `crates/server/docs/ngr-server.md`; `crates/shared-proto` (`is_well_formed_vin`), `crates/agent` (`journal`); details ADR-461 and ADR-476 for the stored instruction

## Context

Nothing created a job: the `jobs` table, the signed instruction (ADR-461) and the delivery queue
(ADR-476) existed, and a row came from a test. Design 4.4 said IDs are client-generated with an
idempotency key attached, the first schema's comment on `jobs.id` said the same, and `jobs` had one
`ir_digest` text column that nothing read while a request names a list of documents. The route has
to decide who makes the ID, what an idempotency key promises when two requests, two operators or two
servers use it, what permission a creator needs, which requests it refuses for now, and how a
concurrent insert of one key ends without a second job or a broken transaction.

## Decision

1. **The server makes the job's ID, and the client's token is the idempotency key.** The ID is a
   UUIDv7 built from the database's clock in milliseconds and ten random bytes; no date or UUID
   crate is added. Design 4.4 and the comment on `jobs.id` say so.
2. **Only acquisition and configuration writes are created.** The other five kinds are 400 "not
   supported". A request that sets `remote: true`, a non-empty `parameters`, an
   `executionWindow`, a `startDeadline` or an `onSiteContact` is 400 as well; `remote: false` and an
   empty `parameters` count as absent. A `startDeadline` cannot be parsed without a date
   dependency, and the others have no meaning to the server. The start deadline is 15 minutes after the
   instruction is issued. The digests are taken as named: at least one, at most 128, in any order
   and with repeats, stored sorted and unique. The confirmation level is `notifyOnly` for
   acquisition and `gracePeriod` for a write, recorded and not enforced. 400 means the request can never be valid, 422 only that the key is
   another request's.
3. **The caller needs the role `worker` with an empty scope**, the agent in the caller's tenant,
   unrevoked and listing the caller among its allowed operators, and the process's key must be the
   active `instruction_keys` row. A refusal is one 403 document whatever the reason, which goes to
   the log through its own gate; a key that is not the active one is 503.
4. **The idempotency key is the tenant's and lives as long as the row.** It is required (400),
   1 to 128 visible ASCII bytes, compared as bytes. The same operator repeating the same request
   gets 202 with the stored job; any other use is 422 with no data of the job, whoever created it.
   A replay shows the job's current state and writes nothing.
5. **The key is judged before the permission.** A replay is of a job this operator created, so it
   needs no permission the creation needed once; a conflict is the same 422 for a caller who may and
   one who may not create, so it reveals nothing about the job.
6. **`request_hash` decides sameness.** A new nullable `bytea` column (32 bytes by a CHECK) holds
   the SHA-256 over a tagged, length-prefixed, fixed-order encoding of the normalized request: the
   kind, the agent, the VCI, the VIN (presence and value), the sorted unique document hashes and
   the artifact (presence and value). The operator is not in it; the row's operator is compared. A
   row with no hash is never replayed. A property added later enters the encoding only when the
   request has it, so earlier hashes stay valid.
7. **One transaction at read committed, in a fixed order.** Read the key's row; if there is none,
   read the permission and the active key in one statement; build and sign the instruction with the
   database's second (rounded down) as its issue time; lock the active key's row and the agent's
   unrevoked row `FOR SHARE`, so a retirement or a revocation waits for the insert (a row gone at
   that point is the key-not-active 503 or the revoked 403); `INSERT ... ON CONFLICT (tenant_id, idempotency_key) DO
   NOTHING RETURNING`. A returned row commits and answers 202. No row means a concurrent insert won:
   the row is read again, with a new statement's snapshot, and decided as in item 4; a row that is
   gone again is 503. A lock wait is bounded by the 5 second `lock_timeout` (503). One function
   builds the `Job` answer from a row for a creation and a replay, with `Cache-Control: no-store`
   and no `Location` (the route has no `GET /v1/jobs/{jobId}` to point to).
8. **The schema changes in migration 0008:** `request_hash`; `ir_digests text[] NOT NULL DEFAULT '{}'`
   replacing `ir_digest` (the sorted unique hex list is what is written); `jobs_kind_check` dropped
   and added again with `identify`, as migration 0004 did for the agent mode; a comment on
   `jobs.id`.
9. **The VIN rule is shared.** `is_well_formed_vin` moves into `shared-proto`; the agent's
   guards delegate to it, so the server refuses what the agent would refuse.

## Consequences

- A replay can show a job to an operator who could no longer create it (item 5).
- The instruction is signed before the insert, so one is discarded when its insert loses to a
  concurrent request for the same key.
- A `gracePeriod` is a recorded level and not a wait.
- Design 4.4 and the comment on `jobs.id` no longer call a job's ID client-generated.
- The route does not take the vehicle lock (8.8), writes no audit record and cannot set a start
  deadline, a window or an on-site contact. Each of these replaces one named place in the route when
  its own work lands: the lock inside the transaction after the insert wins and never on a replay,
  the audit record beside the insert, the start deadline and the window in the request and its
  hash, and seed data for the other kinds in the kind check.

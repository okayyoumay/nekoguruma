# ADR-484: A Created Job Writes One Audit Row, Copied From Its Row in the Creating Transaction

**Date:** 2026-11-17
**Status:** Accepted
**Affects:** design 4.3, 5.1, 8.2.3; `db/README.md`; `crates/server` (`audit`, `jobs`), `crates/server/docs/ngr-server.md`; annotates ADR-477 item 7 and its last consequence

## Context

`POST /v1/jobs` (ADR-477) wrote no audit row, though design 8.2.3 makes `audit_log` the record of
what a job did and the milestone's exit criterion asks for a row per created job. The table
(migration 0001) has columns for the operator, agent, job, VIN, action, confirmation level and a
JSON detail, and a default partition (migration 0011) takes the rows. Open: which action name and
which detail, whether the VIN may be in the detail, which creations write a row, where in the
transaction it goes, and what a failed write does.

## Decision

1. **The action is `jobCreated`, as free text.** `audit_log.action` has no CHECK, so a new action
   needs no migration. `db/README.md` lists the actions written and what each row holds.
2. **The row is copied from the job's row by the database.** An `INSERT ... SELECT ... FROM jobs`
   for the job just inserted writes the tenant, operator, agent, job, `vin` (NULL when the job has
   none) and confirmation level, so the row cannot differ from the job. The id, `recorded_at`
   (equal to the job's `created_at`, one transaction having one `now()`) and `offline_executed`
   take their defaults; `on_site_approved` and `agent_reported_at` stay NULL.
3. **The detail holds the kind, `irDigests` (sorted, unique), `artifactDigest` when there is one
   and `signingKeyId`.** It never holds the VIN or a value derived from it, the request hash (a
   hash over a request that includes the VIN can be tested against a known VIN), the idempotency
   key or the VCI ID. The VIN is in the `vin` column only, the one place a VIN deletion redacts
   (design 4.3).
4. **Only a creation that inserts a row writes one.** A replay writes none, and neither does any
   refusal: the 422, 403, 409 and 503 end before the insert or roll it back with it.
5. **It is the first statement of the winning arm,** after the job insert returns a row and
   before the vehicle lock is taken. The order of ADR-477 item 7, with ADR-480's take, becomes the
   insert, the audit row, the vehicle lock, the agent's lock, the key's lock, the commit.
6. **A failed audit insert fails the creation.** An error, or a count other than one row, rolls the
   transaction back (500 or 503 like any statement); the key stays unbound and the client may
   repeat. A job is never committed without its record.
7. **Append-only enforcement is out of scope.** The server only inserts; the table's grants still
   allow update and delete. The mechanism is a separate decision, since the VIN redaction must
   change `vin` and the retention pass drops partitions.

## Consequences

- ADR-477 item 7's order gains the audit insert (item 5 above), and its consequence that the route
  writes no audit record no longer holds. It still sets no start deadline, window or on-site
  contact.
- The creation depends on `audit_log`: a lock on the table, or an attach of a range partition
  that waits on the default partition, makes creations wait and, past the 5 second lock timeout,
  answer 503 with nothing created and the key unbound.
- A job without a VIN keeps a NULL `vin` in its row; a later promotion to the vehicle lock writes
  its own row and does not edit this one.
- The detail is about 9 KiB at most (128 digests of 64 characters), plus the key ID.
- A VIN deletion must redact `audit_log.vin`, and also `jobs.vin`, `jobs.request_hash` and
  `jobs.signed_job`, which hold the VIN or a value derived from it.
- Rows for a job's end, approval and cancellation do not exist yet; only the start is recorded.

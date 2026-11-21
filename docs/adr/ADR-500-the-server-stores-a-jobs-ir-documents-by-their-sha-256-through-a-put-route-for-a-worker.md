# ADR-500: The Server Stores a Job's IR Documents by Their SHA-256 Through a PUT Route for a Worker

**Date:** 2026-11-21
**Status:** Accepted
**Affects:** design 6.8, 8.2.6, 11.1, 14.3; `crates/server` (`documents`, `storage`, `storage/s3`, `audit`, `register`, `uploads`, `lib`), `db/migrations/0015_document_extension_id_nullable.sql`, `db/README.md`, `api/openapi.yaml`, `crates/server/docs/ngr-server.md`; fulfils the writer that ADR-497 leaves to a later route; extends ADR-454 and ADR-492

## Context

A job names its IR documents by SHA-256 in its signed instruction (ADR-454, ADR-461), the agent
fetches them through a presigned GET (ADR-497), and the download route finds them in
`ir_documents` by tenant and digest. Nothing wrote that table: tests seeded it. ADR-497 recorded
that the writer must insert a row only for an object the store has read back (ADR-492). The
document comes from the author's conversion (design 8.4) and, until package ingestion exists
(M6), nothing else puts it on the server. The maintainer decided the choices below on 2026-11-21
from a case table of the route's answers.

## Decision

1. **One route, `PUT /api/v1/documents/{sha256}`, raw body.** The client names the hash in the
   path; the body is the document's bytes. The path is 64 lowercase hex digits, the body is
   judged against it, and the answer repeats the name as `{"sha256": ...}` with
   `Cache-Control: no-store` and no `Location`.
2. **The body goes through the server, up to 16 MiB, two stores at a time.** Design 14.3 sends
   captured data and artifacts straight to the store; a document is small, and the server must
   parse it before it keeps it, so this is the exception. The bound is
   `shared_proto::MAX_DOCUMENT_BYTES`, the same the agent's fetch holds. A third concurrent
   store is 503 with `Retry-After`, since each holds its body in memory. The body gets its own
   time budget, 30 seconds (the small routes' 5 seconds do not suit 16 MiB).
3. **The store call is a new `MultipartStore::put_part`, then the existing `complete`.** The
   server begins an upload, sends the body as part 1 itself and completes it, which reads the
   object back and compares its length and SHA-256 (ADR-492). The last part is exempt from the
   5 MiB minimum, so one part is the whole object. A failure after `begin` deletes the object.
4. **Permission: the role `worker` with an empty scope**, the role job creation needs (design
   6.8, ADR-477). The agent's allow-list does not carry over: the route names no agent. The
   permission is judged before the body is read.
5. **A document belongs to a tenant.** Each tenant has its own row and its own object;
   `UNIQUE(storage_key)` (migration 0014) forbids two rows sharing an object, and a tenant's
   delete or retention rule then cannot reach another's.
6. **The document is validated and its row is filled from it.** `diag_ir::declaration::
   load_ecu_document` must accept the body (identifier, structure, schema version, values). The
   row takes the schema version, the variant's name, part numbers and software versions, the
   source format and the source digest. The document carries no package ID, so migration 0015
   drops NOT NULL on `ir_documents.extension_id` and the route leaves it NULL: a placeholder
   would look like a real package's ID.
7. **Metadata is bounded.** A variant without a name, a string over 256 bytes or with a control
   character (a NUL would be a database error, so a 500), and more than 1024 part numbers or
   software versions are refused as not a document.
8. **Refusals are 400 with codes.** `DIGEST_MISMATCH` (the body does not hash to the name) and
   `INVALID_IR_DOCUMENT`. A 400 means the request can never be valid; a problem never repeats a
   client value, so neither carries the name.
9. **Idempotent: 201 for the store that wrote the row, 200 for any other.** A tenant that
   already holds the digest gets 200 with no parse, no store call and no write, so a row whose
   object was lost is not repaired by a second store (the download fails at the object server).
10. **A race has one winner.** The row is inserted with `ON CONFLICT (tenant_id, digest) DO
    NOTHING`; the loser deletes its own object, whose key no row holds, and answers 200. No
    transaction is open across a store call: the existing-row check and the insert with its audit
    row are transactions of their own.
11. **The key is guarded against the other two tables.** The insert is conditional on the key
    being in neither `artifacts` nor `job_objects`. The key always comes from this request's
    `begin`, so a hit cannot happen unless the store reissues a key; it is a 500 and the object is
    not deleted, since it may be another row's. The guard is one-directional: the upload route's
    insert does not look at `ir_documents`, and a key table shared by all three is the full fix.
12. **The store is audited.** A `documentStored` row of `audit_log` (tenant, operator, and in the
    detail the digest and the size) is written in the insert's transaction, since
    `ir_documents` records no importer. A store that finds the row writes none.
13. **A stored document is inert until a signed instruction names it** (ADR-454 item 2). The
    route does not make a document run; it makes one available to a job.

### The route's answers

The first row that applies answers.

| Row | Case | Answer |
|---|---|---|
| R1 | The request guard refuses, or the method is not PUT | the guard's answer; 405 |
| R2 | No pinned Origin; no or a bad session cookie | 403; 401 |
| R3 | The path is not 64 lowercase hex digits | 400, body unread |
| R4 | The Content-Type is not exactly one `application/octet-stream` | 400 |
| R5 | The declared length is over 16 MiB (exactly 16 MiB passes) | 413 before a byte |
| R6 | No `worker` role, or a scoped one | 403, body unread, the reason logged |
| R7 | A third concurrent store | 503 with `Retry-After` |
| R8 | The body grows past the bound, stalls, or breaks | 413, 408, 400; the connection closes |
| R9 | The body's hash differs from the name, whether or not a row exists | 400 `DIGEST_MISMATCH`, nothing stored |
| R10 | The tenant holds the digest | 200, no parse, no store call, no write |
| R11 | The body is not a document the loader accepts (the empty body included) | 400 `INVALID_IR_DOCUMENT` |
| R12 | A valid document with metadata out of bounds | 400 `INVALID_IR_DOCUMENT` |
| R13 | The server has no object store | 503 |
| R14 | `begin` fails | 503 (500 without randomness), no row |
| R15 | `put_part` fails | 503, the object is deleted, no row |
| R16 | `complete` fails (a mismatch has deleted the object already) | 503, the object is deleted, best effort, no row |
| R17 | The insert wins | audit row, commit, 201 |
| R18 | A concurrent store won | the loser's object is deleted, 200; one row, one object |
| R19 | The key is in `artifacts` or `job_objects` | 500, no row, no delete |
| R20 | A statement fails after the read-back (the audit insert) | rollback, the object is deleted, 503 or 500 |
| R21 | The commit fails with the outcome unknown | 503, the object is kept |
| R22 | Another tenant holds the digest | 201, with a row and an object of its own |
| R23 | The row exists and its object is lost | 200, no repair |
| R24 | On the real object server, a small and a 16 MiB document | 201; the object equals the body; the download route signs it |

## Consequences

- **A worker can store a program and name it in a job.** The only bound on what it can do is the
  agent's own: a read-only release build runs no writing program and a debug build runs only on a
  simulator (design 6.7, `crates/agent` policy). Before M6 lifts that ceiling the route must
  require the distribution signature (design 11.1) or the administrator role, so that
  ADR-454 item 4's second check exists. ADR-454 item 5 rejected ingesting unsigned documents
  because other OS users on the PC could put a program before a vehicle; this route is behind
  the operator session, the pinned Origin and a role, and a stored document runs only when the
  signed instruction names it, but it does not replace that rejection for the M6 ceiling.
- **Job creation still takes the digests it is given** (ADR-477 item 2); a job may name a
  document no one stored, and its agent's fetch then gets the download route's 404. Checking at
  creation that each named document is stored, and the artifact too, is a later change. It is
  point-in-time and safe only while nothing deletes `ir_documents` rows.
- Jobs created before the route existed that name a document are left as they are.
- Identification plans' documents (ADR-395) are not stored by this route.
- A request cancelled while the store is called (the client leaves) can leave an object that no
  row names; nothing collects it yet. A commit whose outcome is unknown (R21) keeps its object,
  for the same reason: deleting an object a committed row names is the worse error, and a later
  store of the same bytes finds the row or makes its own object.
- The agent's check that the instruction's document set equals what it holds before any link
  opens belongs with job start, not with this route.
- Memory is up to 16 MiB for each of two stores, and the hash and the parse run off the
  runtime's threads.

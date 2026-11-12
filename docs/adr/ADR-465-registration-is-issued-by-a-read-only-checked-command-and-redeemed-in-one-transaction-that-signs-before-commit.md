# ADR-465: Registration Is Issued by a Command That Checks Migrations Read-Only, and Redeemed in One Transaction That Signs Before It Commits

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 6.4, 6.7, 14.4; `db/migrations/0005_agent_registration.sql`, `db/README.md`; `crates/server` (`registration`, `db::require_current`, the `registration-code` command in `main.rs`, `crates/server/docs/ngr-server.md`) and the redemption route (later work); details ADR-463 items 5 and 14; relies on ADR-458 (the command never migrates), ADR-462 (the active instruction key) and ADR-456 item 7 (`--config` stays)

## Context

ADR-463 fixed what registration is: a single-use code issued by a server command, bound to an
owner and an agent key, redeemed by a signed request and answered with a signed response. Building
the server's side left questions that the ADR states in one line each or not at all:

- The partial unique index on `agents.public_key` (item 14) meets databases that already hold
  duplicate unrevoked rows of one key, and says nothing about tenants: a key is one identity, but
  the table is keyed by tenant.
- The command "never migrates" (item 1), yet sqlx's only way to compare applied and embedded
  migrations, `Migrator::run_direct`, creates its bookkeeping table.
- The refusal limit (item 5) does not say when it is consulted or what it counts over.
- The order of checking, signing and committing decides what a failure leaves behind.
- An owner can be disabled between issuance and redemption.

## Decision

1. **Issuance is one transaction, and the code is printed after the commit.** The command reads
   the active instruction key, finds exactly one enabled operator with the owner's subject, and
   inserts the row with `expires_at = now() + the lifetime`, guarded in the statement itself by
   the key still being active. `created_at` and `expires_at` come from the same database `now()`,
   so the lifetime is exact. Only the SHA-256 of the code is stored, and the code is never logged;
   a failed write to standard output after the commit leaves a row that expires unused. A subject
   that names no operator, a disabled one, or operators in more than one tenant (the subject is
   unique per tenant only) are three refusals with three fixed messages.
2. **The command checks the migrations read-only and refuses unless they equal the binary's.**
   `db::require_current` reads `_sqlx_migrations` (found by `to_regclass`, so an unmigrated
   database is not touched) and compares version, checksum and success flag with the embedded
   migrations both ways: behind, ahead, a changed file and a half-applied one each refuse. It
   reads committed state and takes no lock, so a server that is applying migrations at that
   moment makes the command refuse; rerunning after the start passes. A test pins the hard-coded
   table name against the migrator's own.
3. **One unrevoked `agents` row per key in the whole database.** The index is partial on
   `public_key WHERE revoked_at IS NULL` and spans tenants: a registration in one tenant revokes
   the key's row in another. Only the holder of the key can cause that, since the proof of
   possession is required. `agents.owner_id` is nullable; it is null only on rows that predate
   registration.
4. **The migration revokes all but the newest.** Where a database already holds several
   unrevoked rows of one key, migration 0005 keeps the newest by `(registered_at, id)`, sets
   `revoked_at` on the others and reports the count with a warning, then creates the index.
   Already revoked rows are untouched.
5. **Redemption verifies the proof before the database and signs before the commit.** The
   request's shape and the proof are judged without any database lookup. The redemption then runs
   in one transaction that consumes the code by one conditional update, revokes the key's earlier
   rows, inserts the agent and its allowed-operator row, records the agent ID on the code, and
   signs the response (ADR-463 item 21) before it commits. Any failure before the commit rolls
   back and leaves the code redeemable; a 5xx is not a refusal. The owner must still be enabled at
   redemption: a disabled owner is refused, uniformly with every other refusal.
6. **The refusal limit is applied after the verdict.** It is a sliding window over the last ten
   refusals (403) of one router, on a monotonic clock. A request that redeems never consults it
   (ADR-463 item 5), and a 5xx is not counted. Each refusal's reason is logged with one log gate
   per reason, so one flood of a reason cannot silence another.
7. **The command line.** `registration-code` is a subcommand only as the first argument; any other
   position is the serving form, so `ngr-server --config registration-code` still serves
   (ADR-456 item 7 holds). Its flags may come in any order, each at most once, as `--flag value`.
   Values are validated before any connection: an invalid command line is exit 2. Exit 0 is an
   issued code, 1 a refusal or failure. Standard output is one JSON line (`code`, `expires_at`,
   `server_key_id`); errors are `ngr-server: <fixed text>` on standard error and never repeat the
   configuration's URL or the code.
8. **The key file is not part of issuance.** The command checks the database's active row only; it
   neither reads nor creates the key file, so it works while the server is stopped and cannot
   start a second key identity. A row with no file is the server's refusal at its next start
   (ADR-462), not the command's.

## Redemption design

The route is `POST /api/v1/agents/register` (path `/v1/agents/register` under the base `/api` of
`api/openapi.yaml`). The tables are the cases its tests cover; "refused" is the one uniform 403.

### Request order

The first row that applies decides. None of these steps touches the database or the limiter.

| # | Case | Outcome |
|---|---|---|
| H1 | The request guard rejects (ADR-456) | the guard's answer; the handler never runs |
| H2 | A method other than POST | 405 with `Allow`, as the health route |
| H3 | The body is not complete within 5 s | 408 (the first route that reads a body; ADR-456 item 6 bounds only the head) |
| H4 | More than 4096 bytes (exactly 4096 passes) | 413, nothing parsed |
| H5 | Content-Type is not `application/json` (parameters allowed) | 400 (not 415) |
| H6 | Not JSON, not an object, a missing, unknown or duplicate field, a non-string value, bytes after the value | 400 |
| H7 | `code` over 64 characters, or `RegistrationCode::parse` refuses it | 400 |
| H8 | `publicKey` over 512 characters, or not a parsable SPKI PEM | 400 |
| H9 | `mode` is not exactly `user` or `device` | 400 |
| H10 | `proof` over 128 characters, not strict standard padded base64, or not 64 bytes | 400 |
| H11 | Any of H3 to H10 while the database is down or the limit is exceeded | the same answer |
| H12 | Typed variants of one code (case, I, L and O, outer whitespace) | one hash; it redeems |

### Verdict

| # | Case | Outcome |
|---|---|---|
| V1 | A 64-byte proof that is no valid signature pair, is high-S, is by another key, or covers another mode, code or server key ID than this server's in-memory key | refused before any database access; the code is untouched |
| V2 | The hash is not stored | refused |
| V3 | `used_at` is set | refused |
| V4 | `revoked_at` is set | refused |
| V5 | `expires_at` is not after the database's transaction-start `now()` | refused |
| V6 | The code is bound to another agent key | refused; the right key still redeems it afterwards |
| V7 | The code's instruction key is retired in the database, or is not this server's in-memory key | refused |
| V8 | The owner is disabled since issuance, or is not in the code's tenant | refused |
| V9 | Everything holds | 201 |
| V10 | Refused with fewer than ten counted refusals in the window | 403, byte-identical for every reason; counted |
| V11 | Refused with ten counted | 429 with `Retry-After`; not counted |
| V12 | Redeemed, in any limiter state | 201; the limiter is not consulted |

A 201 has these effects in one transaction: the code row gets `used_at` and `agent_id`; the key's
earlier unrevoked rows are revoked in every tenant; a new `agents` row is inserted (ID from
`gen_random_uuid()`; tenant, owner and display name from the code row the consuming statement
returns; mode from the request; `public_key` the canonical DER SPKI; preset `personal` and
`allow_remote` false taken from the same Rust values that are signed, not from column defaults;
`registered_by` and the host hints null); one `agent_allowed_operators` row is inserted; no
`audit_log` row. The body carries `agentId`, `ownerId`, `preset`, `allowRemote`, `instructionKey`
(the active key's SPKI PEM), `instructionKeyId` and `signature`, which verifies against that PEM
whose fingerprint is `instructionKeyId`. The server key is read once per request and the ID, the
PEM and the transcript all derive from that one value.

### Concurrency and failure

| # | Case | Outcome |
|---|---|---|
| C1 | One code twice at once, including a replay of the captured request | exactly one 201; the other waits on the code row, then is refused as used |
| C2 | The first of C1 rolls back | the second redeems |
| C3 | The same key with a new code | 201 with a new agent ID; the earlier rows are revoked before the insert; their allowed-operator rows stay |
| C4 | An earlier row in another tenant | revoked too |
| C5 | A different key after a key reset | the old row is untouched (ADR-463 item 14) |
| C6 | Two codes for one key at once | both 201, serialised by the per-key lock; one unrevoked row at the end |
| C7 | The response is lost, the client disconnects or a shutdown aborts | the transaction is wholly committed or rolled back; if committed the same code is refused, the agent needs a new code, and the orphan row is revoked by the key's next registration |
| F1 | The database is unreachable or the pool is exhausted | 503, not counted, the code untouched |
| F2 | A statement error, or the 5 s lock or statement limit, mid-transaction | rollback; 503; the same code still redeems |
| F3 | Signing the response fails | fails before the commit: rollback, 500, the code still redeems |
| F4 | The outcome of the commit is unknown | 503, then as C7; accepted (ADR-463 item 15) |

### The limiter

| # | Case | Outcome |
|---|---|---|
| L1 | Refusals 1 to 10 inside 60 s | 403 each |
| L2 | The eleventh would-be refusal within 60 s of the oldest of the last ten | 429; `Retry-After` is the seconds until that oldest counted refusal is 60 s old, rounded up (1 to 60); a 429 is not recorded |
| L3 | The oldest ages out | the next refusal is 403 again |
| L4 | 429, 400, 405, 408, 413, 5xx, guard rejections and 201 | never counted |
| L5 | Many concurrent refusals with nine counted | exactly one 403, the rest 429 |

The state is a mutex around a queue of at most ten instants in the route's own state, one per
`router()` call (not a static, since several servers share a test process). It is never held across
an await and its poisoning is ignored, as in the health route. Every request is still evaluated:
the limit changes the answer, not the work.

### The transaction (read committed)

Locks are taken in the order: code row, per-key advisory lock, agent rows.

1. Set the transaction's `lock_timeout` and `statement_timeout` to 5 s (`set_config` with local
   scope).
2. Consume the code with one UPDATE that sets `used_at = now()` where the hash matches, it is not
   used or revoked, its expiry is after `now()`, its bound key fingerprint equals the request's,
   its instruction key ID equals the in-memory key's ID the proof was verified with, that key is
   still active in the table, and its owner row exists in the code's tenant and is not disabled;
   it returns the tenant, the owner and the display name. Zero rows means refused: one classifying
   SELECT of the row's flags (for the log only; order: absent, revoked, used, expired, other key,
   instruction key, owner, otherwise "state changed") and a rollback.
3. Take `pg_advisory_xact_lock` in its two-int4 form: a fixed class constant and the first four
   bytes of the key's fingerprint. It is transaction-scoped (a session lock would stay held on the
   pooled connection), and the two-int4 form cannot collide with sqlx's single-bigint migration
   lock. It comes after the code UPDATE and before the revoke, so two registrations cannot wait on
   each other in a cycle.
4. Revoke the key's unrevoked `agents` rows by `public_key`, with no tenant filter.
5. Insert the agent, returning its ID.
6. Insert the owner's `agent_allowed_operators` row.
7. Record the agent ID on the code row.
8. Sign the response transcript, then commit.

### Status mapping and logging

The order is guard, 405, 408, 413, 400, proof, transaction. A redemption is 201; a refusal is 403
while the limiter has room and 429 otherwise; 503 is an unavailable pool or connection, or a
statement failure or timeout; 500 is the signer or anything else. Content-Type that is not JSON is
400. Each refusal is one warning line with a fixed reason word and the code's redacted `Debug`,
through a log gate per reason (one minute each, in the router's own state, not a static); a 503
gets a gated line with the error; a success gets one ungated info line with the agent ID and the
number of rows revoked. The code, the display name and the owner are never logged.

## Consequences

- A registration in one tenant can revoke a key's row in another; this is accepted because the
  key holder alone causes it.
- A database upgraded with duplicate rows loses all but the newest of each key; the warning in the
  migration's output is the only record, and the revoked rows remain.
- A server that is migrating makes the issuance command fail for that moment; there is no waiting.
- The route, the limiter and the response signature are separate work and follow this record's
  rules; their implementation is the check of items 5 and 6.

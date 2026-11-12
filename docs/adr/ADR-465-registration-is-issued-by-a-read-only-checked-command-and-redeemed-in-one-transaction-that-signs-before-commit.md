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

## Consequences

- A registration in one tenant can revoke a key's row in another; this is accepted because the
  key holder alone causes it.
- A database upgraded with duplicate rows loses all but the newest of each key; the warning in the
  migration's output is the only record, and the revoked rows remain.
- A server that is migrating makes the issuance command fail for that moment; there is no waiting.
- The route, the limiter and the response signature are separate work and follow this record's
  rules; their implementation is the check of items 5 and 6.

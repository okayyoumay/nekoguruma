# ADR-458: The Server Applies Embedded Migrations Before It Binds, and Its Tests Start a Throwaway PostgreSQL

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** `crates/server` (`db.rs`, `config.rs`, `health.rs`, `build.rs`, `tests/postgres/`), `db/migrations`, `db/README.md`, `api/openapi.yaml` (health), design 12 and 13.3, `.gitattributes`, `.github/workflows/ci.yml`, CLAUDE.md "Building and testing"; extends ADR-456 items 7 and 8

## Context

Design 12 names PostgreSQL through sqlx for the server, bundled for the local deployment too, and
design 14 asks for one binary, but nothing said how the schema reaches the database, what the
server does when the database is missing or at the wrong version, or what the health route
checks. The server of ADR-456 had no database. M3 adds it, with sqlx 0.9.0 (the maintainer's
choice; its minimum Rust version is ADR-457).

The server's tests now need a real PostgreSQL: `SKIP LOCKED`, partitioned tables and `jsonb` are
the reason design 12 picks it, so a fake would test nothing that matters. `core-windows` runs the
server's tests, and service containers exist on Linux runners only. `cargo test --workspace` is
the documented local command, a test that cannot run must fail rather than be skipped, and agent
sessions run as root with PostgreSQL installed but not started.

## Decision

1. sqlx is used with default features off and `runtime-tokio`, `postgres`, `migrate` and
   `macros`. There is no TLS feature (the local profile's database is on loopback; the standard
   profile adds it) and no `query!` family, which needs `DATABASE_URL` or a committed cache in
   every job that compiles the server. Queries are `sqlx::query` with static SQL, checked by
   the database tests.
2. The configuration has a required `[database]` table: `url` and `connect_timeout_secs`
   (default 10, 1 to 300). The URL holds the password, so the file must be readable by the
   service account only, the URL's `Debug` output is empty, an invalid URL is reported with
   fixed text, and no log line or error repeats it. The URL is checked when the file is loaded:
   it must be `postgres://user:password@host[:port]/database` and nothing else. A query string
   is refused before sqlx sees it, because sqlx logs the value of a parameter it does not know,
   so a mistyped `?pasword=...` would reach the log. TLS is off. The parts that are required are
   the ones sqlx would otherwise fill from `PGHOST`, `PGUSER`, `PGPASSWORD`, `PGDATABASE` or
   `~/.pgpass`; `sslmode` and `application_name` are set explicitly. One limit remains: sqlx
   reads `PGOPTIONS` and has no way to unset it, so that variable still reaches the connection.
3. The migrations of `db/migrations` are embedded with `sqlx::migrate!`; `build.rs` makes cargo
   watch the directory. The start order is: connect (within the connect timeout, as the pool's
   acquire timeout), migrate (within 60 s), bind. Nothing listens before the schema is current.
4. Any failure of the connection or the migrations ends the process with exit 1 and no retry
   inside the process. sqlx's advisory lock is held per session and counts how often it was
   taken, so a retry on a session that still holds it takes it again, and sqlx's single unlock
   then leaves it held; the pool is closed and the process exits, and the service manager's
   restart policy covers a database that starts later. The run uses one connection of its own,
   with `lock_timeout` and `statement_timeout` set to the 60 s limit on it, so that PostgreSQL
   itself ends a wait for the lock or a statement that runs too long (dropping the client's
   future would leave the backend running and holding the lock); the client waits 5 s longer as
   a backstop, and on any failure closes that connection, which ends the session and releases
   the lock. The two settings are reset before the connection returns to the pool. A database that holds a migration the binary does not carry (a newer binary
   wrote it), one whose recorded checksum differs from the embedded file, or one left half
   applied refuses the start.
5. Two servers starting together are safe: sqlx's advisory lock makes one apply the migrations
   and the other find nothing to do.
6. A merged migration file is immutable byte for byte (a fix is a new file), its name is
   `NNNN_snake_case.sql` (sqlx reads the digits before the first underscore as the version), and
   its line endings are LF on every checkout: the checksum covers the bytes, and a Windows
   checkout that converts line endings would make every applied migration look changed.
   `.gitattributes` pins `db/migrations/*.sql` to LF and a unit test checks that no embedded
   migration contains a carriage return.
7. `GET /api/v1/health` runs `SELECT 1` on the pool and answers 200 when it succeeds within 2
   s, otherwise 503 `application/problem+json` with no detail; the cause goes to the log in a
   field named `external` (design 12.2). It stays unauthenticated.
8. The server's tests that need no database stay unit tests or use a pool that points nowhere.
   The others start a throwaway PostgreSQL through a helper in the server's test tree
   (`tests/postgres/support.rs`), from the machine's own `initdb` and `pg_ctl`. The binaries are
   looked up in this order: the directory in `NGR_TEST_PG_BIN`, `initdb` on `PATH`, the highest
   `/usr/lib/postgresql/<version>/bin`, the directory in `PGBIN`. A missing PostgreSQL fails
   the test, naming what was searched; it never skips it.
9. One cluster per test, in a temporary directory, with password authentication on 127.0.0.1 (not
   `trust`, which would let any local user act as the cluster's superuser for the test's
   duration), no Unix socket (`unix_socket_directories` empty, since the default directory is not
   writable by an ordinary user), a free port that is tried again when another process takes it,
   and `pg_ctl stop -m immediate` when the value is dropped. The helper always goes through
   `pg_ctl` and `initdb`, which drop the elevated token that `postgres` itself refuses to run
   with on Windows. When the tests run as root, which PostgreSQL refuses, the helper runs both as
   the `postgres` operating-system account, with the temporary directory handed to it (the ids
   come from the `id` command, so there is no `unsafe` and no libc binding).
10. The tests are one test binary, `postgres`, so the helper links once and nextest has one name
    for the group. Tests that start `ngr-server` stay in the package, so the binary under test
    is always current. CI runs them on `core-linux` and on `core-windows`, on pull requests
    too, with `NGR_TEST_PG_BIN` set from the runner image's PostgreSQL (16 on Ubuntu, 17 on
    Windows at the time of writing).

## Consequences

- PostgreSQL's server binaries become a prerequisite of `cargo test --workspace`. CLAUDE.md says
  so, and the helper's message names the lookup.
- The suite runs on two PostgreSQL major versions in CI and depends on the runner images
  shipping PostgreSQL; a step that prints the version turns a dropped image into one clear
  failure.
- A database test costs about a second on Linux for `initdb` and the start; Windows is slower.
  A cluster shared by all tests is the upgrade path, since a test only asks for a fresh
  database. A test killed by nextest's timeout leaves its postmaster and directory behind;
  CI discards the runner.
- A running server refuses a database that a newer binary has migrated, so a downgrade needs the
  database restored. An older migration file cannot be edited; the check catches an edit at the
  next start.
- A root run with `TMPDIR` inside a directory that the `postgres` account cannot enter fails at
  `initdb`; the failure message says so.
- The server holds a pool of at most 10 connections; the limit is revisited with the first
  queries that need more.

# `ngr-server`

The server (design 3.2, 12, 13). `crates/server` is a library plus the binary `ngr-server`. This
build serves the local deployment profile (design 13) only: it listens on loopback, uses
PostgreSQL through sqlx and has two routes, the health probe and the redemption of registration
codes. The rules of the listener and the
request guard are in ADR-456; the database, the migrations and the test database are in ADR-458;
the job instruction key is in ADR-462; registration codes and their command are in ADR-463 and
ADR-465.

## Running

```sh
ngr-server --config <file>
ngr-server registration-code --config <file> --owner <subject> --name <display name> \
    --agent-key <64 hex digits> [--ttl-minutes <1-60>]
```

The first form serves; `--config` is required there. There is no default path, no search and no
environment variable. The second issues a registration code ("Issuing a registration code"
below); `registration-code` is a subcommand only as the first argument, so
`ngr-server --config registration-code` still serves, with a configuration file of that name.
Exit codes: 0 after a graceful stop or an issued code, 1 when the configuration is refused or the
server cannot start or run (including a database that cannot be reached or migrated and an
instruction key that is refused), or when a code cannot be issued, 2 for a usage error.

## Configuration

One TOML file; every table refuses unknown keys, so a misspelt key is an error and not a silent
default.

```toml
profile = "local"            # the only value this build accepts; the default

[http]
port = 8080                  # 1-65535, default 8080
shutdown_grace_secs = 30     # default 30, at most 300

[database]                   # required
url = "postgres://ngr:<password>@127.0.0.1:5432/nekoguruma"
connect_timeout_secs = 10    # default 10, 1-300

[instruction_key]            # required
path = "/etc/nekoguruma/instruction.key"   # absolute; Windows: use a TOML literal string
```

- The `[database]` table is required, and the file holds the database password: make it readable
  by the service account only. The URL must be `postgres://user:password@host[:port]/database`
  (`postgresql://` also works) with a password and no query string or fragment; it is checked
  when the file is loaded. A parameter is refused because sqlx logs the value of one it does not
  know, and the required parts keep sqlx from filling them from `PGHOST`, `PGUSER`,
  `PGPASSWORD`, `PGDATABASE`, `PGPORT` or `~/.pgpass`. One exception: sqlx reads `PGOPTIONS` and
  cannot be told not to, so that variable still applies if it is set for the service. It is never logged, and its `Debug` output is empty. An invalid
  one is reported with fixed text. The server has no TLS to the database yet (ADR-458).
- The `[instruction_key]` table is required: `path` is an absolute path to the PKCS#8 file of the
  key that signs job instructions (design 11.1, ADR-462). There is no default path, no search and
  no environment variable. The table holds a path, never key material. The directory must exist,
  belong to the server's account (or, on Unix, to root) and, on Unix, not be writable by group or
  others; the server
  creates only the file. Put it beside the configuration, not in the database's data directory,
  and not in a directory that a backup of the database covers.
- There is no bind-address key. The listener is `127.0.0.1:<port>` in code, so no configuration
  can make the local profile listen elsewhere, and `[::1]` is not bound (design 13.3).
- There is no pinned-host key. The pinned origin is `http://127.0.0.1:<port>`. Changing the port
  changes the origin, and the browser keeps its per-origin data (design 3.1: edits are saved to
  IndexedDB first) under the old one, so choose the port once.
- A configuration error is logged with the parser's message and the line and column, never the
  source line, and a quoted value in the message is replaced by `...`, because a later key
  may hold a password.
- Port 0 is refused when the file is loaded. A `Config` built in code (a test) may carry it, and
  `Server::start` then derives the pinned origin from the port the system picked.

## Start and stop

1. Arguments, then the configuration file.
2. Logging: `tracing` text on stderr, at `info` unless `RUST_LOG` says otherwise. The operational
   log of design 12.2 replaces it later. A rejected Host or Origin value is not logged.
3. Connect to the database, waiting at most `connect_timeout_secs` (sqlx retries a refused
   connection within it). Then apply the migrations embedded from `db/migrations`, at most 60
   seconds. Any failure ends the process with exit 1, with no retry: the advisory lock sqlx
   takes is per session and counts how often it was taken, so a retry on a session that still
   holds it could leave it held. The run has a connection of its own with `lock_timeout` and
   `statement_timeout` set to that limit, so PostgreSQL itself stops a wait or a statement that
   takes too long (the client waits 5 seconds longer as a backstop), and a failed run closes
   its connection, which releases the lock. A database with a migration the binary does not carry, a changed one or
   one left half applied refuses the start. Two servers starting together are safe: one
   applies the migrations, the other finds nothing to do. Restarting after the database is
   back is the service manager's job.
4. Make the instruction key ready (below). A failure ends the process with exit 1 and nothing
   is retried.
5. Bind `127.0.0.1:<port>`, install the signal handlers, then log the pinned origin: a stop
   signal sent once the log line appears is never lost. A listening port means a migrated
   schema and a usable instruction key.
6. Serve until SIGTERM or SIGINT (Unix) or Ctrl-C or Ctrl-Break (Windows). Then stop accepting,
   let requests in flight finish for at most `shutdown_grace_secs`, and exit 0. A second signal
   during the grace period ends the wait at once (still exit 0). A connection that is still
   open then is dropped. A connection that has not delivered a complete request head is held
   for no longer than the header-read timeout below. A Windows service stop control arrives
   with packaging. A long-lived connection such as a WebSocket never finishes by itself, so the
   routes that hold one must close it when the shutdown begins. The database pool is closed
   last.

## The instruction key

`instruction_key.rs`; the decision and its alternatives are in ADR-462. The key is one ECDSA P-256
key per deployment. Its private half is the PKCS#8 file at `instruction_key.path`; its public half
is the one active row of `instruction_keys` (`db/README.md`), which is what keeps two servers of a
deployment, or a restart with another file, from signing under two identities.

| File | Active row | Start |
|---|---|---|
| absent | none | the file is created (mode 0600), the key is recorded, "instruction key file created" is logged |
| valid | same key | starts |
| valid | none | the key is recorded, starts |
| absent | present | exit 1 |
| unreadable, not a P-256 PKCS#8 key, or too open | any | exit 1; the file is never overwritten or regenerated |
| valid | another key | exit 1 |

- Every start logs `instruction key ready` with the key ID (64 hex digits, the SHA-256 of the SPKI
  document, the value of `jobs.signing_key_id`). There is no endpoint and no command for the key
  yet; registration returns the public key as SPKI PEM.
- On Unix the server opens the file with `O_NOFOLLOW`, `O_NONBLOCK` and `O_NOCTTY` (as the agent
  opens its lock files, ADR-262 item 6), so a symbolic link, dangling or not, is refused and a
  FIFO never blocks the start. On the open handle it then refuses what is not a regular file,
  what is not owned by the server's effective user, and any group or other permission bit. By
  path it refuses a directory that is writable by group or others or owned by neither the
  server's user nor root. On Windows the file is opened as a reparse point and refused if it is
  one. Errors are fixed text plus the OS error kind, never
  the path's contents or key bytes. Only the server's account should read the file; a user-mode
  agent that runs under the same account can read it too, which design 6.6 accepts for a personal
  installation.
- On Windows the file is a plain file protected by the ACL of its directory, which the
  installation sets; the code checks no ACL. DPAPI wrapping is not done.
- Creation writes the bytes to a temporary file of a unique name in the same directory, links it
  to the final name (which fails when the name exists, so a second creator loads the first one's
  key), removes the temporary file and syncs the directory. The file system must support hard
  links. A file left from a crash between the temporary file and its removal is named
  `.instruction-key-<hex>.tmp` and may be deleted.
- The file is in neither a database dump nor the export of design 13.3. Losing it costs the
  agents' re-registration, not data.
- `server::InstructionKey` is `AppState.instruction_key`: `public_key()`, `key_id()` and the
  asynchronous `seal(&JobInstruction)`, which encodes the instruction and joins the envelope of
  `shared_crypto::instruction`. It holds a `Signer`, so a signing service or an HSM can replace
  the file; such a signer must cache its public key, because `seal` asks for it on every call.

**Resetting the key** (a new key; every agent must then register again, and jobs signed under the
old key are not delivered): stop the server; in the database, set `retired_at = now()` on the row
of `instruction_keys` where it is null; remove the key file; start the server, which creates a new
key and records it. Also remove any `.instruction-key-*.tmp` beside the key file: a crash between
the link and the removal of the temporary file leaves a second link to the key. A file restored over a retired row is refused ("only a retired record"). A
command that does this is not written.

## Issuing a registration code

`registration.rs` and `main.rs`; the decisions are in ADR-463 and ADR-465. A code lets one agent
register (design 6.4, 6.7): it is bound to an owner (an operator, by subject) and to the agent
key's fingerprint, which `ngr-agent identity` prints. Whoever can read the server's configuration
is the Administrator (design 6.8).

```sh
ngr-server registration-code --config <file> --owner alice --name "Bench PC" \
    --agent-key <64 hex digits> [--ttl-minutes 10]
```

- The flags come in any order, each at most once, as `--flag value`. The lifetime is 10 minutes
  unless `--ttl-minutes` gives a whole number from 1 to 60. The name is 1 to 128 characters
  without control characters; the owner is not empty; the fingerprint is accepted in either case
  and stored in lower case. Anything else is exit 2 with the usage text, before the database is
  touched.
- The flow: the configuration, the database connection (within `connect_timeout_secs`), a
  read-only check that the applied migrations equal the embedded ones, then one transaction that
  reads the active `instruction_keys` row, finds exactly one enabled operator with the subject and
  inserts the row (`agent_registration_codes`). The expiry is the database's `now()` plus the
  lifetime. The command never applies a migration and never reads or creates the instruction key
  file: a database that was never migrated (the migrations table is not created), one behind or
  ahead of the binary, or one with a changed or half-applied migration is exit 1, and so is a
  server that is applying migrations at that moment (rerun it). No active key row, an owner that
  is unknown, disabled or present in more than one tenant (the subject is unique per tenant only)
  are exit 1 with a message each.
- Standard output is one JSON line after the commit, for example
  `{"code":"ngr1-...","expires_at":"2026-11-12T10:20:30Z","server_key_id":"<64 hex digits>"}`.
  The operator gives the agent the code (standard input or a file, never an argument) and the
  server key ID (`--server-key`). If writing the line fails, the row stays and expires unused.
  Only the code's SHA-256 is stored; the code is never logged and appears nowhere else. Errors
  are `ngr-server: <fixed text>` on standard error, without the database URL or the code. Two
  commands at once make independent rows, and an earlier unused code stays valid.

## Registration

`register.rs` is the route that redeems a code issued by the command above (ADR-463, ADR-465
"Redemption design", which tables every case). The steps, in order, and the first that applies
answers:

1. The request guard (above).
2. A method other than POST: 405 with `Allow: POST`.
3. The body, read within 5 seconds (408), at most 4096 bytes (413; exactly 4096 passes).
4. `Content-Type` and shape, with no database lookup: 400 unless there is one `Content-Type` of
   `application/json` (parameters allowed), a JSON object with `code`, `publicKey`, `mode` and
   `proof` exactly once each as strings and no other field, a code of at most 64 characters that
   `RegistrationCode::parse` accepts (case, `I`, `L`, `O` and outer whitespace are normalized
   before hashing), a `publicKey` of at most 512 characters that is an SPKI PEM, a `mode` that is
   exactly `user` or `device`, and a `proof` of at most 128 characters that is strict standard
   padded base64 of 64 bytes.
5. The proof, with no database access: the signature must verify over the code's hash, the key's
   fingerprint, the mode and this server's in-memory key ID. A 64-byte value that is no signature,
   or one made for another mode, code or server key, is a refusal.
6. One read committed transaction: `lock_timeout` and `statement_timeout` of 5 seconds; one
   conditional `UPDATE` consumes the code (unused, not revoked, not expired by the database's
   clock, bound to this agent key and to the in-memory key ID, that key still active, the owner in
   the code's tenant and enabled); on zero rows one `SELECT` classifies why, for the log only, and
   the transaction rolls back. Then a transaction-scoped `pg_advisory_xact_lock` (two-integer
   form, a fixed class and the first four bytes of the key's fingerprint) serialises registrations
   of one key; the key's unrevoked `agents` rows in every tenant are revoked; the agent row is
   inserted (preset `personal` and remote use off are the values that are signed); the owner's
   `agent_allowed_operators` row is inserted; the agent ID is recorded on the code; the response is
   signed with the instruction key; then the commit. Any failure before the commit rolls back and
   leaves the code redeemable. No `audit_log` row is written.

Answers: 201 with `agentId`, `ownerId`, `preset`, `allowRemote`, `instructionKey` (SPKI PEM),
`instructionKeyId` and `signature` (base64); 403 for every refusal, the same bytes whatever the
reason; 429 with `Retry-After` instead of a 403 once ten refusals were given within the last
minute; 503 when the pool, the connection or a statement fails or times out (an unknown commit
outcome too, after which the code is used and the agent needs a new one); 500 when the signer
fails. Only a 403 counts toward the limit: a redemption never consults it, and 400, 405, 408, 413,
429 and the 5xx are never counted. The limiter and the log gates belong to one `router()`, not to
the process.

Each refusal is one warning line with a fixed reason word (`proof`, `absent`, `revoked`, `used`,
`expired`, `other_key`, `instruction_key`, `owner`, `state_changed`) and the code's redacted
`Debug`, at most once a minute per reason; a 503 and a 500 have a gated line of their own; a
success is one info line with the agent ID and the number of revoked rows. The code, the display
name and the owner are never logged, and a database error is logged as its SQLSTATE only.

## The request guard

`local_guard.rs` is one middleware that wraps the whole route table as a service (`router()` in
`lib.rs`), so that it runs before routing for every request, unknown paths included. With P the listener's
port, the first row that matches decides:

| # | Request | Answer |
|---|---|---|
| 1 | Request target not in origin form | 400 |
| 2 | Host missing, repeated, empty, with userinfo or malformed | 400 |
| 3 | Host is `localhost:P` | 307 to `http://127.0.0.1:P` plus the path and query |
| 4 | Host is anything but `127.0.0.1:P` | 421 |
| 5 | More than one Origin header, or one that is not the pinned origin | 403 |
| 6 | Origin absent or the pinned origin | the route |

A port may be left out of Host or Origin only when P is 80. Names compare ignoring ASCII case;
nothing else is normalized. Bodies are `application/problem+json` and never repeat a rejected
value. The 307 carries `Cache-Control: no-store` and an absolute `Location`.

- A missing Origin passes: the agent, curl and same-origin browser GETs send none. Origin never
  authorizes (design 5.7.1); the login and the agent's key will.
- A browser always sends Origin on a WebSocket handshake, so row 5 refuses a cross-site
  handshake before any upgrade. The agent's control channel sends none and passes row 6.
- A browser sends no Origin on a cross-site navigation or a no-cors GET, so a GET route must
  never change state, and what a GET returns must be protected by the login, not by this guard.
- The UI addresses the server by relative URLs or the pinned origin: a script request from a
  pinned-origin page to `localhost` is redirected with an Origin that row 5 refuses.
- No CORS header is sent and `X-Forwarded-Host` is not read.
- Only HTTP/1 is served, since with HTTP/2 the authority would arrive without a Host header.
  `axum::serve` is not used: another crate in the workspace build (tonic) turns hyper's HTTP/2
  on, and `axum::serve` would then accept cleartext HTTP/2. `Server::run` drives hyper's HTTP/1
  connection builder itself, so the protocol does not depend on which features the build unifies.
- A connection must deliver a complete request head within 5 seconds (`HEADER_READ_TIMEOUT`);
  a client that sends nothing, or one byte at a time, is dropped.
- The guard wraps the whole route table, so a request it rejects never gets a method router's
  405 with `Allow` or the fallback's 404.
- Loopback has no server authentication: another OS user that binds the port first answers as
  the server. The guard does not address this.

## Routes

- `GET /api/v1/health` (`api/openapi.yaml`): runs `SELECT 1` on the pool. 200
  `{"status":"ok"}` when it succeeds within 2 seconds, otherwise 503 `application/problem+json`
  with no detail (the cause goes to the log, in a field named `external`). Both carry
  `Cache-Control: no-store`; no login.
- `POST /api/v1/agents/register` (`api/openapi.yaml`; `register.rs`; ADR-463, ADR-465): redeems a
  registration code. No login: the code and the agent key's proof authorize it. See
  "Registration" below.

Any other path answers 404 with a problem body, after the guard.

## Database access

sqlx 0.9 with default features off and `runtime-tokio`, `postgres`, `migrate` and `macros`. There
is no `query!` macro (it needs `DATABASE_URL` or a committed cache in every job that compiles the
server): queries are `sqlx::query` with static SQL, and the database tests are their check. There
is no TLS feature; the standard profile adds it. `build.rs` makes cargo watch `db/migrations`, so
a new migration file rebuilds the crate. How migrations are written and applied is in
`db/README.md`.

## Tests

- `cargo test -p server` needs the machine's PostgreSQL server binaries (`initdb`, `pg_ctl`):
  the database tests start a throwaway cluster per test through the helper in
  `tests/postgres/support.rs`, with password authentication on 127.0.0.1 and no Unix socket,
  stopped when the test ends. The binaries are looked up in the directory `NGR_TEST_PG_BIN`,
  then on `PATH`, then in the highest `/usr/lib/postgresql/<version>/bin`, then in `PGBIN`; a
  missing PostgreSQL fails the test and never skips it. When the tests run as root, the helper
  runs PostgreSQL as the `postgres` account, which then needs access to the temporary directory
  (`TMPDIR` and its parents). A test killed by a timeout can leave its postmaster behind.
- The tests are one test binary, `postgres` (`tests/postgres/`). `migrations.rs` covers the
  migrations: applying them, a second start as a no-op, a changed, unknown or half-applied
  migration refusing the start, two starts at once, and the connect and migration timeouts.
  `serving.rs` runs the server in the test process on an ephemeral port and drives it over TCP
  with hand-written HTTP/1.1 requests, which gives the test every header; most of its tests
  serve over a pool that points at no database, the ones that need health 200 or 503 with the
  database up or down start a cluster. It also covers the stop: a request in flight finishes
  during the grace period, a stalled one is dropped at its end, a second signal ends the wait.
  `registration.rs` covers issuance: a stored row (hash only, the database clock's expiry, the
  exact 1 and 60 minute lifetimes), a missing or retired key, an unknown, disabled or ambiguous
  owner, two issuances at once, and the command as a process (one JSON line that matches the row,
  usage errors with no connection, a database never migrated and left untouched, behind, ahead,
  changed and half-applied migrations, a database that is down, no key file read or created).
  `redeem.rs` covers the route over TCP. Without a database (a pool that points at nothing): a 400
  for each shape rule, 405, 4096 and 4097 bytes, the 408 after 5 seconds, eleven bad proofs giving
  ten identical 403s and then a 429 with `Retry-After` while 400, 405 and 413 stay as they were,
  and valid proofs giving 503 that are never counted. With a database: the happy path's rows, the
  signature verifying and an `audit_log` that stays empty; typed variants of a code; every refusal
  (a bad proof, used, unknown, revoked, expired, another key, a disabled owner, an owner outside
  the tenant, a retired key) giving the same 403 bytes and its reason in the log, with the right
  key redeeming a code that another key was refused; a code issued under a replaced key; the same
  key registering again, in another tenant, and another key; one code twice at once and two codes
  for one key at once (20 rounds each on the multi-thread runtime); a statement failure from a test
  trigger and a failing signer, each rolling back and leaving the code redeemable; and an unknown
  code limited while a valid registration is not. No log line holds a code, a name or an owner.
  `migrations.rs` also upgrades a database that holds duplicate agent rows (newest kept, ties by
  id, revoked rows untouched, cross-tenant duplicates).
  `binary.rs` starts `ngr-server` as a process (on a port that was free a moment before, trying
  again when another process takes it): usage and start failures, a refused database and a
  changed migration (exit 1, nothing listens, no password in the log), health over TCP, and the
  SIGTERM stops on Unix.
  `instruction_key.rs` (database) covers the key's start outcomes: the first start creates the
  file and records the row, a restart keeps both, a valid file without a row is recorded, a
  removed file with a row, a file of another key and a file that is no key are refused and left as
  they were, a retired record, one active row only, two starts with one path (both succeed, one
  key) and two starts with different paths (exactly one succeeds). `binary.rs` also checks that a
  refused key gives exit 1 and nothing listens. Every server test config names a key path; the
  tests that start several servers on one cluster share it (`TestPostgres::key_path`).
- The unit tests need no database: the guard's decision table, the configuration (including that
  no error shows the URL and that the key path is required and absolute), the key file (create,
  reload, garbage, a directory, too-open modes and directories on Unix, sixteen threads creating at
  one path, no secret in `Debug`), a sealed instruction that splits, verifies and decodes with
  `shared_crypto::instruction` and `shared_proto::JobInstruction`, and that the embedded
  migrations are the files of `db/migrations` and contain no carriage return, that the migrations
  table the read-only check reads is sqlx's, the check's comparison, the command line's grammar
  and the request validation; and the redemption's limiter (ten counted, the eleventh limited with
  the exact seconds, one slot freeing at 60 seconds, 50 threads at one instant), its shape table,
  its proof checks and its log gates. The tests may run as
  root, so a refusal is provoked by mode bits or by a directory at the path, never by mode 000.
- The HTTP/1-only test must also pass in a workspace build, where hyper's HTTP/2 is enabled:
  build with `cargo test --workspace --no-run` and run the `postgres` test binary.

# `ngr-server`

The server (design 3.2, 12, 13). `crates/server` is a library plus the binary `ngr-server`. This
build serves the local deployment profile (design 13) only: it listens on loopback, uses
PostgreSQL through sqlx and serves the health probe, the redemption of registration codes, the
agent's connection proof and control channel, and the local users' login. The rules of the listener and the
request guard are in ADR-456; the database, the migrations and the test database are in ADR-458;
the job instruction key is in ADR-462; registration codes and their command are in ADR-463 and
ADR-465; local users, their command and the operator session are in ADR-469; the object store is in
ADR-474.

## Running

```sh
ngr-server --config <file>
ngr-server registration-code --config <file> --owner <subject> --name <display name> \
    --agent-key <64 hex digits> [--ttl-minutes <1-60>]
ngr-server user add --config <file> --username <name> --name <display name> [--role <role>]...
ngr-server user set-password --config <file> --username <name>
ngr-server user disable --config <file> --username <name>
```

The first form serves; `--config` is required there. There is no default path, no search and no
environment variable. The second issues a registration code ("Issuing a registration code"
below); `registration-code` is a subcommand only as the first argument, so
`ngr-server --config registration-code` still serves, with a configuration file of that name.
The `user` forms manage local users ("Local users" below), with the same rule for `user`.
Exit codes: 0 after a graceful stop, an issued code or a user change, 1 when the configuration is refused or the
server cannot start or run (including a database that cannot be reached or migrated and an
instruction key or an object store directory that is refused), or when a code cannot be issued or a user changed, 2 for a
usage error (for `user add` and `set-password`, also a terminal on standard input).

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

[storage]                    # required
dir = "/var/lib/nekoguruma/objects"        # absolute; created when absent
max_object_bytes = 4294967296              # default 4 GiB, at least 1
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
- The `[storage]` table is required: `dir` is the absolute path of the object store's directory
  ("The object store" below, ADR-474), and `max_object_bytes` (default 4 GiB, not 0) is the
  most one object may hold. There is no default path. The server creates the directory (mode
  0700 on Unix) when it is absent and its parent exists; it never creates the parent. The
  directory, and the `staging` and `objects` directories inside it, must belong to the server's
  account (or, on Unix, to root) and, on Unix, not be writable by group or others; a symbolic
  link to such a directory is accepted. Put it on one file system, apart from the database's
  data directory and from the instruction key.
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
5. Open the object store (below): create what is missing in `storage.dir`, check it and lock it
   for the life of the process. A failure closes the database pool and ends the process with
   exit 1; nothing is retried. The commands `registration-code` and `user` never open, create
   or lock the store.
6. Bind `127.0.0.1:<port>`, install the signal handlers, then log the pinned origin: a stop
   signal sent once the log line appears is never lost. A listening port means a migrated
   schema, a usable instruction key and a usable, locked object store.
7. Serve until SIGTERM or SIGINT (Unix) or Ctrl-C or Ctrl-Break (Windows). Then stop accepting,
   let requests in flight finish and close every control channel connection with 1001, for at
   most `shutdown_grace_secs`, and exit 0. A second signal during the grace period ends the wait
   at once (still exit 0). What is still open then is aborted: the HTTP connections first, then
   the control channel sessions, then the database pool is closed (see "The control channel"
   below). A connection that has not delivered a complete request head is held for no longer
   than the header-read timeout below. A Windows service stop control arrives with packaging.

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

## The object store

`storage.rs` (the `ObjectStore` trait and its types) and `storage/local.rs` (`LocalStore`); the
decision is in ADR-474. The store holds the large bodies that the database refers to by key (the
`storage_key` columns, `db/README.md`): the local profile keeps them as files; `LocalStore` is the only
implementation, and the standard profile's S3-compatible store is not written (design 13). The
trait's futures are boxed by hand, so `Arc<dyn ObjectStore>` works, and `Server::object_store()`
returns the store `Server::start` opened. No route or task of the server uses the store.

A write is a resumable upload by offset:

- `begin()` issues the key, a version 4 UUID in lowercase text, and an empty unfinished object.
  The caller records the key in its `storage_key` column. A key of any other form is
  `InvalidKey`, before any file is touched: no path is built from caller text but a validated
  key.
- `append(key, offset, bytes)` writes only when `offset` is the stored length, syncs the file and
  returns the new length. Any other offset is `OffsetMismatch` carrying the stored length and
  writes nothing, so a writer cut off mid-chunk, or one whose answer was lost, resumes from the
  length in the refusal or from `status`. An append that would pass `max_object_bytes` is
  `TooLarge` and writes nothing (exactly the limit is accepted); appending nothing at the stored
  length returns it. An append never creates an object (`NotFound`), and a finished one is
  `AlreadyFinished`.
- `finish(key, sha256, length)` checks the length against the stored length, then the SHA-256 of
  the whole content, and moves the file from the staging area to the finished area (rename, then
  both directories synced). A mismatch (`LengthMismatch`, `HashMismatch`) leaves the object
  unfinished and writable. A finished object is immutable: no append, no second finish. A
  zero-length object can be finished. The size limit is not applied at finish.
- `get(key)` opens a finished object and returns its length and an `AsyncRead`; an unfinished
  object is `NotFinished`. A read that began before a `delete` stays complete on Unix.
- `delete(key)` removes an object, finished or not (`NotFound` when there is none).
- `list_unfinished()` returns each unfinished object's key, stored length and last write time,
  for a caller that cleans up abandoned uploads. Names in the staging area that are not keys are
  ignored; an entry that is not a regular file is skipped with a warning. An object that is
  also finished (a hand-made copy) is not listed.

The directory holds `staging/<key>`, `objects/<key>` and `.lock`, flat, on one file system. The
rules the implementation keeps:

- The stored length is the length of the staging file after one `sync_all`, read under the key's
  lock by the one function every operation uses. It is not recorded anywhere, so after a crash
  it is what reached the disk, and a chunk cut part way leaves a prefix that the writer resumes
  after. Nothing truncates, preallocates or repairs a file.
- A key whose `sync_all` failed answers an I/O error to everything but `delete` until the process
  restarts, because a failed sync can drop data the operating system had accepted.
- A key is finished if `objects/<key>` exists (it wins when the key is in both areas, and
  `delete` removes both), else unfinished if `staging/<key>` exists, else not found. If the
  directory or an area is gone, the answer is `Unavailable`, never not found, and nothing is
  recreated until the server restarts.
- Calls on one key are serialised by a per-key mutex whose guard moves into the one blocking task
  that does the work, so a caller that drops its future does not free the key while its write
  runs. Different keys run concurrently. File I/O runs on the blocking pool.
- Every call is durable before it answers: `begin` syncs the file and the staging directory,
  `append` the file, `finish` the directories of the rename, `delete` the directory it removed
  from, and the start both areas. A rename across devices fails with an I/O error and leaves the
  object unfinished; the file is never copied and deleted. A failed directory sync after the
  rename is an I/O error, and the object is finished.
- One process per directory: the store holds an exclusive lock on `.lock` for its life; a second
  server on the directory (or a second `LocalStore` in the process) is refused with `InUse` and
  exits 1. The file is never deleted. The operating system drops the lock when the process dies.
- Files are created mode 0600 and opened with `O_NOFOLLOW` and `O_NONBLOCK` (a link or a FIFO in
  the directory is refused, as for the instruction key's file), and the open handle must be a
  regular file. Windows opens staging files for read and write, because a sync may be refused on
  a read-only handle; it locks only `.lock`, and checks no ACL and syncs no directory.
- The directory checks (ownership, group and other write bits, a path that is a directory) are
  the instruction key's, in `fs_guard.rs`, which both call.
- Errors are fixed text plus the `ErrorKind` of a failed file operation, never a path.

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

1. The request guard ("The request guard" below).
2. A method other than POST: 405 with `Allow: POST`.
3. The body, read within 5 seconds (408), at most 4096 bytes (413; exactly 4096 passes).
   A declared `Content-Length` over 4096 is answered 413 before any byte is read. A streamed
   (chunked) body has no declared length and is cut when it grows past 4096, so a client that
   keeps sending may see the connection reset instead of the 413.
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
6. One transaction, begun explicitly at read committed whatever the database default: `lock_timeout` and `statement_timeout` of 5 seconds; one
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

## Local users

`local_user.rs` and `main.rs`; the decisions are in ADR-469. On the local profile an operator logs
in with a username and a password instead of OIDC (design 6.4, 13.3). The username is the
operator's `subject` in the local tenant, whose ID is fixed in the server
(`4e475200-0000-4000-8000-000000000001`, named `local`); `user add` creates that tenant if it is
absent. The password is kept only as an Argon2id hash (19 MiB, two passes, one lane, a random
salt; the parameters are in the stored string) in `operator_credentials`. Like
`registration-code`, the command writes the database directly: whoever can read the server's
configuration is the Administrator (design 6.8).

```sh
printf '%s\n' "$PASSWORD" | ngr-server user add --config <file> --username alice \
    --name "Alice" --role worker --role admin
ngr-server user set-password --config <file> --username alice < password.txt
ngr-server user disable --config <file> --username alice
```

- The flags come in any order, each at most once except `--role`. The username is 1 to 64 of
  `a-z`, `0-9`, `.`, `_`, `@` and `-`; the display name is 1 to 128 characters without control
  characters; a role is one of `viewer`, `worker`, `reprogrammer`, `remote`, `approver` and
  `admin`, each at most once, and no role is given implicitly. Anything else is exit 2 with the
  usage text, before standard input or the database is touched. The configuration path is taken
  as given, as for `registration-code`: a path need not be UTF-8.
- `add` and `set-password` read the password from standard input, never from an argument. A
  terminal on standard input is exit 2 with a hint to pipe or redirect it, since the password
  would echo. One trailing line break (LF or CRLF) is removed and nothing else is trimmed. The
  password must be valid UTF-8 without a byte order mark, one line, without control characters,
  at least 12 characters and at most 1024 bytes; otherwise the command exits 1 and writes nothing.
- The configuration, the database connection and the check that the applied migrations equal the
  embedded ones are those of `registration-code`: the command never migrates. `add` refuses a
  username that exists in the local tenant (exit 1, also when two run at once); it then writes
  the tenant if absent, the operator, its roles and the credential in one transaction.
  `set-password` refuses an unknown or a disabled operator and otherwise replaces (or creates)
  the credential. `disable` refuses an unknown operator, sets `disabled_at` (an operator already
  disabled is exit 0 and unchanged) and keeps the credential. A disable or a new password ends
  the operator's open sessions at their next request.
- Standard output is one JSON line after the commit: `add` prints
  `{"operator_id":...,"username":...,"roles":[...]}`, `set-password` prints the ID and the
  username, `disable` adds `disabled_at` and `already_disabled`. If writing the line fails, the
  change stays and the exit is 1. The password and its hash are never printed or logged.

## Logging in

`operator_session.rs`; the decisions are in ADR-469.

- **`POST /api/v1/login`, in this order, the first step that applies answers.** The guard; the
  method (405 with `Allow: POST`); the Origin (403 "Origin required" unless it is the pinned
  origin); the body (4096 bytes within 5 seconds: 413, 408, 400); the shape (an `application/json`
  object with exactly a string `username` and a string `password`: 400); the lengths (a username
  of 1 to 256 bytes without control characters, a password of 1 to 1024 bytes: 400); the limiter
  (ten admitted attempts within a sliding minute; while ten are recorded every attempt is 429
  with `Retry-After`, a right password included, and only a 200 gives its place back); one read
  of the operator, its credential and its roles (503, 500); one Argon2 verification; then the
  session. An unknown username, an operator without a credential, a disabled operator and a
  wrong password get one 401 body with `WWW-Authenticate: Ngr-Session`, and each costs one
  verification (against a dummy hash made once from 32 random bytes when there is none to check). A stored hash that is
  not an Argon2id string is 500. Verifications run one at a time on the blocking pool.
- **The session.** A 200 answers `{"operatorId","username","displayName","roles"}` and sets
  `ngr_session` (43 base64url characters; `HttpOnly; SameSite=Strict; Path=/`, no `Max-Age`). The
  server keeps the token's SHA-256 with the operator, the tenant and the SHA-256 of the verified
  stored hash. Sessions a login request presents are ended, never adopted. A session ends 8 hours
  after login or 30 minutes after its last use, judged on both tokio's clock and the wall clock
  (exactly at the limit counts as ended); an operator keeps at most 16, and the next login drops
  the least recently used. A restart ends them all.
- **The session layer** sits on each operator route's method router (`MethodRouter::route_layer`),
  so 404 and 405 answer first. Then: a method other than GET or HEAD needs the pinned Origin
  (403); exactly one well-formed `ngr_session` among the request's cookies (other pairs are
  ignored; none is 401, a malformed or repeated one is 401 that clears it); the session, judged
  at the request's arrival (401, cleared and removed when it expired); one read of the operator's
  `disabled_at` and stored hash (503 keeps the session and does not refresh it); a disabled or
  deleted operator, or a stored hash whose SHA-256 differs from the session's, is 401 and ends it;
  a session removed meanwhile is 401. Only a request that passes refreshes the last use.
- **`POST /api/v1/logout`** needs the pinned Origin and an empty body, ends every well-formed
  token presented and clears the cookie: 204, with or without a session, and no database access.
  **`GET /api/v1/me`** answers the session's operator with its name and roles read at that
  request.
- **Logging.** At login the reasons `unknown_user`, `no_credential`, `disabled` and
  `wrong_password` (with the operator ID when there is one) and `login limited`; in the session
  layer `operator_gone`, `operator_disabled`, `no_credential` and `credential_changed`; `login`
  and `logout` at info. Every reason has its own gate, once a minute, so one never silences
  another. The password, the typed username of a failed login, the
  cookie, the token and the hashes are never logged.

## Routes

- `GET /api/v1/health` (`api/openapi.yaml`): runs `SELECT 1` on the pool. 200
  `{"status":"ok"}` when it succeeds within 2 seconds, otherwise 503 `application/problem+json`
  with no detail (the cause goes to the log, in a field named `external`). Both carry
  `Cache-Control: no-store`; no login.
- `POST /api/v1/agents/register` (`api/openapi.yaml`; `register.rs`; ADR-463, ADR-465): redeems a
  registration code. No login: the code and the agent key's proof authorize it. See
  "Registration" above.
- `POST /api/v1/agent/challenge` and `POST /api/v1/agent/session` (`api/openapi.yaml`;
  `session.rs`; ADR-466, ADR-467): the server's side of an agent's connection proof. See "The
  agent's connection proof" below.
- `GET /api/v1/agent/control` (`api/asyncapi.yaml`, `agentControl`; `control.rs`, `hub.rs`;
  ADR-466, ADR-468): the agent's WebSocket. See "The control channel" below.
- `POST /api/v1/login`, `POST /api/v1/logout` and `GET /api/v1/me` (`api/openapi.yaml`;
  `operator_session.rs`; ADR-469): the local users' login and the operator session. See "Logging
  in" above.

## The agent's connection proof

`session.rs` holds both routes, the challenge store, the token table and their tests (design 6.4,
14.4, 16.3; ADR-466 items 1 to 4, ADR-467). No login: the proof authorizes the session.

- **Challenge route.** The body must be empty (any byte is 400; `Content-Type` is not looked at),
  read within 5 seconds (408) and at most 4 KiB (413). 200 `{"challenge": <44 characters>}`, with
  `Cache-Control: no-store`. The store keeps 32 random bytes for 60 seconds, oldest first, with at
  most 1024: a new one first drops the expired ones from the front, then the oldest while 1024
  remain. Issuing is capped at **60 per sliding 60 seconds** per router (a `register::Limiter`
  with its own numbers); past it the answer is 429 with `Retry-After` (1 to 60), a gated warning,
  and nothing is recorded, drawn or stored. A request that passed the body rules counts even if
  the random source then fails (500). With the cap, at most 60 challenges are live, so the bound
  of 1024 is the store's own invariant, tested directly. A caller that uses the whole cap denies
  challenges to honest agents for up to a minute.
- **Session route, in this order, the first step that applies answers.** The guard; the method
  (405); the body (413, 408); the shape (400): one `application/json` `Content-Type`, a strict
  object of `agentId`, `challenge`, `nonce` and `proof` (each once, as a string), `agentId` in
  lower-case hyphenated UUID form, `challenge` and `nonce` strict standard padded base64 of 32
  bytes, `proof` of 64. A 400 consumes nothing. The challenge (401): taken from the store, which
  removes it whatever the answer, so a missing, already presented or 60-second-old one is the
  same 401 with `WWW-Authenticate: Bearer`, with no database access, not counted and never
  turned into a 429. The agent's row (503, 500): one read of `public_key` and the revoked flag
  in a transaction that is dropped, with a 5 second lock and statement timeout; nothing is
  written (`last_seen_at` belongs to the control channel). The key and the proof (403, or 429):
  an unknown agent, a revoked row (its signature is not checked) and a proof that does not verify
  over the version, this server's audience, `agentId`, challenge and nonce are one 403 document;
  the third includes a wrong key, audience, agent ID, challenge or nonce and a high-S signature.
  A stored key that does not decode is a 500, not counted. The ten-refusals-in-a-minute limiter
  is the route's own (`register::Limiter::default()`), counts only 403, and answers 429 with
  `Retry-After` for the eleventh; a valid proof is never limited. Last the token: 200 `{"token",
  "expiresInSecs": 300}`.
- **The audience** is `local_guard::pinned_origin(port)` of the bound port, parsed once as a
  canonical `shared_crypto::proof::Audience`; nothing from a request is used.
- **Tokens.** `Sessions` (`Server::sessions()`, `AppState.sessions`) maps the SHA-256 of a
  token's 32 bytes to its agent and an expiry. A token is a multi-use bearer for 300 seconds;
  an agent may hold several, and nothing replaces an earlier one. Every token costs one
  challenge, so at most 360 are live; expired ones are dropped at each issue.
  `Sessions::authenticate(token)` and `revoke_agent(agent)` are for the control channel route.
- **Clock.** The stores, limiters and log gates read tokio's clock and take the instant as a
  parameter.
- **Logging.** A refusal logs a warning with the word `unknown_agent`, `revoked` or `proof` and
  the agent ID, once per word per minute; a wrong audience cannot be told from a bad signature
  and is `proof`. A 503, a 500 and a limited challenge have a gate of their own; a 401 logs at
  debug; a session issued logs at info. No token, challenge, nonce, proof or body is logged.

## The control channel

`control.rs` holds the route and the connection's task, `hub.rs` the registry of connections and
the tasks that run them (design 5.2, 13.3, 14.1; ADR-466, ADR-468). The accept loop serves each
connection with hyper's `with_upgrades()`.

- **The route, in this order, the first step that applies answers.** The guard (before routing);
  the method (405 with `Allow: GET` for every other method, HEAD included: the route takes every
  method and the handler refuses them itself, so that this 405 is a problem document too); the
  token (401 with `WWW-Authenticate: Bearer`); the WebSocket request (400); the subprotocol
  (400); then the 101 with `Sec-WebSocket-Protocol: ngr-control.v1` and no extension. Every refusal is a problem
  document, and a caller with no token learns nothing else about the route: a request with
  neither a token nor an upgrade is 401, and a valid token on a plain GET is 400.
- **The token.** Exactly one `Authorization` header, visible ASCII, the scheme `Bearer` in any
  case, one or more spaces, a non-empty token text, which goes to `Sessions::authenticate`
  unchanged (so a comma, a second word, another alphabet or a wrong length is a 401). One body
  for every reason, no limiter, one gated debug line, never the token in the log. The token is
  judged at the upgrade only: it is not consumed, not bound to the connection and not checked
  again, so one that expires while the connection is open ends nothing, and a second upgrade
  with the same token is accepted and replaces the first connection. A 400 after a valid token
  leaves it usable.
- **The subprotocol.** Every `Sec-WebSocket-Protocol` header is read, each value split at
  commas and trimmed, then compared byte for byte with `ngr-control.v1`: a list, two headers or
  the name twice select it; another case, quotes, another version or no header is 400. A request
  that cannot be upgraded (HTTP/1.0, no key, a version other than 13) is 400, never 426.
- **One task per connection,** spawned by `Hub::adopt` into the hub's own `JoinSet` (axum's
  callback only hands the session over), with one writer, since the socket is not split. It
  stamps `agents.last_seen_at`, registers as the agent's current connection, writes `hello
  {heartbeatSecs: 20}` (there is no configuration key for the interval), and then selects on the
  hub's close instruction, the stop, the frames the hub queued, the next message and the idle
  deadline. Every write is bounded by `WRITE_TIMEOUT` (10 seconds); one that fails or times out
  ends the connection with no frame.
- **The stamp (ADR-468 item 11).** One statement, `UPDATE agents SET last_seen_at =
  GREATEST(last_seen_at, now()) ... RETURNING revoked_at IS NOT NULL`, in a transaction with
  `db::statement_timeouts`, inside a 5 second bound (`stamp` in `control.rs`, `db::stamp_agent`).
  At connect it runs after the 101 and before the registration and `hello`, so a client that has
  read `hello` knows it is done. A failure or timeout is one gated warning and the connection
  goes on. A row that is revoked or missing (the stamp is committed either way) drops the agent's
  tokens and closes with 4002 as the first frame, without registering: no earlier connection is
  displaced and no disconnect is written. At disconnect it runs for a connection that was the
  agent's current one (`Hub::leave` returned true), after the Close frame (after the reply to the
  agent's own Close) and before the drain, with its result ignored and a failure one gated
  warning. A replaced connection, one that is aborted, a heartbeat, a message, a send, the
  revocation pass and a refused upgrade write nothing.
- **What it reads.** Text only. `heartbeat` is echoed. Another valid message is read and
  given to the hub's inbox (`Hub::inbox`, below). A text that does not decode closes with 1002
  (1009 when `decode` reports the size), a binary message with 1003, and a Close from the agent
  is answered by the stack on one more read. Both transport limits are 131072 bytes: a frame
  of 131073 bytes is refused on its header with 1009, one of 131072 is read. A read error is
  classified by the stack's own error (`tungstenite::Error`, hence the direct dependency): a
  capacity error is 1009, invalid UTF-8 or a framing violation 1002, a reset or an I/O error
  gets no frame. The Close reason is a fixed word per code, never anything the peer sent.
- **Idle.** Any complete text message, ping or pong resets the clock, which starts when `hello`
  has been written; three intervals (60 seconds) without one close with 4003. A partial frame
  resets nothing, a command written to the agent resets nothing, and time spent waiting for a
  full inbox is not idle time.
- **Commands and the inbox (ADR-468 item 12).** `Hub::send_job_command(agent, signed_job)` is not
  async: it draws a `commandId` (`c-<n>`, hub-wide, from 1; a failed call uses its number),
  encodes the frame outside the lock and queues it on the agent's current connection, whose
  queue holds 16 frames and which the task drains after `hello`, each write within
  `WRITE_TIMEOUT`. `Delivery { connection, command_id }` means queued, not written;
  `SendError::NotConnected` (no current connection, or it is closing), `Busy` and `TooLarge`
  (the frame over 131072 bytes, judged before the connection is looked at). A frame queued on a
  connection that has been told to close, or at a stop, is not written. `Hub::inbox()` gives its
  receiver (256 messages) to the first caller: `commandAck`, `commandRejected`, `agentEvent`
  and `reconcile` arrive as `Inbound { agent, connection, message }` in read order, never a
  heartbeat. The task asks the hub per message: with no receiver (not taken yet, or dropped) the
  message is dropped with a gated debug line, never buffered. With the inbox full the task holds
  the message and waits, reading nothing (so the agent sees no echo), while it watches its close
  instruction and the stop. The hub keeps no delivery table. `Hub::close_agent(agent, code)`
  gives the current connection's close instruction once (a code that is not one of the server's
  own close codes is refused with `false`), and `Hub::connected()` lists the
  current connections.
- **The revocation pass (`revocation.rs`).** One task of the hub's set, spawned by
  `Server::run`: a tick every 30 seconds (the first one 30 seconds after the start, a missed
  tick delayed, so a long jump gives one pass), no query when nobody is connected, otherwise one
  read-only statement (`db::live_agents`) over the connected agent IDs. An agent that is
  connected and not returned (a missing row included) has its tokens dropped, then its
  connection told to close with 4002, with one warn line; a failed read is one gated warning
  and closes nobody. It ends when the stop begins, in its wait and in its read.
- **After a close.** The task leaves the registry first, then writes the Close frame, then the
  disconnect stamp (above), then drains (reads and discards until the peer's reply, at most `CLOSE_WAIT`, 5 seconds). After a
  read error the stack allows no read, so the task waits `CLOSE_WAIT` without reading (a stop
  ends the wait) before it drops the socket: dropping at once with input unread can reset the
  connection and lose the Close at the peer.
- **Replacement.** `Hub::register` makes the new connection current and tells the displaced one
  to close with 4001 under one lock; a replaced connection removes nothing when it ends, since
  the entry it finds is its successor's.
- **The stop.** After the accept loop ends: signal the HTTP connections (each task calls
  hyper's `graceful_shutdown`, because hyper-util's graceful watch does not cover a connection
  that allows upgrades), begin the hub's stop (every session closes with 1001, also one that registers
  after the stop began; a connection whose upgrade completes after it is dropped without a
  frame), wait for both sets
  within `shutdown_grace_secs` or until the second signal, abort the HTTP connections (a pending
  upgrade then fails), abort the sessions, close the pool.
- **Logging.** One info line at the connect and one at the end, with the agent ID, a connection
  number and a word for the end; no token and no message text.
- **Scope.** The hub holds no job state and writes no table of its own: the connection tasks
  write `agents.last_seen_at` through the stamp, the revocation pass reads `agents`, and
  everything else the agent says goes to the inbox for whoever takes it.

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
  `session.rs` (unit tests in `src/session.rs`) covers the stores at their boundaries (59.999 and
  60 seconds, 299.999 and 300, 1024 and 1025 challenges, fifty threads taking one challenge), the
  shape table, the judgement of a row and a proof with no database, the cap's numbers and the
  log gates; `tests/postgres/session.rs` runs the two routes over TCP. Without a database: the
  challenge route's body, method and size rules, both routes' 408, the cap (60 issued, the 61st
  429, a challenge issued before it still live), each shape rule giving 400 with the challenge
  still live afterwards, unknown challenges giving 401 and never 429 or 503, and the guard's
  answers on both routes. With a database: a valid exchange (a token that authenticates twice,
  `last_seen_at` untouched, which the control channel alone writes); a challenge used once whatever the outcome; every refusal (unknown
  agent, revoked by SQL, revoked by registering the key again, another key, another audience,
  another challenge, another agent ID, high-S) giving the same 403 bytes, one gated log line per
  reason and no secret in the log; the eleventh refusal 429 while a 401, a 400 and a valid
  exchange are answered as before; a 503 from a locked table that is not counted; a challenge
  and a token expiring under a moved tokio clock (`pause`, `advance` and `resume` back to back
  with no request in flight); and one valid request sent twice at once giving one 200 and one 401.
  `control.rs` (unit tests in `src/control.rs` and `src/hub.rs`) drives the control channel over
  TCP with `tokio-tungstenite` as the client. Refusals: every way of having no valid token is
  the same 401 (no header, another scheme, a bare scheme, two headers, 43 characters, an unknown
  token, a lower-case scheme, nothing else valid in the request), the guard and the method answer
  before the token (307, 403, a 405 problem document with `Allow: GET` for POST, PUT, DELETE and
  HEAD), an expired token (a jump of
  301 seconds) and a token sent twice are 401, and a valid token on a plain GET, with no
  subprotocol, another case, quotes, another version, version 12, no key or over HTTP/1.0 is 400
  and still usable. Connection: the subprotocol from a list and from two headers with `hello`
  first and no extension, the heartbeat echo and the idle close under moved clocks (jumps of 50,
  50, 50 and 61 seconds with a ping in between, so that the ping is shown to count), binary
  giving 1003, 131073 bytes giving 1009 and 131072 bytes read, each malformed text giving 1002,
  a text frame that is not UTF-8 giving 1002 (a read error),
  valid outcomes and events read and the connection kept, a Close from the agent answered, a connection outliving its token
  (seven heartbeats 50 seconds apart, after which the token itself is refused),
  replacement (the old connection reads 4001, the new one is echoed). Stop: 1001 within the grace
  period, a peer that never answers aborted at the end of the grace period, a stop ending the wait
  after a read error, and a second signal
  dropping the connection at once. Last-seen time: NULL before a connection, set when `hello`
  is read and unchanged by three heartbeats and an event, later once the agent has gone (a
  Close from the agent, and a dropped transport), a refused upgrade writing nothing, a revoked
  or missing row closing with 4002 as the first frame (the stamp committed, the tokens
  dropped, an earlier connection left alone), a failed stamp (a renamed column) giving one
  warning each way with the connection served, a locked table delaying `hello` by about five
  seconds with the row unchanged, a second connection stamping again and the replaced one
  writing nothing, a stamp that never moves back, a stop during the stamp closing with 1001
  first, the Close written while the disconnect stamp waits on a lock, a stop writing the
  disconnect before `run` returns, and an aborted connection writing nothing. The pass: one
  jump of 31 seconds closing a row revoked by SQL (or deleted) with 4002 and dropping its
  tokens while a second agent is still echoed, live agents left alone, and a failed read
  closing nobody before a normal next period. Commands and the inbox: commands read in call
  order with the exact 131072 byte frame accepted, `TooLarge` and `NotConnected` returned,
  the inbox giving `commandAck`, `jobState`, `commandRejected` and `reconcile` in order with the
  agent and connection and no heartbeat, 300 events dropped while nobody takes the inbox and
  an inbox taken later used, a full inbox holding the task (no echo, no idle close across a
  jump of 61 seconds, handed over in order when a slot frees), and a close instruction and a stop
  ending a task that waits; a command written to a silent agent not restarting the idle clock.
  The unit tests cover the `Authorization` table, the close for each decode error, the
  classes of read errors, the hub's registration, replacement, stop, queues (`Busy`, the size
  boundary, the numbering, the close instruction) and `leave`, and the pass (`revocation.rs`
  under a paused clock: cadence, no query for nobody, the agents that change after the
  snapshot, a failed read, the stop in the wait and in the read). The plain-HTTP stop tests in `serving.rs` are the check that the loop's own watch did
  not change that behaviour.
  `binary.rs` starts `ngr-server` as a process (on a port that was free a moment before, trying
  again when another process takes it): usage and start failures, a refused database and a
  changed migration (exit 1, nothing listens, no password in the log), health over TCP, and the
  SIGTERM stops on Unix.
  `instruction_key.rs` (database) covers the key's start outcomes: the first start creates the
  file and records the row, a restart keeps both, a valid file without a row is recorded, a
  removed file with a row, a file of another key and a file that is no key are refused and left as
  they were, a retired record, one active row only, two starts with one path (both succeed, one
  key) and two starts with different paths (exactly one succeeds). `binary.rs` also checks that a
  refused key gives exit 1 and nothing listens. Every server test config names a key path and a
  storage directory; the tests that start several servers on one cluster share the key path
  (`TestPostgres::key_path`) and each take a storage directory of their own
  (`TestPostgres::storage_path`), since a store's directory is locked.
- `storage.rs` (database) covers the store at start: the first start creates the directory (mode
  0700 on Unix), its areas and the lock file and binds, the store from `Server::object_store`
  writes and finishes an object that a restart still finds, a group-writable directory and a
  missing parent refuse the start and leave the directory as it was, and a second server on one
  directory is refused until the first has stopped. `binary.rs` also checks that a refused
  storage directory (a file; on Unix, a group-writable directory) and one in use give exit 1
  with nothing listening, and `registration.rs` and `local_users.rs` that their commands create
  no storage directory.
- The object store's unit tests (`storage/local/tests.rs`) need no database and run on every
  platform, the Unix-only ones gated by `cfg(unix)`. They cover every operation on a key nobody
  issued, an unfinished, a finished and a deleted object; the key syntax table; the size limit
  (exactly the limit, one past it, a limit lowered across a restart, finish unaffected); the
  resumed write (a cut after zero, one, some and all bytes of the second chunk, resent from the
  old offset and finished, read back byte for byte); a lost answer; an orphan from a `begin`
  that was cut; injected failures (a write failing part way, a failed sync poisoning the key
  until a restart with delete still working, a failed directory sync after the rename, a failed
  rename); a caller that drops its future mid-append (the key stays held and the second append is
  refused with the new length); directories built by hand (no areas, one area, both areas, a key
  in both); the open's refusals (relative, missing parent, a file); the directory checks by mode
  bits and by owner (Unix); a link as the lock file or a staging file (Unix); sixteen appends at
  one offset, two finishes, and append, finish and delete in pairs; a read that outlives a
  delete (Unix); two stores on one directory, also while a call is still running; a directory
  removed under a live store (Unix); and the listing with sizes and times.
- The unit tests need no database: the guard's decision table, the configuration (including that
  no error shows the URL, that the key path is required and absolute, and that the storage
  directory is required and absolute and its limit defaults to 4 GiB and is not 0), the key file (create,
  reload, garbage, a directory, too-open modes and directories on Unix, sixteen threads creating at
  one path, no secret in `Debug`), a sealed instruction that splits, verifies and decodes with
  `shared_crypto::instruction` and `shared_proto::JobInstruction`, and that the embedded
  migrations are the files of `db/migrations` and contain no carriage return, that the migrations
  table the read-only check reads is sqlx's, the check's comparison, the command line's grammar
  and the request validation; and the redemption's limiter (ten counted, the eleventh limited with
  the exact seconds, one slot freeing at 60 seconds, 50 threads at one instant), its shape table,
  its proof checks and its log gates. The tests may run as
  root, so a refusal is provoked by mode bits or by a directory at the path, never by mode 000.
- `local_users.rs` (database) covers the local users: the login's order (the guard, 405, the
  Origin, the body limits, the shape and the lengths before any database access), one
  byte-identical 401 for an unknown user, no credential, a disabled operator and a wrong password,
  each waiting for a hash; a hash that is not
  Argon2id (500); the limiter (eleven concurrent wrong logins give ten 401s and one 429, a right
  password refused while it is full, a failed database read keeping its place, a client that
  disconnects during the hash keeping its place); the whoami body and cookie; a login replacing
  presented sessions; sixteen sessions per operator; two logins at once; a disable or a new
  password committed during a login; a disable, a deleted operator or credential and a new
  password (also the same text) ending a session, and a changed `updated_at` alone not ending it;
  a logout during the per-request read; logout with a live session and the database stopped,
  and its Origin, body and method rules; a disabled operator without a credential logged as
  disabled; the idle and the absolute limit across clock jumps; and that the role list in the
  code is the migration's CHECK. Its `command` tests run `ngr-server user` as a process: 24 usage
  errors (exit 2), a terminal on standard input (exit 2; Linux only, through util-linux
  `script`), each refused password (exit 1, nothing written), the configuration, database and
  migration checks, two `add`s of one username at once, the JSON lines, and a failed write to
  standard output after the commit. The unit tests in `operator_session.rs` cover the cookie
  parsing, the session table's limits at each boundary and one step before it (a nanosecond
  on the monotonic clock, a microsecond on the wall clock, which Windows keeps in 100 ns units), the clocks,
  eviction, and the layer's order with a dead pool; those in `local_user.rs` the password rules
  the dummy hash's parameters and that each hasher makes its own once. The tests generate every
  password at run time, so no literal reaches a hash.
- The HTTP/1-only test must also pass in a workspace build, where hyper's HTTP/2 is enabled:
  build with `cargo test --workspace --no-run` and run the `postgres` test binary.

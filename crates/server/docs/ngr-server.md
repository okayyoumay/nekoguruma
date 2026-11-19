# `ngr-server`

The server (design 3.2, 12, 13). `crates/server` is a library plus the binary `ngr-server`. This
build serves the local deployment profile (design 13) only: it listens on loopback, uses
PostgreSQL through sqlx and serves the health probe, the redemption of registration codes, the
agent's connection proof and control channel, the local users' login and the creation of jobs. The rules of the listener and the
request guard are in ADR-456; the database, the migrations and the test database are in ADR-458;
the job instruction key is in ADR-462; registration codes and their command are in ADR-463 and
ADR-465; local users, their command and the operator session are in ADR-469; job creation is in
ADR-477; the bundled object server, which the server starts as its child, is in ADR-491; the
object store, a bucket on that server, is in ADR-492 (ADR-474's local-file store is the test
store).

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
instruction key, an object server or an object store bucket that is refused, and an object server
that ends before a stop signal), or when a code cannot be issued or a user changed, 2 for a
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
path = "/etc/nekoguruma-server/instruction.key"   # absolute; Windows: use a TOML literal string

[object_server]              # required in a file
binary = "/opt/nekoguruma/rustfs"          # absolute; what scripts/fetch-rustfs.sh prints
data_dir = "/var/lib/nekoguruma-server/object-server"   # absolute; created when absent
credentials_dir = "/etc/nekoguruma-server/object-server"   # absolute; must exist; not the key's directory
port = 8081                  # default: http.port + 1
bucket = "nekoguruma"        # default; the object store's bucket
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
- The `[object_server]` table is required in a file (a `Config` built in code may leave it out and
  then starts no child): `binary`, `data_dir` and `credentials_dir` are absolute paths, with no
  search of `PATH` ("The object server" below, ADR-491). `port` is the object server's loopback
  port; the default is `http.port` plus one, and 0, the server's own port and an overflow of the
  default (`http.port` 65535) are refused. There is no address, bind, access-key or secret-key key
  (an unknown key is a parse error), the listener is `127.0.0.1` in code, and the table holds no
  credential. `credentials_dir` may not be the directory of `instruction_key.path`; one directory
  nested in another is not detected, so keep the directories apart. `bucket` is the bucket the
  object store keeps its objects in ("The object store" below, ADR-492): 3 to 63 characters of
  lower-case letters, digits and hyphens, not starting or ending with a hyphen, not starting with
  `xn--` (RustFS refuses it), and not `health`, `minio` or `rustfs`; the default is `nekoguruma`. The server creates the bucket when it is
  absent. The size limit of an object (4 GiB) is a constant of the code, not a key.
- There is no `[storage]` table any more (ADR-492): a file that still has one is refused as an
  unknown key. **Migration:** delete the `[storage]` table from existing files. Objects that
  `LocalStore` wrote into its directory are not moved; nothing in this build wrote any (no route
  used the store).
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
5. Start the object server (below): check its binary, create and lock its data directory, read
   or create its credential files, try its port, spawn it and wait until it answers its readiness
   probe, at most 30 seconds. A failure closes the database pool, stops what was started and ends
   the process with exit 1; nothing is retried. The commands `registration-code` and `user` never
   read the credentials, lock the data directory or spawn the child.
6. Open the object store (below): check the bucket on the object server and create it when it is
   absent, retrying a connection that fails or a 5xx every 100 ms within 30 seconds of its own (after step 5's wait), and stopping
   at once if the object server ends meanwhile. A failure (a refusal of the credentials, a status
   the store does not expect, the budget spent, the child ending) stops the object server, closes
   the database pool and ends the process with exit 1. The commands `registration-code` and `user`
   never open the store.
7. Bind `127.0.0.1:<port>` (a failure stops the object server and closes the pool), install the
   signal handlers, then log the pinned origin: a stop signal sent once the log line appears is
   never lost. A listening port means a migrated schema, a usable instruction key, a ready object
   server and a bucket that is ready. A stop signal during the steps before this one has
   the default disposition and ends the server, and the child ends with it (below).
8. Serve until SIGTERM or SIGINT (Unix) or Ctrl-C or Ctrl-Break (Windows). Then stop accepting,
   let requests in flight finish and close every control channel connection with 1001, for at
   most `shutdown_grace_secs`, and exit 0. A second signal during the grace period ends the wait
   at once (still exit 0). What is still open then is aborted: the HTTP connections first, then
   the hub's tasks (the control channel sessions, the revocation pass, the delivery queue and the
   job expiry task), then the object server is stopped (SIGTERM, at most 10 seconds, then SIGKILL;
   a second signal kills it at once), then the database pool is closed (see "The control channel"
   below). The tasks are started by `Server::run` before it accepts a request, and each one ends
   at the stop, so none runs against a closed pool. A connection that has not delivered a complete
   request head is held for no longer than the header-read timeout below. If the object server
   ends while the server runs and no stop signal has been received, the server stops as for a
   signal and exits 1 (a stop signal and the child's end together is exit 0). A Windows service
   stop control arrives with packaging.

## The object server

`object_server.rs` and `object_server/`; the decision and its alternatives are in ADR-491. The
local profile bundles RustFS, an S3-compatible server, pinned to a release in `scripts/rustfs-pin.txt`,
and runs it as the server's child. The object store (below) is a bucket on it, opened after the
readiness probe.

- **Getting the binary.** `scripts/fetch-rustfs.sh` reads the pin file (the version, and per
  platform the asset name, the asset's SHA-256, the archive member and the member's SHA-256),
  downloads the asset, checks both hashes, extracts only the named member with mode 0755 into
  `target/rustfs/<version>/` (or `NGR_RUSTFS_DIR`) and prints the binary's path; when the binary
  is in place with the right hash it downloads nothing. Platforms: linux-x86_64, linux-aarch64,
  macos-aarch64, windows-x86_64 (the release has no macOS x86_64 asset; that platform is refused by
  name). `scripts/test-fetch-rustfs.sh` tests the script. The server never hashes the binary; it
  refuses, on Unix, a binary (a symbolic link is followed and the target judged and run) or its
  directory that group or others can write or that belongs to neither its account nor root.
- **Credentials.** Two owner-only files in `credentials_dir`, `object-server-access-key` (20
  upper-case hexadecimal characters) and `object-server-secret-key` (40 lower-case), made at the
  first start by a temporary file, a hard link and a directory sync, as the instruction key is
  (ADR-462, `fs_guard.rs`): "object server credential files created" is logged, without values.
  The directory must exist and, on Unix, belong to the server's account or root and not be
  writable by group or others; it is never created. A symbolic link, FIFO or directory in place
  of a file, a foreign owner or a group or other bit (Unix) ends the start; so does a file that is
  not the shape above (empty, short, long, extra lines, another alphabet, not UTF-8), and it is
  never overwritten or regenerated. Windows checks no ACL: the directory's ACL, set when the
  service is installed, is the protection. A `.tmp` file left by a crash between the link and its
  removal is ignored; delete it. There is no rotation: to reset, stop the server, delete the two
  files and start it (RustFS accepted new keys over its old data in the tests). The server gives
  the child the two paths as `RUSTFS_ACCESS_KEY_FILE` and `RUSTFS_SECRET_KEY_FILE`; that is the
  child's own interface and not configuration of the server by environment (ADR-456 item 7). The
  secret is in no configuration, environment, argument or log line.
- **The data directory** holds `.lock` (the local store's lock code: a second server on the
  directory exits 1 before it spawns anything) and `volume/`, which the child is given as its
  volume. The server creates the directory (mode 0700 on Unix) when it is absent and its parent
  exists, and checks it as it checked the local store's directory.
- **The child.** The binary's `server` subcommand, with a cleared environment holding
  `RUSTFS_ADDRESS=127.0.0.1:<port>`, `RUSTFS_VOLUMES`, the two `_FILE` variables,
  `RUSTFS_CONSOLE_ENABLE=false` (RustFS 1.0.6 otherwise opens its console on `0.0.0.0:9001`) and the
  minimum a process needs (`HOME` and `PATH` on Unix, `SystemRoot` on Windows); standard input
  null; the data directory as working directory; its own process group on Unix and no console on
  Windows, so a Ctrl-C at a terminal reaches the server alone, which orders the stop; no
  inherited descriptor. Its standard output and error are drained and forwarded to the log, one
  event per line (target `object_server`), at most 1024 characters, control characters replaced
  and the secret key redacted. The listener is fixed to loopback: the tests check that the host's
  other IPv4 address and `[::1]` refuse, that the child has one listening socket, and that an
  anonymous request is answered 403.
- **Readiness.** After the spawn, `GET /health/ready` on the loopback port every 100 ms, each
  probe at most 1 second, until it answers 200, the child ends or 30 seconds pass. The failure
  names the last probe ("status 503", "cannot connect", "no answer within 1 s", "unparseable
  answer") or how the child ended, and the output the child wrote is in the log above it. Another
  process that answers on the port between the port check and the child's bind is an accepted
  residual.
- **Ending with the server.** The server stops the child in its own stop (above) and in every
  failed start. If the server is killed or aborted instead: on Linux the child has a parent-death
  SIGKILL, bound to the thread that spawned it, which is why a thread of its own spawns it and lives
  as long as the handle; on Windows the server joins a kill-on-close job object before it spawns
  (the stop there is a termination, and the job covers the server's death only); other Unix systems
  can leave an orphan, and the next start fails at the port check, naming `object_server.port`.
  A service manager should signal the server alone first: with systemd, `KillMode=mixed` (SIGTERM
  to the server, SIGKILL to what remains in the control group after the stop timeout).
- **The stop's tail.** After the child is reaped the server kills its process group (Unix), so a
  grandchild that holds the child's pipes or port does not outlive the stop, and then joins the
  two log forwarders together for at most 1 second, aborting what is left.
- **Not covered.** A hung child is not detected (no probe after the start), and a stranger on the
  object server's port is not told from it.

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

`storage.rs` (the `MultipartStore` trait and its types), `storage/s3.rs` with `s3/http.rs` and
`s3/xml.rs` (`S3Store`); the decision, the alternatives and the known limits are in ADR-492. The
store holds the large bodies that the database refers to by key (the `storage_key` columns,
`db/README.md`) in the bucket `object_server.bucket` of the bundled object server. The server does
not carry the bytes: the writer PUTs each part to a presigned URL. The trait's futures are boxed by
hand, so `Arc<dyn MultipartStore>` works, and `Server::object_store()` returns the store
`Server::start` opened (`None` for a `Config` with no `[object_server]`, and for a server made by
`bind`). No route or task of the server uses the store yet.

- `begin()` issues the key (a version 4 UUID in lowercase text) and starts a multipart upload;
  the result is `Upload { key, id }`, which the caller's row keeps whole, since the store records
  nothing. The upload ID is a bearer of access to the upload: its `Debug` shows nothing, it has
  no `Display`, and it is never logged.
- `presign_part(upload, part)` computes a presigned PUT URL for part 1 to 10000 (else
  `InvalidPartNumber`), valid 300 seconds, signed for `127.0.0.1:<port>` and the object server's
  region `us-east-1`, path style. It does no I/O. `PartUrl` has a redacting `Debug` and no
  `Display`; `expose()` gives the text for the writer. The URL does not bind the length of what is
  sent (ADR-492's known limits).
- `list_parts(upload)` pages through the object server's listing (1000 a page) and returns each
  part's number and size, not its entity tag. A listing that is not strictly ascending within 1 to
  10000, or a continuation that does not advance, is `PartsChanged`. An upload that is gone is
  `AlreadyFinished` if the object exists and `NotFound` if not.
- `complete(upload, sha256, length)` takes the key's lock, lists the parts and checks them itself
  (`NoParts`; numbered consecutively from 1, else `MissingPart`; each but the last at least
  5 MiB, else `PartTooSmall`; the sizes add up to `length`, else `PartsLengthMismatch` with the sum,
  answered before anything is completed so the upload stays as it is; at most 10000 parts,
  else `PartsChanged`; at most 4 GiB, else `ObjectTooLarge`), sends CompleteMultipartUpload with the entity tags the
  listing gave (a refusal of a part is `PartsChanged`), then reads the object back, streaming its
  SHA-256, and compares length and digest. A mismatch deletes the object (the delete order below)
  and is answered as `LengthMismatch` (the read-back's length) or `HashMismatch` only once the delete has succeeded; a
  failed delete answers its own error, since the object is still there. A read-back that fails
  (cut, idle for 30 seconds) is an I/O error and deletes nothing. A retry after the upload was
  completed finds the upload gone and the object present and reads the object back again, so a
  completion whose answer was lost is finished by calling it again; with no object it is
  `NotFound`.
- `get(key)` returns the object's length (from its `content-length`) and an `AsyncRead` that
  fails after 30 seconds without a byte and has no total bound. It verifies nothing; `NotFound`
  for a missing key.
- `delete(key)` takes the key's lock, lists the uploads whose key starts with the key, aborts
  those whose key is exactly it, repeats while any were found (at most 3 rounds), then deletes
  the object. Deleting what is not there is `Ok`.
- `list_unfinished()` pages through the uploads of every key in the bucket and returns the
  ones this store could have issued (key, ID, start time); the others are counted in `skipped`
  and not returned. The listing request is built by hand (rusty-s3 has no action for it).

The rules the implementation keeps:

- Every request is a presigned URL built by rusty-s3 and sent over its own HTTP/1 connection to
  `127.0.0.1:<port>` (`connection: close`): no pool, no retry, no redirect, no HTTP/2. A request
  with a body always sends `content-length`, `0` included. A control request is bounded by 30
  seconds, a presigned URL of the store's own is valid 60 seconds, an XML answer is read up to
  4 MiB and an error body up to 64 KiB (the rest is not read).
- Nothing logs or puts into an error a URL (it carries a signature), an object key, an upload ID
  or text the object server sent. An error is a fixed message, an operation name and an HTTP
  status or an `ErrorKind`. A 401 or 403 is `Refused`; any other status the operation does not
  expect is `Server`. A 404 means gone only with the code that says so (`NoSuchKey`, `NoSuchUpload`, or an
  empty answer to a HEAD); `NoSuchBucket` or any other code is `Server`. An error answer is read
  up to the error bound whatever the operation's XML bound.
- The key lock serialises `complete` and `delete` on one key within this process; other calls
  take none. It does not cross processes.
- At open the bucket is checked (HeadBucket) and created if absent (409 "owned by you" counts as
  present); a refusal or an unexpected 4xx ends the open at once, a connection failure or a 5xx is
  retried.
- The bounds above are the constants of a hidden `Tuning` that tests shorten; the store has no
  configuration key.

`ObjectStore` and `LocalStore` (`storage/local.rs`, ADR-474: resumable append by offset into a
directory) stay in the code with their unit tests (`storage/local/tests.rs`); the server does not
use them, but the object server's data directory shares their directory-locking code (`.lock`,
`ensure_area`, `open_locked_dir`). Their settings, `StorageConfig`, are built in code and are not
read from the configuration file.

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

## Creating a job

`jobs.rs`; the decisions are in ADR-477 and, for the vehicle lock, ADR-480.

- **`POST /api/v1/jobs`, in this order, the first step that applies answers.** The guard; the
  method (405 with `Allow: POST`); the session layer of "Logging in" (403 for the Origin, 401,
  503, 500), which answers before the body is read; the body (16384 bytes within 5 seconds: 413,
  408, 400, each closing the connection); exactly one `Content-Type` of `application/json` (400);
  exactly one `Idempotency-Key` of 1 to 128 bytes, each a visible ASCII character, compared as
  bytes (400); the JSON (one object of the eleven `JobRequest` names, each at most once, none
  null, of the right type, nothing after it: 400); the values (400); then one transaction.
- **The values.** `kind` is `acquire` or `writeSettings` (the other five kinds are 400 "not
  supported"); `agentId` a lowercase hyphenated UUID; `vciId` 1 to 1024 bytes without a control
  character; `vin`, when present, well-formed (the agent's rule, `shared_proto::is_well_formed_vin`);
  `irDigests` 1 to 128 entries of 64 lowercase hex digits, in any order and with repeats (stored
  sorted and unique); `artifactDigest`, when present, 64 lowercase hex digits. `remote: true`, a
  non-empty `parameters`, an `executionWindow` and an `onSiteContact` are 400 "not supported";
  `remote: false` and `parameters: {}` count as absent. `startDeadline`, when present, is a
  string that `agent_events::epoch_seconds` reads (the grammar of the device time: upper-case `T`
  and `Z`, an offset of at most 15:59, one to nine fractional digits, a real date from the year 1,
  a second of at most 59), kept as UTC whole seconds with the fraction floored; anything else
  (not a string, null, a duplicate property included) is 400 with no database access. A 400
  means the request can never be valid; a 422 that the key is another request's or that the
  deadline is out of bounds (next item).
- **The transaction** (read committed, 5 second lock and statement timeouts). (a) The row of
  `(tenant, key)`: when it exists, the same operator with an equal request hash is answered with
  the stored job (202), any other request with 422 and no data of the job, whatever the
  caller's permission (the key is judged before the permission). (b) One read of the permission:
  the role `worker` with an empty scope, an agent row of the session's tenant that is unrevoked,
  an allow-list row, and whether the process's key is the active `instruction_keys` row, with
  the database's clock (the second is rounded down). A refusal is one 403 document; the reason (`not_worker`, `agent_unknown`,
  `agent_revoked`, `not_allowed`) goes to the log through its own gate. Then the deadline's
  bounds, when the request names one, against the issue time (the floor of this read's second):
  60 to 86400 seconds after it, both inclusive, else 422 with the Problem `code`
  `START_DEADLINE_OUT_OF_BOUNDS` (nothing written, the key unbound, no audit row, no lock). A key
  that is not the active one is 503 and one logged error, judged after the bounds. (c) The kind's confirmation level is read from
  `confirmation_defaults` (ADR-488); a kind without a row is 500 and nothing is signed or
  written. The ID (UUIDv7 from the database's milliseconds and ten random bytes) and the instruction (version 1, generation 1, the session's operator, the
  sorted unique hashes, no plan, issued at the database's second, start deadline the request's
  second or, with none, 900 seconds later) are sealed; a signer's failure is 500 and nothing is written. (d) `INSERT ... ON
  CONFLICT (tenant_id, idempotency_key) DO NOTHING RETURNING`. On a returned row, and only
  then, the winner takes these steps in this order: the `audit_log` row (`jobCreated`, ADR-484),
  the vehicle's lock (next item), then the agent's unrevoked row and the active key's row,
  locked `FOR SHARE` before the commit, and the commit, so a revocation or a retirement waits for it or is seen; a row gone is a rollback
  and the 403 `agent_revoked` or the 503 for a key that is not active. The commit is the 202.
  The locks come after the insert so that an insert waiting behind another insert of the key
  holds neither row and does not block the agent's connect stamp. Between the insert and these
  locks a job that names a VIN takes the vehicle's lock (next item). (e) No row means another request inserted the key first: the row is read again and
  decided as in (a); a row that is gone again is 503. The conflict path takes no lock. Each lock wait is bounded by the 5 second `lock_timeout`
  (per wait, not per request); one past it, for example on an uncommitted insert of the same
  key, is 503.
- **The row and the answer.** The row holds the signed bytes, the key's ID, the sorted hashes as
  hex (`ir_digests`), the confirmation level (read in the same transaction from
  `confirmation_defaults` for the kind, step (c), ADR-488: `notifyOnly` for `acquire`,
  `gracePeriod` for `writeSettings` as seeded, recorded and not enforced) and `request_hash`: SHA-256 over a tagged, length-prefixed
  encoding of the normalized request (kind, agent, VCI, VIN, hashes, artifact, and, only when the
  request names one, the name `startDeadline` and its normalized seconds, so that a request
  without it hashes as before and another deadline, or the field added or dropped, is another
  request: the existing 422), with the operator left out (the row's operator is compared). A
  replay is answered before the bounds are judged. The 202 body is the `Job`
  (`jobId`, `kind`, `state`, `progressPermille`, `confirmation.level`, and `failure.code` and
  `failure.procedureCode` when the job has a failure code, never the detail), built by one function
  from the row for a creation and a replay, which shows the current state; `Cache-Control:
  no-store`, no `Location`. The delivery queue sends the stored bytes (ADR-476).
- **The vehicle lock** (design 8.8, ADR-480). After the insert wins, a job with a VIN runs one
  `INSERT INTO vehicle_locks ... ON CONFLICT (tenant_id, vin) DO UPDATE ... WHERE expires_at <=
  now() RETURNING 1`. A new row, or an expired one replaced, is the take; no returned row means a
  live lock of another job, and the transaction rolls back (no job row, the key unbound) and
  answers 409 with a Problem whose `code` is `VEHICLE_LOCKED` and which names no holder, expiry
  or VIN. The row's `expires_at` is the instruction's start deadline (at most 24 hours after the
  issue time) plus `Kind::lock_secs` (acquire 600 s, writeSettings 1800 s). The replace condition reads only the lock row, so a
  concurrent take of the same VIN waits for the other transaction and re-checks the row it left;
  a wait past the 5 second lock timeout is 503. A replay answers 202 before any lock is read,
  and a job with no VIN takes no lock. The take comes before the agent and key locks, so its wait
  holds neither. No sweeper removes expired rows: the next take replaces them.
- **The audit row** (design 8.2.3, 4.3, ADR-484). The first statement after the job insert
  wins, before the vehicle lock is taken, in `audit.rs`: `INSERT INTO audit_log ... SELECT ...
  FROM jobs` for the job just inserted, so the row is the job's own values: tenant, operator,
  agent, job, `vin` (NULL when the job has none), the action `jobCreated` and the confirmation
  level, with the database's clock as `recorded_at`. The detail is the kind, `irDigests`,
  `artifactDigest` when there is one and `signingKeyId`; it holds no VIN, request hash,
  idempotency key or VCI ID. A count other than one row, or a failed insert, is 500 or 503 like
  any statement, and the transaction rolls back. A replay and every refusal (409, 403, 503) write
  none: they end before the insert or roll it back. The order is the insert, the audit row, the
  vehicle lock, the agent's lock, the key's lock, the commit. `audit_log` locked for more than
  the 5 second lock timeout is 503 with no job and the key unbound.
- **Not part of creation.** The request cannot set the execution window, remote execution, an
  on-site contact or parameters.
- **Logging.** `job created` at info with the job and operator IDs; each refusal reason, a
  key that is not the active one, a locked vehicle, a 503 and a 500 have their own gate, once a minute. The body,
  the idempotency key, the VCI ID and the VIN are never logged.

## Routes

Every route is mounted through `Mounts::mount` in `lib.rs`, which records its method and path.
`tests/postgres/api_spec.rs` checks the recorded routes against `api/openapi.yaml` (and the
control route against `api/asyncapi.yaml`): an operation the server does not serve carries
`x-ngr-status: planned` there, and every other one must be served (`api/README.md`). The test
also refuses a route mounted any other way.

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
- `POST /api/v1/jobs` (`api/openapi.yaml`; `jobs.rs`; ADR-477): creates an acquisition or a
  configuration write job behind the operator session. See "Creating a job" above.
- `POST /api/v1/agent/uploads`, `POST /api/v1/agent/uploads/{uploadId}/parts/{part}` and
  `GET /api/v1/agent/uploads/{uploadId}` (`api/openapi.yaml`; `uploads.rs`; ADR-493): the data
  channel's begin, part URL and status. See "The data channel" below. The completion
  (`POST .../complete`) is planned and not served.

## The data channel

`uploads.rs` serves begin, the part URL and the status (ADR-493 items 1 to 5 and 15 to 18). The
routes are built from `AppState::uploads` (`uploads::Backend`: the object store, which is `None`
on a server without `[object_server]`, and the part size), so a store set after the router is
built does not reach them; `Server::start` passes the store it opened, and tests use
`Server::bind_with_store`.

A request is judged in this order: the guard, the method (405), the body (4 KiB, within 5
seconds: 413, 408, 400; the part URL and the status take no body), the shape (400), begin's length
(over 4 GiB: 413), the token (401 with `WWW-Authenticate: Bearer`, the same answer as for control
and for a revoked or missing agent row), the agent's row (a revoked or missing row drops the
agent's tokens; 503 when it cannot be read), the job or the upload (404, also for another
agent's or tenant's), the request against the row (a part past the count 400, a row that is not
`open` 409 with its `state`) and the store (503, or 500 for a stored key or upload ID that does not
parse and for a store without randomness). Begin compares the declared generation with the one in
the job's stored instruction (404 on a difference) and does so in any job state. Every database
call is a short transaction of its own, and none is held while the store is called; the row is
inserted with `ON CONFLICT ... DO NOTHING` on the partial unique index, and the loser of a race
deletes the upload it began and answers the winner's row.

The status counts the leading parts the store lists with the expected size. The part size of a row
is the 5 MiB the server had when the row was begun (`uploads::PART_BYTES`; the test override is
`Backend::with_part_bytes`).

Never logged: the token, the body, the digest, the job, an object key, an upload ID, a URL and the
text of a database error (the SQLSTATE only).

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
  (the frame over 131072 bytes with the widest command ID, `c-` and the 20 digits of `u64::MAX`, so that the verdict does not depend on the counter; judged before the connection is looked at). A frame queued on a
  connection that has been told to close, or at a stop, is not written. `Hub::inbox()` gives its
  receiver (256 messages) to the first caller: `commandAck`, `commandRejected`, `agentEvent`
  and `reconcile` arrive as `Inbound { agent, connection, message, text }` in read order, never a
  heartbeat; `text` is the exact frame text of a numbered `agentEvent` (the event is stored as
  it was sent) and `None` for every other message. The task asks the hub per message: with no receiver (not taken yet, or dropped) the
  message is dropped with a gated debug line, never buffered. With the inbox full the task holds
  the message and waits, reading nothing (so the agent sees no echo), while it watches its close
  instruction and the stop. The hub keeps no delivery table. `Hub::close_agent(agent, code)`
  gives the current connection's close instruction once (a code that is not one of the server's
  own close codes is refused with `false`), `Hub::connected()` lists the
  current connections, and the events' answer goes through two more calls (ADR-479):
  `Hub::ack_events(agent, connection, seq)` raises the connection's one-value slot of the
  latest number to acknowledge and wakes its task, and `Hub::close_connection(agent, connection,
  code)` gives the close instruction by connection number. Each returns `false`, and does
  nothing, unless the connection is still the agent's current one and has not been told to close
  (`ack_events` also not after the stop began), so a replaced connection's events never
  acknowledge or close its successor. The task writes one `eventAck {latest}` when its slot
  changes, never after a close instruction or a stop, and its read loop does not look at the slot
  while it waits on a full inbox (it is inside the hand-over then).
- **The revocation pass (`revocation.rs`).** One task of the hub's set, spawned by
  `Server::run`: a tick every 30 seconds (the first one 30 seconds after the start, a missed
  tick delayed, so a long jump gives one pass), no query when nobody is connected, otherwise one
  read-only statement (`db::live_agents`) over the connected agent IDs. An agent that is
  connected and not returned (a missing row included) has its tokens dropped, then its
  connection told to close with 4002, with one warn line; a failed read is one gated warning
  and closes nobody. It ends when the stop begins, in its wait and in its read.
- **The delivery queue (`job_queue.rs`, ADR-476).** One task of the hub's set, spawned by
  `Server::run` when the queue is on (`Server::start` turns it on with a 2 second period and a 90
  second lease; `Server::bind` leaves it off, and a test turns it on with `with_job_queue`), and
  the only consumer of the hub's inbox. Each iteration takes the connection snapshot, drains the
  inbox, settles what it holds and releases the deliveries of connections that are no longer
  current, then, on a tick, runs a pass: one transaction that ends the jobs of a retired key
  (`failed`, `instructionKeyRetired`), takes the `received` rows of the connected, unrevoked
  agents `FOR UPDATE SKIP LOCKED`, queues each stored instruction (base64 of the stored bytes)
  through `Hub::send_job_command` and marks the rows queued (`delivery_attempts`, and `delivery_sent_at`
  as the time of the mark statement, which the flight keeps as the token of that delivery). The
  take reads, for each connected agent, a lateral select of its oldest rows (at most the outbound
  queue's 16), ranks them within the agent and keeps the first 64 of all by (rank, creation time,
  ID): every agent with work gets its oldest job into the pass before any agent gets its second,
  an idle agent takes no place, the transaction stays short and no state is kept between passes.
  Those two steps lock nothing (a window function cannot be combined with `FOR UPDATE`, and locking
  before the limit would hold rows the limit drops); a last step locks the chosen rows `FOR UPDATE
  SKIP LOCKED` and checks their conditions again, so a row another pass holds is skipped and
  becomes free when that pass commits. The sweep skips rows another pass holds too. The take
  also returns the row's mark as it was (null or an expired one): a flight keeps that mark
  (`prior`) and the mark its own statement returned (`returned`); after a commit that succeeded
  only `returned` counts, after one that failed either does, as a failure can be reported for a
  commit that happened. The answer's writes compare the row's mark and attempts with them: `prior`
  goes with n attempts and `returned` with n + 1.
  A frame over the limit (`SendError::TooLarge`) is not like a full queue or a missing connection
  (`Busy`, `NotConnected`: the job stays queued): it can never be sent, so the job is ended as `failed`
  with `internal`.
  Every statement that ends a job here (the sweep, the too-large end, a rejection) is one
  statement that also deletes the vehicle lock the job holds (`WITH ended AS (UPDATE ... RETURNING
  ...) DELETE FROM vehicle_locks ... WHERE job_id = ended.id`, ADR-480), so a lock of another job
  on the same VIN stays and a rejection that ends nothing frees nothing; the sweep counts the
  jobs it ended, not the locks it freed. A create that holds a lock row (also on the 409 path,
  since `ON CONFLICT DO UPDATE` locks the conflicting row) can make the ending statement wait; past
  the 5 second lock timeout that pass fails and the next tick retries.
  While a pass or a write waits for the database the task keeps reading the inbox into a buffer
  (4096 messages at most, applied by the next iteration), and an answer that could not be
  written is kept and written again on the next tick (not on every message); what was buffered during a write is applied right after it, and at the latest on the next tick. Two instances never hold one row at once; a commit that
  fails after a frame was queued leaves the row as it was before the send (or, if it went through though it was reported as failed, at the new mark), so it can be sent again. An
  acknowledgement sets `delivered_at`; a rejection ends the job
  (`failed`, `instructionRejected`, the agent's reason as the detail); both, and a release, write only while the
  job is neither finished nor delivered and still carries a mark that delivery expects (see above)
  with the attempts that go with it. `progress` and `reconcile` messages are dropped with a
  debug line. The numbered `agentEvent`s are split out of the buffered messages before the
  answers are applied and stored by `agent_events.rs` (ADR-478 item 6, ADR-479): the answers of
  the iteration are written first, then each agent's events, in the order read, in one
  transaction per agent (at most 64 agents per round). The transaction takes the tenant from the
  agent's unrevoked row (a revoked or missing agent gets nothing stored or answered), inserts the
  cursor row `ON CONFLICT DO NOTHING`, locks it `FOR UPDATE` (so two instances take turns), and
  applies the pure rule `Plan` to each event: a number at or below the cursor is a repeat whose
  text is compared with the stored row's in SQL (equal, or no row because it was deleted, is
  answered with the cursor; a different text stores nothing, acknowledges nothing and ends that
  connection with 1002 and one gated warning with the agent ID and the number, never the text);
  the next number is inserted with its kind, job ID (only when a canonical UUID), device time
  (only when `strict_device_time` accepts it) and the exact text, and a `jobState` event is then
  applied to the sender's own job in the same transaction (design 5.6, ADR-481): `job_state.rs`
  reads the event into a `Report` (the state, the failure code kept for failed and
  preconditionFailed only, the server-only codes and unknown names read as `other`, the
  procedure code only for failed with `procedure` and 1 to 128 bytes without U+0000, never cut, the detail
  only for failed, preconditionFailed and needsOnSiteRecovery with U+0000 replaced by U+FFFD and
  the text cut to 1024 bytes on a character boundary, empty as none), the row is locked
  (`SELECT ... FOR UPDATE` on the tenant, job ID and the sender as `agent_id`: another agent's
  job is no row) and the pure `decide` picks one write: nothing (no row, which a job ID that is
  not canonical is too; finished and final;
  unknown name; a `received` on a finished needsOnSiteRecovery), the `received` stamps
  (`last_reported_at`, `delivered_at` when null; never the state), the report time alone (the
  state is the row's), a move (the state and both stamps; on a reopened needsOnSiteRecovery also
  the end and the failure columns cleared, no lock taken again) or an end (the state, the failure
  columns, `finished_at` and the stamps, in one statement with the deletion of the job's own
  vehicle lock, `WITH ended AS (UPDATE ... RETURNING) DELETE FROM vehicle_locks ... WHERE job_id
  = ended.id`, ADR-480). Every time is the transaction's `now()`. A report that wrote nothing
  because the job is not the sender's or does not exist, is final, or the state name is unknown
  (not a late `received` on a finished job) is stored and acknowledged and counted as ignored;
  after the commit the queue logs one line per answer under a gate of its own (not the 1002
  gate): the count of such reports and the first one's agent ID, job ID (when canonical),
  number and the two states' names (`unknown` for no job), never the text; any other number, one no
  `bigint` holds included, is a gap that closes its connection with 1002 and ends its later
  events in the round. The cursor is moved to the last number stored and the transaction
  commits; only then are the answers given through the hub (`Hub::ack_events` for each
  connection, `Hub::close_connection` for each close), and the counters of `Settings::events`
  (seen, stored, repeats answered, applied; for tests) are raised. The events of a connection that ended
  are stored and not acknowledged. A failed transaction (the statements run under the 5 second
  lock and statement timeouts, so a held cursor row is a failure after 5 seconds) keeps its
  events and those of the agents not yet tried, ends the round and is tried again on the next
  tick, with one gated warning, the failed agent's events behind the others' so that only that
  agent waits; a store failure and a failure to write answers are kept apart, so the first does
  not delay settling an answer. A connection the queue closed with 1002 keeps its later events
  from being stored, in later rounds too, until it leaves the hub's snapshot; the stop is checked
  between agents. The kept events count with the buffered messages against the
  4096 bound, and past it the inbox is not read. A failed pass sends nothing and is one gated warning (60 seconds). The task ends
  when the stop begins, in its wait and in its read.
- **The job expiry task (`job_expiry.rs`, ADR-487).** One task of the hub's set, spawned by
  `Server::run` when it is on (`Server::start` turns it on with a 30 second period; `Server::bind`
  leaves it off, and a test turns it on with `with_job_expiry`, which also takes the counters of
  passes and failures a test waits on). The first tick is one period after the start, a missed
  tick is delayed (one pass, not a burst). A pass is `expire`: one transaction with the 5 second
  timeouts and one statement (`EXPIRE_SQL`) that sets the jobs with `state = 'received' AND
  finished_at IS NULL AND start_deadline <= now()` to `expired` with `finished_at = now()`, chosen
  inside `WHERE (tenant_id, id) IN (SELECT ... FOR UPDATE SKIP LOCKED)` as the retired-key sweep
  does, and deletes the vehicle lock row that names each ended job in the same statement (the
  fragment `db::delete_own_lock!`, shared with the queue's ends and the agent's reported end). It
  compares only the database's clock, has no term for `delivered_at`, the mark or the lease, and
  leaves the failure columns, `delivered_at`, `last_reported_at` and the mark as they are. It
  writes no `agent_events` row, moves no cursor and writes no `change_feed` or `audit_log` row. A
  row another transaction holds is skipped and ended by the next pass. A pass that ends jobs logs
  the count at info; a failed pass writes nothing and is one gated warning (60 seconds), tried
  again at the next tick. The task ends when the stop begins, in its wait and in its statement
  (the dropped statement rolls back), before the pool closes. A job that an agent reports on
  after its end is stored, acknowledged and not applied (`job_state::decide`).
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
  write `agents.last_seen_at` through the stamp, the revocation pass reads `agents`, the delivery
  queue reads and marks `jobs`, and everything else the agent says goes to the inbox, which the
  queue takes.

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
- The object server's tests need the pinned RustFS binary: run `scripts/fetch-rustfs.sh` and set
  `NGR_TEST_RUSTFS_BIN` to the path it prints (CI does this in the `core-linux` and
  `core-windows` jobs). A missing binary fails the real-binary tests with one message that names
  the script. The tests of the child's lifecycle run against a fake child, the second binary of
  this package, `ngr-fake-object-server` (`src/bin/`; an installation does not use it), which
  takes RustFS's arguments and environment and is steered by `fake-object-server.cfg` in its data
  directory: late or never ready, garbage or no answer, ending at once or later (by a code or a
  signal), ignoring SIGTERM, printing the secret, leaving a grandchild on its pipes, reporting
  its environment and descriptors.
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
  `object_server.rs` covers the child against the fake (ADR-491): the binary and data directory
  checks before anything is made, the credential files at a start, readiness (never ready, 503,
  garbage, hung probes, just under the bound), a child that ends early or later or by a signal,
  the stop sequence (TERM, the bound, KILL, a second signal, a failing wait, a pipe held by a
  grandchild), what the child inherits (environment, working directory, descriptors, process
  group), the output forwarded without the secret, the guards (a failure after the spawn, a
  dropped handle or start future, a thread that ends), the start with the server (a failure
  before, in and after step 5, a taken port, the run ending with the child, the stop signal and
  the child's end together), and the server as a process (the commands touching nothing, the
  listening line after readiness, a signal during the start and a kill taking the child with it,
  on Linux). `object_server_rustfs.rs` runs the real binary: readiness, 403 for an anonymous
  request, the stop, the loopback-only listener, the one listening socket, no secret in the
  command line, environment or log, new keys after the files are deleted, and a killed server
  taking the child. Units are in `src/` (`config.rs`, `object_server/credentials.rs`,
  `object_server/forward.rs`, `object_server.rs`).
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
  `job_queue.rs` (unit tests in `src/job_queue.rs`) covers the queue: unit tests for the dispatch
  (a full queue, no connection, a frame over the limit, a stop between two sends) and for the
  settling of answers; over PostgreSQL, fake agents on WebSocket and rows the test inserts:
  delivery with the stored bytes and the acknowledgement, an agent that is not connected, two
  servers on one database (every job on one socket, the take skipping held rows, the plan using
  `jobs_pending`), a dropped take, a server that goes away and the lease, a dropped socket and a
  replacement, a rejection, answers that write nothing, a retired or missing key, a revoked
  agent, a past deadline, the stop, a failing pass, and `Server::start` turning the queue on.
  `job_expiry.rs` covers the expiry statement and its task (ADR-487; the case names in its
  comments are the rows of the ADR's table): a job ended from its deadline and not before,
  delivered or not, with no failure and no write to the event, cursor, feed or audit tables; the
  deadline's own instant against the queue's take in one transaction; every other state and every
  finished job untouched and a second pass rewriting nothing; the lock row of the ended job
  deleted and no other (a job with no VIN, a row naming another job, none, another tenant's); the
  one-statement end (a deletion that fails leaves the job `received`); a row another transaction
  holds skipped at once and ended by the next pass; two passes at once; a revoked agent's job;
  an answer or a departure after the end writing nothing; the task's end of what was past at its
  start, its stop during a statement, and its failing pass logged once and retried; and
  `Server::start` turning it on. The
  agent binary's refusal of a forged stored command, and its acceptance of a job created through
  the route, are in `agent_connect.rs`, which also follows three accepted jobs' `received` events
  through a held cursor row, a kill of the agent, a restart (every event a repeat, answered) and a
  fourth job's event stored and acknowledged, with nothing sent or closed after a further restart.
  `agent_events.rs` (database; the pure rule, the strict device-time check and the event's
  derivation are unit tests in `src/agent_events.rs`, the queue's split, grouping, bound and
  failure in `src/job_queue.rs`, and the hub's slot and scoped close in `src/hub.rs`) covers the
  storing of events with fake agents: events in order stored once with their exact text and the
  cursor, an acknowledgement never read before its row and cursor are visible, a repeat with the
  same text answered with the cursor, a repeat with another text (nothing stored or
  acknowledged, 1002 on that connection only, the warning without the text), a repeat of a
  deleted row, a gap beside a sibling agent, numbers beyond `bigint`, a repeat in one batch, a
  refused commit (kept and stored later), a commit that went through though reported as failed
  (staged), a connection replaced or gone between the store and the acknowledgement, two servers
  and a held cursor row, a first store beside a concurrent creation of the cursor row, a revoked
  or missing agent, tenants and the job an event names, a `received` event only stamping its job, a
  NUL escape, a frame of the limit and device times, the apply rules (received, running,
  completed, the failure columns, a NUL and an oversized detail, a repeat, a later `received`,
  the same state twice, a report after an end and its warning, a reopened needsOnSiteRecovery,
  another agent's, another tenant's, an unknown and a non-canonical job, a job the server ended,
  the delivery settled by `received` beside a late `commandAck`, a report and an answer in one
  round in both orders, a refused commit retried), `progress` and `reconcile` not stored, and
  a re-registered agent's own cursor.
  `jobs.rs` (unit tests in `src/jobs.rs` for the shape rules, the request hash, the job ID, the
  largest request, the deadline's bounds and the pure decisions; the parser's are in
  `src/agent_events.rs`) covers `POST /api/v1/jobs` over PostgreSQL: the layer
  and the body bounds, creation and its row, the stored instruction verifying under the key, the
  idempotency key (replay, conflict, a key of another tenant, a replay after delivery, the key
  judged before the permission), concurrent requests (two at once, two servers, a key held by an
  uncommitted insert that then commits or rolls back, a lock wait of 5 seconds), each refusal of
  permission, a key that is not active, a failing signer, a locked table, and delivery to a fake
  agent, and the lock's end with the agent's reported end (held by a state that is not one,
  freed by completed or needsOnSiteRecovery, another job's later row left alone) and the replay
  showing the failure code and procedure code and no detail, and the audit row (one `jobCreated`
  row per created job with its columns and detail, none for a replay, a conflict, a refusal or a
  rolled-back creation, one per job under concurrent requests, the same VIN in two tenants, rows
  unchanged by a job's end and by the deletion of its row, and a locked `audit_log` as a 503 that
  leaves no job and blocks no connect stamp), and the request's `startDeadline` (signed, stored
  and the lock's base in every form of one instant, refusals outside the bounds with their code
  and nothing left, 403 before 422 before 503, malformed values as 400, replays with another
  deadline or the field added or dropped as the key's 422, a replay not judged again, two requests
  at once, and an expired job freeing its vehicle with its replay showing `expired`). The migrations are checked in
  `migrations.rs`.
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
  boundary, the numbering, the close instruction), the event acknowledgement slot, the scoped
  close and `leave`, and the pass (`revocation.rs`
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
  refused key gives exit 1 and nothing listens. Every server test config names a key path; the tests
  that start several servers on one cluster share the key path (`TestPostgres::key_path`).
- `storage.rs` (database) tests the S3 store on the real pinned RustFS (`NGR_TEST_RUSTFS_BIN`, as
  `object_server_rustfs.rs`), with a plain HTTP client PUTting to the part URLs: an upload in two
  parts that completes and reads back (and no signature or secret in the child's log); a resume
  that re-sends a part and a second completion that verifies again; a digest mismatch that leaves
  neither object nor upload and the same on a retry; the store's own part checks (none, a gap, a
  small part, a wrong length) with the upload still usable; one empty part; delete of an
  unfinished upload, of a finished object, twice and of a key that never was; the unfinished
  listing; an edited part URL and an anonymous read refused; the bucket made once and separate by
  name; an upload that survives a restart of the object server; and `Server::start` opening the
  store, a restart finding the object, and a `Config` with no `[object_server]` having no store.
  `object_server.rs` checks with the fake child's `s3_status` key that a refused bucket stops the
  child and closes the pool, that the child ending while the store retries ends the start, and
  that a 5xx is retried until the budget is spent. `binary.rs` checks that a refused bucket gives
  exit 1 with nothing listening, and `registration.rs` and `local_users.rs` that their commands
  never create the object server's data directory.
- The S3 store's unit tests (`storage/s3/tests.rs`) need no database and no RustFS: a scripted
  loopback listener stands in for the object server and records each request. They cover the open
  (existing, created, owned by us, refused, retried, spent, unreachable, a bad bucket name), the
  HTTP rules (`content-length: 0`, one connection per request, no redirect, the control timeout,
  the XML and error bounds, garbled answers), that no error or `Debug` holds a URL, key, upload ID
  or server text, the part URL (300 seconds, the part number signed, 1 to 10000, no I/O), the
  listings (paging, the markers, a listing that goes backwards or stalls, a gone upload, the
  skipped foreign uploads and the sorted signed query), every check of `complete` and that it
  sends nothing then, the read-back (a mismatch deletes, a failed delete is answered instead, a
  length mismatch, a cut read-back deletes nothing, the idle timeout, a retry after completion,
  an upload lost to a race, the object server's own refusal of a part), the key lock (two
  completions, a delete behind a completion, another key free), `get` and the delete order (the
  exact key, three rounds, nothing to delete, a failed abort).
- The local store's unit tests (`storage/local/tests.rs`) need no database and run on every
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
  removed under a live store; a retried delete that syncs before `NotFound`; a failed unlink; a `.lock` or staging file of another account (Unix; the second as root); and the listing with sizes and times.
- The unit tests need no database: the guard's decision table, the configuration (including that
  no error shows the URL, that the key path is required and absolute, that `[storage]` is refused and
  that the bucket defaults to `nekoguruma` and is validated), the key file (create,
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

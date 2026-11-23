# ADR-491: The Server Starts the Pinned RustFS as Its Child, Stops It With Itself and Holds It to Loopback

**Date:** 2026-11-18
**Status:** Accepted (item 1 loses the `storage.dir` rule; item 2's signed check is ADR-492's HeadBucket; items 2, 3, 6 and 8 and the port residual gain TLS, a certificate and key beside the credentials, `RUSTFS_TLS_PATH` and a pinned probe, and the listener is no longer relied on to be unshared, ADR-505)
**Affects:** design 13, 13.3; `crates/server` (`object_server`, `config`, `fs_guard`, `lib`, `bin/ngr-fake-object-server`, `tests/postgres/`), `crates/server/docs/ngr-server.md`, `scripts/fetch-rustfs.sh`, `scripts/rustfs-pin.txt`, `.github/workflows/ci.yml`, CLAUDE.md "Building and testing", `docs/glossary.md`; extends ADR-458's start order and ADR-462's file rule; details ADR-456 item 7

## Context

Design 13 gives the local profile an object store of local files (ADR-474) and the standard
profile an S3-compatible one. The maintainer decided on 2026-11-17 that the local profile also
ships an S3-compatible server, RustFS (Apache-2.0), so that the object-storage interface the
standard profile uses is exercised where the server is tested and used, and on 2026-11-18 chose
release 1.0.6 as the pin. Nothing reads or writes through it yet: the S3 `ObjectStore` and the
data channel's use of it are separate work, and so is the change to ADR-474 they bring. This ADR
settles how the server holds the program: who starts and stops it, where it listens, how it
authenticates, how a pinned binary reaches an installation and CI, and what the tests need.

Facts about RustFS 1.0.6 that the decisions rest on, found by running it: `rustfs server` reads
its address, its volume and its credentials from `RUSTFS_*` variables, the credentials also as the
paths of files (`RUSTFS_ACCESS_KEY_FILE`, `RUSTFS_SECRET_KEY_FILE`); it needs its volume
directory to exist; it answers `GET /health/ready` with 200 within a second of its start; it
starts a web console on `0.0.0.0:9001` unless `RUSTFS_CONSOLE_ENABLE` says false; SIGTERM ends it
with exit code 0 at once; it does not print its secret.

## Decision

1. **The server's child, one `[object_server]` table.** The server starts the object server as
   its child and stops it with itself. The table is required in a configuration file (`Config`
   has an `Option`, so a `Config` built in code starts no child) and names `binary`, `data_dir`,
   `credentials_dir` (all absolute, no search of `PATH`, ADR-456 item 7) and an optional `port`
   (default `http.port` plus one; 0, `http.port` itself and an overflow are refused). There is no
   address, bind, access-key or secret-key key: an unknown key is a parse error. `data_dir` may
   not be `storage.dir`, and `credentials_dir` may not be the directory of `instruction_key.path`;
   one directory nested in another is not detected.
2. **Start order step 5.** After the instruction key and before the object store: the binary is
   checked, `data_dir` is created (mode 0700, its parent must exist), checked, locked with the
   object store's `.lock` code and given a `volume` directory, the credential files are read or
   created, the port is tried on `127.0.0.1` (a taken port names `object_server.port`), the child
   is spawned, and the server waits for `GET /health/ready` on its loopback port to answer 200: at
   most 30 s, each probe at most 1 s, and no longer than the child lives. Every failure after the
   spawn runs the one stop sequence and closes the pool; a failure before it leaves no child.
   The commands `registration-code` and `user` never read the credentials, take the lock or spawn.
   A signed request as the readiness check comes with the S3 `ObjectStore`.
3. **Credentials are two owner-only files, created by the server.** The access key (20 upper-case
   hexadecimal characters) and the secret key (40 lower-case) are written by the server at the
   first start into `credentials_dir`, which must exist, with ADR-462's rules: a temporary file,
   a hard link and a directory sync (so a file appears complete or not at all and of two
   starts one link wins), the code shared with the instruction key's file; a symbolic link,
   FIFO, directory, a foreign owner, a group or other bit (Unix) or a directory that group or
   others can write is refused; a file is never overwritten or regenerated, and a corrupt one ends
   the start with exit 1. There is no rotation: stop, delete the two files, start. The server
   passes the child the two file paths as `RUSTFS_ACCESS_KEY_FILE` and `RUSTFS_SECRET_KEY_FILE`.
   That is the child's own interface, not configuration of the server by environment (ADR-456
   item 7): no secret is in the configuration, an environment, an argument or a log, and the
   forwarding of the child's output redacts the secret.
4. **No hash at start; a pin and one script.** The server does not hash the binary. It refuses one
   (and its directory) that group or others can write or that belongs to neither its account nor
   root (Unix; a symbolic link is followed and the target judged and run). The version, the asset
   name and the SHA-256 of the asset and of the extracted binary, per platform, are in
   `scripts/rustfs-pin.txt`, outside `docs/` and not Markdown so that CI counts a change as code;
   `scripts/fetch-rustfs.sh` downloads, checks, extracts only the named member (mode 0755) and
   skips the download when the binary is in place with the right hash. It serves installations
   and CI (`NGR_TEST_RUSTFS_BIN`). A platform with no row (macOS on x86_64: the release has no
   asset) is refused by name.
5. **The child ends with the server, however the server ends.** The child handle kills the child
   when dropped (a failed start, a dropped start future, a `Server` dropped without `run`). Against
   a killed or aborted server the kernel acts: on Linux the child has a parent-death SIGKILL, which
   is bound to the thread that spawned it, so the child is spawned by a thread of its own that
   lives as long as the handle; on Windows the server puts itself in a kill-on-close job object
   before it spawns (kept for the life of the process: closing it would end the server), and the
   child inherits the membership; other Unix systems have neither, so an orphan is possible and the
   next start fails at the port check with a hint.
6. **What the child inherits.** A cleared environment rebuilt from `RUSTFS_ADDRESS`
   (`127.0.0.1:<port>`), `RUSTFS_VOLUMES`, the two `_FILE` variables, `RUSTFS_CONSOLE_ENABLE=false`
   and the minimum a process needs (`HOME` and `PATH` on Unix, `SystemRoot` on Windows); standard
   input null; output piped and forwarded per line through `tracing`, bounded in length, with
   control characters replaced; the data directory as working directory; its own process group on
   Unix and no console on Windows, so that a Ctrl-C at the terminal reaches the server alone, which
   orders the stop. No descriptor is inherited. The `Command` is never formatted: its `Debug`
   prints the environment.
7. **The stop sequence and the exit codes.** One function stops the child (used by a failed start
   and by the run's end): SIGTERM (Windows: terminate), wait up to 10 s, SIGKILL, reap; a second
   stop signal skips the wait; the child's process group is killed after the reap (Unix); the log forwarders are
   joined together for at most 1 s and aborted after that; a failure is logged and changes no exit code. `Server::run` stops the child after the
   connections and before it closes the pool. The server exits 1 when the child ends before a stop
   signal was received (after stopping as for a signal; restarts are the service manager's job),
   and 0 when a stop signal and the child's end come together. One wait owner: `run` polls the
   child's end beside its other events. Docs recommend systemd `KillMode=mixed`, so that a stop
   signals the server alone first and the whole group only if it hangs.
8. **Loopback only, and nothing else listens.** The child's address is fixed in code to
   `127.0.0.1`; its console is off; the tests check, against the real binary, that the host's
   non-loopback address and `[::1]` refuse and that the child's only listening socket is the
   loopback one (Linux, through `/proc`), and that an anonymous request is answered 403.
9. **Tests.** The child's lifecycle is tested against a fake child (`ngr-fake-object-server`, a
   second binary of the server's package that the tests start; it is not part of an installation),
   which takes RustFS's arguments and environment and misbehaves on a steering file in its data
   directory. The real binary is tested in a few tests that need `NGR_TEST_RUSTFS_BIN` and fail,
   naming the script, when it is not set.

## Consequences

- The server's tests need the pinned RustFS beside PostgreSQL: CI runs the script in `core-linux`
  and `core-windows`; a developer runs it and sets the variable (CLAUDE.md "Building and testing").
- Accepted residuals: another process may answer on the object server's port between the port
  check and the child's bind (the child then ends before it is ready, or the probe reaches the
  other process); a hung child is not detected (no liveness probe after the start); the
  nesting of one configured directory in another is not detected; on Windows the stop is a
  termination, and the job object covers the server's own death only.
- A wrong-architecture binary is run by the C library's `execvp` through the shell, so the start
  fails with the shell's end before readiness, not with a spawn error.
- ADR-474's object store is unchanged: it is still the local files. The S3 `ObjectStore`, the
  signed-request readiness check and the data channel's change to ADR-474 are later work.
- Pinning a new release is a change to `scripts/rustfs-pin.txt`: new asset names and hashes, and a
  check that the child's listeners and environment are still the ones above.

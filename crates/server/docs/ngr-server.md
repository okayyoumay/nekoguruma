# `ngr-server`

The server (design 3.2, 12, 13). `crates/server` is a library plus the binary `ngr-server`. This
build serves the local deployment profile (design 13) only: it listens on loopback, has no
database yet and has one route, the health probe. The rules of the listener and the request guard
are in ADR-456.

## Running

```sh
ngr-server --config <file>
```

`--config` is required. There is no default path, no search and no environment variable.
Exit codes: 0 after a graceful stop, 1 when the configuration is refused or the server cannot
start or run, 2 for a usage error.

## Configuration

One TOML file; every table refuses unknown keys, so a misspelt key is an error and not a silent
default.

```toml
profile = "local"            # the only value this build accepts; the default

[http]
port = 8080                  # 1-65535, default 8080
shutdown_grace_secs = 30     # default 30, at most 300
```

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
3. Bind `127.0.0.1:<port>`, install the signal handlers, then log the pinned origin: a stop
   signal sent once the log line appears is never lost.
4. Serve until SIGTERM or SIGINT (Unix) or Ctrl-C or Ctrl-Break (Windows). Then stop accepting,
   let requests in flight finish for at most `shutdown_grace_secs`, and exit 0. A second signal
   during the grace period ends the wait at once (still exit 0). A connection that is still
   open then is dropped. A connection that has not delivered a complete request head is held
   for no longer than the header-read timeout below. A Windows service stop control arrives
   with packaging. A long-lived connection such as a WebSocket never finishes by itself, so the
   routes that hold one must close it when the shutdown begins.

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

- `GET /api/v1/health` (`api/openapi.yaml`): 200 `{"status":"ok"}`, `Cache-Control: no-store`,
  no login. In this build it is a liveness answer only and checks no dependency, because the
  server has no database yet; once it has one, the route answers 503 while the database does not
  respond.

Any other path answers 404 with a problem body, after the guard.

## Tests

- `cargo test -p server` needs nothing outside the workspace. The unit tests cover the guard's
  decision table and the configuration. `tests/serving.rs` runs the server in the test process on
  an ephemeral port and drives it over TCP with hand-written HTTP/1.1 requests, which gives the
  test every header, and covers the stop: a request in flight finishes during the grace period, a
  stalled one is dropped at its end, a second signal ends the wait. `tests/binary.rs` starts
  `ngr-server` as a process (on a port that was free a moment before, trying again when another
  process takes it): usage and start failures, health, and the SIGTERM stops on Unix.
- The HTTP/1-only test must also pass in a workspace build, where hyper's HTTP/2 is enabled:
  build with `cargo test --workspace --no-run` and run the `serving` binary.

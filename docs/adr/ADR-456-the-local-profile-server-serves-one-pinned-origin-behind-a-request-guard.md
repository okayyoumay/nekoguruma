# ADR-456: The Local-Profile Server Serves One Pinned Origin Behind a Request Guard

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** `crates/server`, design 13.3, `api/openapi.yaml`, `crates/server/docs/ngr-server.md`

## Context

Design 13.3 names the protections of the local deployment (listen on 127.0.0.1 only, Host
header and WebSocket Origin validation, one pinned name that the other redirects to) without the
rules: which Host values are served, what the other loopback name gets, which statuses answer
which failure, and what a missing Origin means. Other OS users on the same PC can reach the
port, and a web page on any site can make the browser send a request to it. The agent connects
without an Origin header; a browser always sends one on a WebSocket handshake, and page script
cannot change it. The design also does not say where the server's configuration lives.

## Decision

1. The listener is fixed to `127.0.0.1:<port>` in code. There is no bind-address key, so no
   configuration can make the local profile listen elsewhere, and `[::1]` is not bound.
2. The pinned origin is `http://127.0.0.1:<port>` (without the port when it is 80). An IP
   literal needs no name resolution, a browser that tries `localhost` as `::1` first would miss
   a 127.0.0.1-only listener, and `api/openapi.yaml` already names this origin for the local
   deployment.
3. One middleware, applied after every route and the fallback so it runs before routing for
   every request, decides in this order, the first match winning:

   | # | Request | Answer |
   |---|---|---|
   | 1 | Request target not in origin form (absolute form, authority form, `*`) | 400 |
   | 2 | No Host header, more than one, empty, with userinfo, or not `host[:port]` | 400 |
   | 3 | Host is `localhost:<port>` | 307 to the absolute pinned URL, same path and query, any method, no handler runs |
   | 4 | Host is anything but `127.0.0.1:<port>` | 421 |
   | 5 | Host is `127.0.0.1:<port>` and more than one Origin header, or one that is not the pinned origin | 403 |
   | 6 | Host is `127.0.0.1:<port>` and Origin is absent or the pinned origin | the route |

   The middleware wraps the whole route table as a service, so it runs before routing: a request
   it rejects never reaches the router and never gets a method router's answer (405 with
   `Allow`) or the fallback's 404.

   A port may be omitted from Host or Origin only when the listener's port is 80. Names compare
   ignoring ASCII case; nothing else is normalized. Rejection bodies are
   `application/problem+json` without `code` and never repeat the rejected value.
4. A missing Origin passes and a present one must be the pinned origin, for every method and
   route. Origin never authorizes anything (design 5.7.1): login and the agent's key do. A
   cross-site WebSocket handshake always carries a foreign Origin, so row 5 refuses it before
   any upgrade; this is the WebSocket Origin validation of 13.3.
5. The redirect is 307 (a permanent redirect is cached by browsers and the method must be kept)
   with an absolute `Location`, so a path such as `//other.example/x` stays under the pinned
   authority. Row 3 does not look at Origin: nothing runs on the alias, and the redirected
   request is judged at the pinned origin.
6. Only HTTP/1 is served: with HTTP/2 the authority travels without a Host header, and the
   guard would need a second rule. Not enabling axum's `http2` feature is not enough, because
   another crate in the workspace build (tonic) turns hyper's HTTP/2 on and `axum::serve` would
   then accept cleartext HTTP/2. The server therefore drives hyper's HTTP/1 connection builder
   itself. A connection must deliver a complete request head within 5 seconds
   (`HEADER_READ_TIMEOUT`), so a client that sends nothing or one byte at a time cannot hold a
   connection or a graceful stop. `X-Forwarded-Host` is not read and no CORS header is sent.
7. The configuration is one TOML file named by the required `--config` argument. There is no
   default path, no search and no environment variable. Unknown keys are refused. The listener
   takes the pinned origin from the port it actually bound, so a test can configure port 0.
8. `GET /api/v1/health` answers without authentication, behind the same guard.
9. Shutdown stops accepting on SIGTERM or SIGINT (Ctrl-C or Ctrl-Break on Windows) and waits for
   requests in flight for a bounded grace period (`http.shutdown_grace_secs`, default 30, at
   most 300, a larger value is refused when the file is loaded), after which the process stops
   anyway. A second stop signal during the grace period ends the wait at once. The signal
   handlers are installed before the server logs that it listens.
10. A configuration error is reported with the parser's message and the line and column, never
    the source line, and a quoted value in the message is replaced: a later key may hold a
    password.

## Consequences

- Changing the port changes the pinned origin, which strands the browser's per-origin data
  (design 3.1: edits are saved to IndexedDB first). The documentation says so.
- A request through `localhost` costs one redirect; a WebSocket or an agent pointed at
  `localhost` fails on the 307 and must use the pinned origin. A script on a pinned-origin page
  must address the server by a relative URL or the pinned origin, since a request redirected
  from `localhost` carries a tainted Origin and ends in row 5.
- Row 6 lets through what a browser sends without Origin (cross-site navigations, no-cors GETs),
  so a GET route must never change state, and what a GET returns is protected by the session of
  the local-users work, not by this guard. CSRF protection (cookie attributes, fetch metadata)
  extends the guard with that work.
- WebAuthn does not accept an IP address as the relying party, so signing in with passkeys would
  need `localhost` as the pinned name; that is decided with the local users, before anything
  stores per-origin data.
- The server owns its connection loop, so it must also carry the WebSocket upgrades and close
  them at a graceful stop when the control channel lands.
- Loopback has no server authentication: another OS user that binds the port before the service
  starts answers as the server.
- The standard profile supplies its own ingress rule where the local profile applies this guard.

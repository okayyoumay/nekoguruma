# ADR-469: Local Users Log In With an Argon2id Password and Hold an In-Memory Cookie Session

**Date:** 2026-11-12
**Status:** Accepted
**Affects:** design 6.4, 13, 13.3; `db/migrations/0006_local_users.sql`, `db/README.md`; `api/openapi.yaml` (`/v1/login`, `/v1/logout`, `/v1/me`, the `ngr_session` scheme); `crates/server` (`lib`, `main`, `register`, the local-user and operator-session modules), `crates/server/docs/ngr-server.md`; answers the CSRF and passkey questions ADR-456's Consequences left to this work; departs from ADR-463 item 5's limiter rule for passwords

## Context

Design 13.3 makes login mandatory on the local profile, because other OS users on the same PC
reach the loopback port. Design 13 names local users, not OIDC, as the local profile's
authentication. The `operators` table holds an OIDC subject and no credential, and nothing seeds
a tenant: ADR-463's `registration-code` looks up an operator by subject in whatever tenant the
row is in. ADR-456 left two questions to this work: how CSRF protection extends the guard, and
whether passkeys, which need `localhost` as the pinned name, replace passwords.

The maintainer decided the credential, the session's storage and lifetime, the CSRF rule and the
first routes, and then the limiter, the tenant, the clocks, the Origin rule for login and logout,
the terminal rule and the accepted cookie risk below.

## Decision

1. **Passwords, not passkeys.** A local user logs in with a username and a password, so the
   pinned name stays `127.0.0.1` (ADR-456). The username is the operator's `subject`. The
   password is stored as an Argon2id PHC string (memory 19 MiB, two passes, one lane, a random
   salt) in a new table `operator_credentials` (one row per operator); the parameters live in the
   string, so a later rise needs no migration. A password that is set has at least 12 characters
   and at most 1024 bytes and no composition rule; login accepts 1 to 1024 bytes, so a later
   change of the minimum locks nobody out.
2. **One local tenant.** The server carries a fixed tenant ID. `ngr-server user add` inserts that
   tenant if it is absent; login and the other user commands look an operator up by that tenant
   and the subject, so two operators can never answer to one username. *Rejected:* ADR-463's
   rule of exactly one operator with the subject in any tenant, which makes a second tenant's
   row a failure instead of irrelevant.
3. **Users are managed by a command, not the Web API.** `ngr-server user add`, `set-password` and
   `disable` write the database directly, as `registration-code` does: whoever can read the
   server's configuration is the Administrator (6.8). Roles come only from a repeatable `--role`,
   so no user gets `admin` implicitly. The password is read from standard input, never from an
   argument, and a terminal on standard input is refused, because it would echo the password.
   One trailing line break is stripped and nothing else is trimmed; a byte order mark, a second
   line, a control character, invalid UTF-8 or a length outside item 1 is refused, and nothing
   is written.
4. **Login is judged in this order, and the first step that applies answers:** the request guard,
   the method (405), the Origin (item 9), the body (413, 408, 400 as registration reads it), the
   shape (exactly a username and a password, both strings; 400), the length bounds (400), the
   limiter (item 5), one database read, one hash verification, then the session. An unknown user,
   an operator without a credential, a disabled operator and a wrong password get one 401 body,
   byte for byte, and each costs one Argon2 verification (against a fixed dummy hash when there
   is no hash to check), so neither the answer nor its timing tells them apart. The reason is
   logged with the operator ID when there is one; the typed username, the password, the cookie,
   the token and the hashes are never logged. A stored hash that does not parse is 500, as a
   stored key that does not decode is (ADR-467). Hashes run one at a time on the blocking pool.
5. **The limiter admits before any work, and a right password does not pass it.** Ten admitted
   attempts a minute; when it is full, every attempt is 429 with `Retry-After`, the right password
   included, and no row is read and no hash run. Only a successful login gives its slot back.
   ADR-463 item 5 lets a valid registration code through a full limiter because 128 random bits
   cannot be guessed; a password can, and a limiter that a right password passes would answer it
   200 while wrong ones get 429, which makes guessing unbounded.
6. **The session is a cookie and a row in memory.** A successful login mints 32 random bytes,
   sends them as `ngr_session` (base64url without padding; `HttpOnly`, `SameSite=Strict`,
   `Path=/`, no `Max-Age`, no `Secure` since the profile is plain HTTP on loopback) and keeps only
   their SHA-256, with the operator, the tenant and the credential fingerprint (item 8). A token
   that the request presents is never adopted, and a live session it presents is ended. A
   restart ends every session, as it ends the agents' tokens (ADR-466).
7. **A session lives 8 hours from login and 30 minutes from its last use, by two clocks.** Each
   session records its login and its last use on the monotonic clock and on the wall clock, and
   it has expired when either clock says so (exactly 8 hours or 30 minutes counts as expired). The
   monotonic clock does not advance while the PC sleeps on some platforms (Linux among them), and
   design 6.4 asks for
   a logout after inactivity, which a night with the lid closed is; a wall clock that steps back
   extends nothing, one that steps forward ends sessions early. Only a request that passes the
   whole check refreshes the last use, to the later of its own arrival and the recorded one.
   Sixteen live sessions per operator at most; the next login drops the least recently used.
8. **Every authenticated request reads the operator's state.** The session layer sits on each
   operator route's method router (axum's `MethodRouter::route_layer`; the router-wide layer would
   judge a wrong method first) and judges in this order: the route and the method (404, 405), the Origin
   (item 9), the cookie, the session at one instant (expiry), one read of the operator's
   `disabled_at` and stored hash, the session again, then the handler. A disabled or deleted
   operator, or a credential whose SHA-256 differs from the one recorded at login, ends that
   session with 401: a disable or a password reset by the command takes effect at the next
   request. The fingerprint is taken from the row that login verified, and no timestamp is
   compared, so the server's monotonic clock never meets the database's. A database failure is
   503, with the session kept and not refreshed. *Rejected:* checking only at login, which would
   leave a disabled user up to 8 hours.
9. **Origin presence is the CSRF rule.** A request whose method is not GET or HEAD, to login,
   logout or any route behind the session layer, must carry an Origin equal to the pinned
   origin, with or without a session; a missing Origin is 403 before the cookie or the body is
   read. The guard already refuses a foreign one (ADR-456), browsers send Origin on such requests,
   and `SameSite=Strict` keeps the cookie off cross-site ones. A GET never changes state. This
   replaces the fetch-metadata idea in ADR-456's Consequences.
10. **The cookie is parsed leniently, and twice is refused.** The layer reads the `Cookie` headers
    as bytes and uses the one pair named exactly `ngr_session`, whatever other pairs the header
    holds (other programs on `127.0.0.1` share the cookie jar, since cookies ignore the port). A
    value that is not 43 base64url characters, or the name more than once, is a 401 that clears
    the cookie without a lookup: a second copy may be one that another port planted.
11. **Logout and whoami.** `POST /api/v1/logout` ends every well-formed session token it is
    shown and clears the cookie, answering 204 with no database access, so it works for a
    disabled operator and with the database down. `GET /api/v1/me` answers the operator's ID,
    username, display name and roles, read fresh.

## Consequences

- Cookies are not isolated by port, and `SameSite` does not see ports. Another OS user who listens
  on any loopback port and gets the operator's browser to open a page there receives
  `ngr_session`, and can replay it with a forged Origin from a non-browser client; the same
  listener can plant a cookie of its own. This is accepted for now, as ADR-456 accepts that
  whoever binds the port first answers as the server; the fix is a second secret bound to the
  pinned origin and sent as a header.
- A local caller can keep the limiter full and deny every login for a minute at a time.
- Sessions and the limiter are per process: a second server process on the same database
  neither sees the other's sessions nor shares its limit, as with the agents' tokens.
- A disable or reset takes effect at the next request, not inside one already running.
- Login and logout write no row; the audit log has no partition for them yet.
- A user interface that polls an authenticated route never goes idle; one that should log out
  after inactivity must stop polling itself.
- Enabling an operator again, changing its roles or name, and an operator changing their own
  password have no command or route yet.

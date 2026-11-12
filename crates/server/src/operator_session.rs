//! `POST /api/v1/login`, `POST /api/v1/logout`, `GET /api/v1/me` and the session layer every
//! operator route mounts (design 13.3; ADR-469). The users and their passwords are in
//! [`crate::local_user`]; the agent's own sessions are in [`crate::session`] and are separate.
//!
//! A login judges a request in this order, and the first step that applies answers: the request
//! guard (before routing), the method (405), the Origin (403: a missing one is refused here, a
//! foreign one by the guard), the body (413, 408, 400), the Content-Type and the shape (400),
//! the user name and password rules (400, no database access, no hash), the limiter (429, no
//! database access, no hash), the user's row (503, 500), then one verification: a refusal is
//! one 401 document whatever the reason, and the reason goes to the log only. A failed login
//! keeps its limiter slot; only a 200 gives it back.
//!
//! The session layer is mounted with `MethodRouter::route_layer` (the method router's, not the
//! router's: only that one leaves its 405 outside the layer), so an unknown path (404) and a
//! wrong method (405) are answered before it. It judges in this order: the Origin of a request
//! that is not GET or HEAD (403, session or not), the cookie, the session table at one instant,
//! the operator's row, the table again, then the handler. The row read is what ends a session
//! when its operator is disabled or its password is replaced: the session keeps the SHA-256 of
//! the stored password hash its login verified, and the layer compares it with the stored one.
//! No timestamps are compared.
//!
//! A session is a random 32-byte token in the cookie `ngr_session`; the table keeps its SHA-256,
//! in memory, so a restart logs everyone out. The table takes its instants as parameters and
//! the routes read them through [`session::now`] and the wall clock, so a test passes explicit
//! instants and a route test moves tokio's clock.
//!
//! Never logged: the password, a user name typed on a failed login, the cookie, the token, its
//! hash, the password hash and the body.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::Json;
use axum::extract::{Extension, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use shared_crypto::random_32;
use sqlx::PgPool;

use crate::db;
use crate::health::may_log;
use crate::local_guard::pinned_origin;
use crate::local_user::{self, Hasher, LOCAL_TENANT_ID, MAX_LOGIN_USERNAME, MAX_PASSWORD_BYTES};
use crate::problem::Problem;
use crate::register::{Admission, Limiter, bounded_body, is_json, strict_strings};
use crate::session::{self, too_many};

/// The login route's path.
pub const LOGIN_PATH: &str = "/api/v1/login";
/// The logout route's path.
pub const LOGOUT_PATH: &str = "/api/v1/logout";
/// The whoami route's path.
pub const ME_PATH: &str = "/api/v1/me";
/// The session cookie's name.
pub const COOKIE_NAME: &str = "ngr_session";
/// How long a session lives from its login; exactly this old is expired.
pub const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
/// How long a session may sit unused; exactly this idle is expired.
pub const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);
/// The most live sessions one operator holds; a login at the cap drops the least recently used.
pub const MAX_SESSIONS_PER_OPERATOR: usize = 16;
/// How many failed logins one router counts within [`LOGIN_WINDOW`].
pub const LOGIN_LIMIT: usize = 10;
/// The sliding window of the login limiter.
pub const LOGIN_WINDOW: Duration = Duration::from_secs(60);
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// A token's text: 32 bytes as base64url without padding.
const TOKEN_TEXT: usize = 43;
/// The scheme the 401 answers name. Our own, never Basic: a browser would prompt for Basic.
const CHALLENGE_SCHEME: &str = "Ngr-Session";

/// The clock of the routes: tokio's, as in [`session::now`].
fn wall() -> SystemTime {
    SystemTime::now()
}

// ---------------------------------------------------------------------------------------------
// The session table

/// Which limit a session passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Absolute,
    Idle,
}

#[derive(Clone)]
struct Entry {
    operator: String,
    tenant: String,
    fingerprint: [u8; 32],
    login: Instant,
    login_wall: SystemTime,
    last: Instant,
    last_wall: SystemTime,
}

/// The wall clock's elapsed time: a clock that steps backward changes nothing.
fn wall_elapsed(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or(Duration::ZERO)
}

impl Entry {
    /// Absolute expiry is judged first (either clock), then idleness (either clock).
    fn expiry(&self, now: Instant, wall: SystemTime) -> Option<Expiry> {
        if now.saturating_duration_since(self.login) >= ABSOLUTE_LIFETIME
            || wall_elapsed(wall, self.login_wall) >= ABSOLUTE_LIFETIME
        {
            return Some(Expiry::Absolute);
        }
        if now.saturating_duration_since(self.last) >= IDLE_LIMIT
            || wall_elapsed(wall, self.last_wall) >= IDLE_LIMIT
        {
            return Some(Expiry::Idle);
        }
        None
    }
}

/// What a session was made for: who it belongs to and the fingerprint of the password hash its
/// login verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub operator: String,
    pub tenant: String,
    pub fingerprint: [u8; 32],
}

/// What the table says about a presented token.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Live(Holder),
    /// Never issued, removed, or lost at a restart.
    Unknown,
    /// Past a limit; the session is removed.
    Expired(Expiry),
}

/// The live operator sessions, keyed by the SHA-256 of the token. The mutex is never held across
/// an await, and its poisoning is ignored, as in the agent's token table.
///
/// No constant-time comparison: the key is a digest of a 256-bit secret, so a timing difference
/// on the digest says nothing usable about the token.
#[derive(Clone, Default)]
pub struct OperatorSessions(Arc<Mutex<HashMap<[u8; 32], Entry>>>);

impl fmt::Debug for OperatorSessions {
    /// The count only: neither a token nor its hash is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.table().len();
        f.debug_struct("OperatorSessions")
            .field("count", &count)
            .finish()
    }
}

/// The table key of a token.
fn table_key(token: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(token).into()
}

/// The SHA-256 of a stored password hash: what a session remembers of it.
pub fn fingerprint(password_hash: &str) -> [u8; 32] {
    Sha256::digest(password_hash.as_bytes()).into()
}

impl OperatorSessions {
    pub fn new() -> OperatorSessions {
        OperatorSessions::default()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Looks `key` up at `now` and `wall`; an expired session is removed.
    pub fn look(&self, key: &[u8; 32], now: Instant, wall: SystemTime) -> Lookup {
        let mut table = self.table();
        let Some(entry) = table.get(key) else {
            return Lookup::Unknown;
        };
        if let Some(expiry) = entry.expiry(now, wall) {
            table.remove(key);
            return Lookup::Expired(expiry);
        }
        Lookup::Live(Holder {
            operator: entry.operator.clone(),
            tenant: entry.tenant.clone(),
            fingerprint: entry.fingerprint,
        })
    }

    /// Records a use: the last use becomes the later of its present value and `arrival`, on both
    /// clocks. False when the session is gone (a logout or an expiry removed it meanwhile): it is
    /// never put back.
    pub fn refresh(&self, key: &[u8; 32], arrival: Instant, arrival_wall: SystemTime) -> bool {
        let mut table = self.table();
        let Some(entry) = table.get_mut(key) else {
            return false;
        };
        entry.last = entry.last.max(arrival);
        entry.last_wall = entry.last_wall.max(arrival_wall);
        true
    }

    /// Removes a session and returns its operator's ID.
    pub fn remove(&self, key: &[u8; 32]) -> Option<String> {
        self.table().remove(key).map(|entry| entry.operator)
    }

    /// Makes a session at a login, in one critical section: the sessions `presented` are removed,
    /// the expired ones are purged, the operator's least recently used session goes when it
    /// holds [`MAX_SESSIONS_PER_OPERATOR`], and the new one is put in with both clocks' login and
    /// last use at `now` and `wall`.
    pub fn establish(
        &self,
        presented: &[[u8; 32]],
        key: [u8; 32],
        holder: Holder,
        now: Instant,
        wall: SystemTime,
    ) {
        let mut table = self.table();
        for old in presented {
            table.remove(old);
        }
        table.retain(|_, entry| entry.expiry(now, wall).is_none());
        let mut held: Vec<([u8; 32], Instant, Instant)> = table
            .iter()
            .filter(|(_, entry)| entry.operator == holder.operator)
            .map(|(key, entry)| (*key, entry.last, entry.login))
            .collect();
        if held.len() >= MAX_SESSIONS_PER_OPERATOR {
            held.sort_by_key(|(_, last, login)| (*last, *login));
            let drop = held.len() + 1 - MAX_SESSIONS_PER_OPERATOR;
            for (old, _, _) in held.iter().take(drop) {
                table.remove(old);
            }
        }
        table.insert(
            key,
            Entry {
                operator: holder.operator,
                tenant: holder.tenant,
                fingerprint: holder.fingerprint,
                login: now,
                login_wall: wall,
                last: now,
                last_wall: wall,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.table().len()
    }
}

// ---------------------------------------------------------------------------------------------
// The cookie

/// The values of every cookie pair named exactly `ngr_session`, in order. The header is parsed
/// as bytes: a malformed or non-ASCII pair of another name is skipped, never a reason to refuse
/// the header.
fn cookie_values(headers: &HeaderMap) -> Vec<&[u8]> {
    let mut values = Vec::new();
    for header in headers.get_all(header::COOKIE) {
        for pair in header.as_bytes().split(|b| *b == b';') {
            let pair = pair.trim_ascii();
            let Some(at) = pair.iter().position(|b| *b == b'=') else {
                continue;
            };
            if &pair[..at] == COOKIE_NAME.as_bytes() {
                values.push(&pair[at + 1..]);
            }
        }
    }
    values
}

/// The 32 bytes a cookie value stands for, or `None` unless it is exactly 43 characters of
/// canonical base64url without padding.
fn decode_token(value: &[u8]) -> Option<[u8; 32]> {
    if value.len() != TOKEN_TEXT {
        return None;
    }
    URL_SAFE_NO_PAD.decode(value).ok()?.try_into().ok()
}

fn set_cookie(token: &[u8; 32]) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
        URL_SAFE_NO_PAD.encode(token)
    ))
    .expect("base64url and fixed text are a valid header value")
}

fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_static("ngr_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict")
}

// ---------------------------------------------------------------------------------------------
// Answers

fn unauthorized(title: &'static str) -> Response {
    let mut response = Problem::new(StatusCode::UNAUTHORIZED, title).into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(CHALLENGE_SCHEME),
    );
    response
}

/// 401 for a session that cannot be used, with the cookie cleared.
fn unauthorized_cleared() -> Response {
    let mut response = unauthorized("Not signed in");
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cleared_cookie());
    response
}

fn ok_json(body: serde_json::Value) -> Response {
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn whoami(id: &str, username: &str, display_name: &str, roles: &[String]) -> serde_json::Value {
    serde_json::json!({
        "operatorId": id,
        "username": username,
        "displayName": display_name,
        "roles": roles,
    })
}

// ---------------------------------------------------------------------------------------------
// Log gates

const SLOT_LIMITED: usize = 0;
const SLOT_UNKNOWN_USER: usize = 1;
const SLOT_NO_CREDENTIAL: usize = 2;
const SLOT_DISABLED: usize = 3;
const SLOT_WRONG_PASSWORD: usize = 4;
const SLOT_UNAVAILABLE: usize = 5;
const SLOT_INTERNAL: usize = 6;
const SLOT_OPERATOR_GONE: usize = 7;
const SLOT_CREDENTIAL_CHANGED: usize = 8;
const SLOTS: usize = 9;

/// One log gate per reason, so that a flood of one cannot silence another.
struct Gates {
    slots: [Mutex<Option<Instant>>; SLOTS],
}

impl Gates {
    fn new() -> Gates {
        Gates {
            slots: std::array::from_fn(|_| Mutex::new(None)),
        }
    }

    fn allow(&self, slot: usize, now: Instant) -> bool {
        may_log(&self.slots[slot], now, LOG_EVERY)
    }
}

// ---------------------------------------------------------------------------------------------
// The state

/// The state of the routes and of the session layer: the pool, the pinned port, the session
/// table, the hasher, and the limiter and log gates of this router.
#[derive(Clone)]
pub struct OperatorAuth(Arc<Inner>);

struct Inner {
    pool: PgPool,
    port: u16,
    sessions: OperatorSessions,
    hasher: Hasher,
    failures: Limiter,
    gates: Gates,
}

impl fmt::Debug for OperatorAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorAuth")
            .field("sessions", &self.0.sessions)
            .finish_non_exhaustive()
    }
}

/// The operator a request is judged to be, put in the request's extensions by the session layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOperator {
    pub operator_id: String,
    pub tenant_id: String,
}

impl OperatorAuth {
    /// `port` is the listener's bound port: the pinned origin is derived from it, never from a
    /// request.
    pub fn new(pool: PgPool, port: u16, sessions: OperatorSessions, hasher: Hasher) -> Self {
        OperatorAuth(Arc::new(Inner {
            pool,
            port,
            sessions,
            hasher,
            failures: Limiter::new(LOGIN_LIMIT, LOGIN_WINDOW),
            gates: Gates::new(),
        }))
    }
}

impl Inner {
    /// 403 unless the request carries exactly one Origin, equal to the pinned origin. The guard
    /// refuses a foreign one before routing; a missing one it lets pass, and this refuses it.
    fn origin_refusal(&self, headers: &HeaderMap) -> Option<Response> {
        let pinned = pinned_origin(self.port);
        let mut origins = headers.get_all(header::ORIGIN).iter();
        match (origins.next(), origins.next()) {
            (Some(origin), None) if origin.as_bytes().eq_ignore_ascii_case(pinned.as_bytes()) => {
                None
            }
            _ => Some(Problem::new(StatusCode::FORBIDDEN, "Origin required").into_response()),
        }
    }

    fn failure(&self, error: &sqlx::Error, what: &str, now: Instant) -> Response {
        match db::failure(error) {
            db::Failure::Unavailable(cause) => {
                if self.gates.allow(SLOT_UNAVAILABLE, now) {
                    tracing::warn!(error = %cause, "{what} unavailable");
                }
                Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Sign-in unavailable").into_response()
            }
            db::Failure::Internal(cause) => {
                if self.gates.allow(SLOT_INTERNAL, now) {
                    tracing::error!(error = %cause, "{what} failed");
                }
                Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed").into_response()
            }
        }
    }

    fn internal(&self, operator_id: Option<&str>, what: &str, now: Instant) -> Response {
        if self.gates.allow(SLOT_INTERNAL, now) {
            tracing::error!(operator_id = operator_id.unwrap_or(""), error = %what, "login failed");
        }
        Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed").into_response()
    }

    /// The one 401 of every login refusal: the same document whatever the reason.
    fn refused() -> Response {
        unauthorized("Invalid user name or password")
    }
}

// ---------------------------------------------------------------------------------------------
// Login

/// The user's row as login reads it: the operator, whether it is disabled, the stored password
/// hash when there is one, and the roles.
/// The columns of the login read, in the order of its `SELECT`.
type AccountRow = (String, String, String, bool, Option<String>, Vec<String>);

struct Account {
    operator_id: String,
    tenant_id: String,
    display_name: String,
    disabled: bool,
    password_hash: Option<String>,
    roles: Vec<String>,
}

impl Inner {
    async fn read_account(&self, username: &str) -> Result<Option<Account>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        db::statement_timeouts(&mut tx).await?;
        let row: Option<AccountRow> = sqlx::query_as(
            "SELECT o.id::text, o.tenant_id::text, o.display_name, o.disabled_at IS NOT NULL, \
                        c.password_hash, \
                        COALESCE((SELECT array_agg(r.role ORDER BY r.role) \
                                  FROM operator_roles r WHERE r.operator_id = o.id), \
                                 ARRAY[]::text[]) \
                 FROM operators o LEFT JOIN operator_credentials c ON c.operator_id = o.id \
                 WHERE o.tenant_id = $1::uuid AND o.subject = $2",
        )
        .bind(LOCAL_TENANT_ID)
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(row.map(
            |(operator_id, tenant_id, display_name, disabled, password_hash, roles)| Account {
                operator_id,
                tenant_id,
                display_name,
                disabled,
                password_hash,
                roles,
            },
        ))
    }
}

/// `POST /api/v1/login`: 200 with the whoami document and the session cookie.
pub async fn login(State(auth): State<OperatorAuth>, request: Request) -> Response {
    let inner = &*auth.0;
    let (parts, body) = request.into_parts();
    if let Some(refusal) = inner.origin_refusal(&parts.headers) {
        return refusal;
    }
    let bytes = match bounded_body(body).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    if !is_json(&parts.headers) {
        return Problem::new(
            StatusCode::BAD_REQUEST,
            "Content-Type must be application/json",
        )
        .into_response();
    }
    let Some([username, password]) = strict_strings(&bytes, ["username", "password"]) else {
        return Problem::new(
            StatusCode::BAD_REQUEST,
            "The body is not the expected JSON object",
        )
        .into_response();
    };
    if username.is_empty()
        || username.len() > MAX_LOGIN_USERNAME
        || username.chars().any(char::is_control)
    {
        return Problem::new(StatusCode::BAD_REQUEST, "Invalid user name").into_response();
    }
    if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
        return Problem::new(StatusCode::BAD_REQUEST, "Invalid password").into_response();
    }

    // Admission is one critical section before any await that touches the database or the
    // hasher, so eleven concurrent failures on an empty limiter are ten 401s and one 429. The
    // slot is kept by every outcome but a 200.
    let admitted = session::now();
    if let Admission::Limited { retry_after_secs } = inner.failures.admit(admitted) {
        if inner.gates.allow(SLOT_LIMITED, admitted) {
            tracing::warn!(retry_after_secs, "login limited");
        }
        return too_many(retry_after_secs, "Too many failed sign-ins");
    }

    let account = match inner.read_account(&username).await {
        Ok(account) => account,
        Err(error) => return inner.failure(&error, "login", session::now()),
    };
    // A refusal costs one verification whatever its reason: a real one for a wrong password,
    // the dummy for an account with nothing to check.
    let Some(account) = account else {
        inner.hasher.verify_dummy(password).await;
        if inner.gates.allow(SLOT_UNKNOWN_USER, session::now()) {
            tracing::warn!(reason = "unknown_user", "login refused");
        }
        return Inner::refused();
    };
    let Some(stored) = account.password_hash.clone() else {
        inner.hasher.verify_dummy(password).await;
        if inner.gates.allow(SLOT_NO_CREDENTIAL, session::now()) {
            tracing::warn!(reason = "no_credential", operator_id = %account.operator_id, "login refused");
        }
        return Inner::refused();
    };
    if account.disabled {
        inner.hasher.verify_dummy(password).await;
        if inner.gates.allow(SLOT_DISABLED, session::now()) {
            tracing::warn!(reason = "disabled", operator_id = %account.operator_id, "login refused");
        }
        return Inner::refused();
    }
    // The fingerprint is taken from the row this verification judges and is never read again.
    let fingerprint = fingerprint(&stored);
    let verdict = match inner.hasher.verify(stored, password).await {
        Ok(verdict) => verdict,
        Err(_) => {
            return inner.internal(
                Some(&account.operator_id),
                "the hashing task failed",
                session::now(),
            );
        }
    };
    match verdict {
        local_user::Verdict::Match => {}
        local_user::Verdict::Mismatch => {
            if inner.gates.allow(SLOT_WRONG_PASSWORD, session::now()) {
                tracing::warn!(reason = "wrong_password", operator_id = %account.operator_id, "login refused");
            }
            return Inner::refused();
        }
        local_user::Verdict::Malformed => {
            return inner.internal(
                Some(&account.operator_id),
                "the stored password hash is not an Argon2id PHC string",
                session::now(),
            );
        }
    }

    let Ok(token) = random_32() else {
        return inner.internal(
            Some(&account.operator_id),
            "the random source failed",
            session::now(),
        );
    };
    // A presented cookie's session is replaced, never adopted. This happens only here, on
    // success: any failure above left the cookie and its session as they were.
    let presented: Vec<[u8; 32]> = cookie_values(&parts.headers)
        .into_iter()
        .filter_map(decode_token)
        .map(|token| table_key(&token))
        .collect();
    // The instants are read after the hash: the session's lifetime starts when it is made.
    let (now, wall) = (session::now(), wall());
    inner.sessions.establish(
        &presented,
        table_key(&token),
        Holder {
            operator: account.operator_id.clone(),
            tenant: account.tenant_id.clone(),
            fingerprint,
        },
        now,
        wall,
    );
    inner.failures.release(admitted);
    tracing::info!(operator_id = %account.operator_id, "login");
    let mut response = ok_json(whoami(
        &account.operator_id,
        &username,
        &account.display_name,
        &account.roles,
    ));
    response
        .headers_mut()
        .insert(header::SET_COOKIE, set_cookie(&token));
    response
}

// ---------------------------------------------------------------------------------------------
// Logout

/// `POST /api/v1/logout`: 204, with the cookie cleared, whether or not there was a session.
/// Every well-formed token presented is removed. No database access.
pub async fn logout(State(auth): State<OperatorAuth>, request: Request) -> Response {
    let inner = &*auth.0;
    let (parts, body) = request.into_parts();
    if let Some(refusal) = inner.origin_refusal(&parts.headers) {
        return refusal;
    }
    match bounded_body(body).await {
        Ok(bytes) if bytes.is_empty() => {}
        Ok(_) => {
            return Problem::new(StatusCode::BAD_REQUEST, "The body must be empty").into_response();
        }
        Err(response) => return response,
    }
    for value in cookie_values(&parts.headers) {
        let Some(token) = decode_token(value) else {
            continue;
        };
        if let Some(operator_id) = inner.sessions.remove(&table_key(&token)) {
            tracing::info!(%operator_id, "logout");
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::SET_COOKIE, cleared_cookie());
    response
}

// ---------------------------------------------------------------------------------------------
// The session layer

impl Inner {
    /// The operator's state for the layer: whether it is disabled and its stored password hash.
    /// `None` when the operator row is gone. One read in a transaction that is dropped.
    async fn read_standing(
        &self,
        operator_id: &str,
    ) -> Result<Option<(bool, Option<String>)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        db::statement_timeouts(&mut tx).await?;
        sqlx::query_as(
            "SELECT o.disabled_at IS NOT NULL, c.password_hash \
             FROM operators o LEFT JOIN operator_credentials c ON c.operator_id = o.id \
             WHERE o.id = $1::uuid",
        )
        .bind(operator_id)
        .fetch_optional(&mut *tx)
        .await
    }
}

/// The session layer: mount it with `MethodRouter::route_layer` on every operator route, so that
/// 404 and 405 come first. A request that passes carries a [`SessionOperator`] in its extensions.
pub async fn require_session(
    State(auth): State<OperatorAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let inner = &*auth.0;
    if !matches!(*request.method(), Method::GET | Method::HEAD)
        && let Some(refusal) = inner.origin_refusal(request.headers())
    {
        return refusal;
    }
    // The cookie: no pair is a plain 401, a repeated pair is refused without a lookup.
    let values = cookie_values(request.headers());
    let value = match values.as_slice() {
        [] => return unauthorized("Not signed in"),
        [one] => *one,
        _ => return unauthorized_cleared(),
    };
    let Some(token) = decode_token(value) else {
        return unauthorized_cleared();
    };
    let key = table_key(&token);

    // One instant for the table and for the arrival that refreshes it.
    let (now, wall) = (session::now(), wall());
    let holder = match inner.sessions.look(&key, now, wall) {
        Lookup::Live(holder) => holder,
        Lookup::Unknown | Lookup::Expired(_) => return unauthorized_cleared(),
    };

    let standing = match inner.read_standing(&holder.operator).await {
        Ok(standing) => standing,
        // The session is kept and not refreshed.
        Err(error) => return inner.failure(&error, "session check", session::now()),
    };
    match standing {
        Some((false, Some(stored))) if fingerprint(&stored) == holder.fingerprint => {}
        Some((false, Some(_))) => {
            inner.sessions.remove(&key);
            if inner.gates.allow(SLOT_CREDENTIAL_CHANGED, now) {
                tracing::warn!(reason = "credential_changed", operator_id = %holder.operator, "session ended");
            }
            return unauthorized_cleared();
        }
        Some((false, None)) => {
            inner.sessions.remove(&key);
            if inner.gates.allow(SLOT_CREDENTIAL_CHANGED, now) {
                tracing::warn!(reason = "no_credential", operator_id = %holder.operator, "session ended");
            }
            return unauthorized_cleared();
        }
        Some((true, _)) | None => {
            inner.sessions.remove(&key);
            if inner.gates.allow(SLOT_OPERATOR_GONE, now) {
                tracing::warn!(reason = "operator_unusable", operator_id = %holder.operator, "session ended");
            }
            return unauthorized_cleared();
        }
    }
    // The table again: a session removed while the row was read (a logout) is not put back.
    if !inner.sessions.refresh(&key, now, wall) {
        return unauthorized_cleared();
    }
    request.extensions_mut().insert(SessionOperator {
        operator_id: holder.operator,
        tenant_id: holder.tenant,
    });
    next.run(request).await
}

/// `GET /api/v1/me`: the operator's whoami document, read fresh.
pub async fn me(
    State(auth): State<OperatorAuth>,
    Extension(operator): Extension<SessionOperator>,
) -> Response {
    let inner = &*auth.0;
    let row: Result<Option<(String, String, Vec<String>)>, sqlx::Error> = async {
        let mut tx = inner.pool.begin().await?;
        db::statement_timeouts(&mut tx).await?;
        sqlx::query_as(
            "SELECT o.subject, o.display_name, \
                    COALESCE((SELECT array_agg(r.role ORDER BY r.role) \
                              FROM operator_roles r WHERE r.operator_id = o.id), \
                             ARRAY[]::text[]) \
             FROM operators o WHERE o.id = $1::uuid",
        )
        .bind(&operator.operator_id)
        .fetch_optional(&mut *tx)
        .await
    }
    .await;
    match row {
        Ok(Some((username, display_name, roles))) => ok_json(whoami(
            &operator.operator_id,
            &username,
            &display_name,
            &roles,
        )),
        Ok(None) => unauthorized_cleared(),
        Err(error) => inner.failure(&error, "whoami", session::now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn holder(operator: &str) -> Holder {
        Holder {
            operator: operator.to_owned(),
            tenant: "t".to_owned(),
            fingerprint: [7; 32],
        }
    }

    fn key(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// A table with one session of `operator` made at `(t0, w0)`.
    fn one(t0: Instant, w0: SystemTime) -> OperatorSessions {
        let table = OperatorSessions::new();
        table.establish(&[], key(1), holder("op"), t0, w0);
        table
    }

    const NANO: Duration = Duration::from_nanos(1);

    // ----- expiry

    #[test]
    fn the_absolute_limit_is_eight_hours_and_exactly_eight_is_expired() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        // Keep the session busy so that only the absolute limit can end it: refresh just before.
        let table = one(t0, w0);
        let near = ABSOLUTE_LIFETIME - NANO;
        assert!(table.refresh(&key(1), t0 + near - secs(60), w0 + near - secs(60)));
        assert!(matches!(
            table.look(&key(1), t0 + near, w0 + near),
            Lookup::Live(_)
        ));
        assert_eq!(
            table.look(&key(1), t0 + ABSOLUTE_LIFETIME, w0),
            Lookup::Expired(Expiry::Absolute)
        );
        assert_eq!(table.len(), 0, "an expired session is removed");
        assert_eq!(table.look(&key(1), t0, w0), Lookup::Unknown);
    }

    #[test]
    fn the_idle_limit_is_thirty_minutes_and_exactly_thirty_is_expired() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        assert!(matches!(
            table.look(&key(1), t0 + IDLE_LIMIT - NANO, w0),
            Lookup::Live(_)
        ));
        assert_eq!(
            table.look(&key(1), t0 + IDLE_LIMIT, w0),
            Lookup::Expired(Expiry::Idle)
        );
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn either_clock_alone_expires_a_session() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        // The wall clock steps forward; the monotonic clock does not.
        let table = one(t0, w0);
        assert_eq!(
            table.look(&key(1), t0, w0 + IDLE_LIMIT),
            Lookup::Expired(Expiry::Idle)
        );
        let table = one(t0, w0);
        assert!(matches!(
            table.look(&key(1), t0, w0 + IDLE_LIMIT - NANO),
            Lookup::Live(_)
        ));
        assert_eq!(
            table.look(&key(1), t0, w0 + ABSOLUTE_LIFETIME),
            Lookup::Expired(Expiry::Absolute)
        );
        // The monotonic clock alone, as the paused clock of a route test moves it.
        let table = one(t0, w0);
        assert_eq!(
            table.look(&key(1), t0 + ABSOLUTE_LIFETIME, w0),
            Lookup::Expired(Expiry::Absolute)
        );
    }

    #[test]
    fn a_wall_clock_that_steps_backward_changes_nothing() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        let back = w0 - secs(24 * 3600);
        assert!(matches!(
            table.look(&key(1), t0 + secs(1), back),
            Lookup::Live(_)
        ));
        // Nor does it shorten the monotonic limits.
        assert_eq!(
            table.look(&key(1), t0 + IDLE_LIMIT, back),
            Lookup::Expired(Expiry::Idle)
        );
    }

    #[test]
    fn the_absolute_limit_is_judged_before_idleness() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        // Past both: the answer is the absolute one.
        assert_eq!(
            table.look(&key(1), t0 + ABSOLUTE_LIFETIME + secs(1), w0),
            Lookup::Expired(Expiry::Absolute)
        );
    }

    // ----- refresh

    #[test]
    fn a_use_moves_the_idle_limit_and_never_earlier_and_never_back_in() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        let used = secs(20 * 60);
        assert!(table.refresh(&key(1), t0 + used, w0 + used));
        // A refresh with an earlier arrival (a slower concurrent request) keeps the later one.
        assert!(table.refresh(&key(1), t0 + secs(60), w0 + secs(60)));
        assert!(matches!(
            table.look(
                &key(1),
                t0 + used + IDLE_LIMIT - NANO,
                w0 + used + IDLE_LIMIT - NANO
            ),
            Lookup::Live(_)
        ));
        assert!(matches!(
            table.look(&key(1), t0 + used + IDLE_LIMIT, w0),
            Lookup::Expired(Expiry::Idle)
        ));
        // A removed or expired session is not put back by a refresh.
        assert!(!table.refresh(&key(1), t0, w0));
        assert_eq!(table.len(), 0);
        let table = one(t0, w0);
        assert_eq!(table.remove(&key(1)).as_deref(), Some("op"));
        assert!(!table.refresh(&key(1), t0, w0));
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn a_lookup_alone_does_not_refresh() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        for minute in [10, 20, 29] {
            let at = secs(minute * 60);
            assert!(matches!(
                table.look(&key(1), t0 + at, w0 + at),
                Lookup::Live(_)
            ));
        }
        assert_eq!(
            table.look(&key(1), t0 + IDLE_LIMIT, w0 + IDLE_LIMIT),
            Lookup::Expired(Expiry::Idle)
        );
    }

    // ----- establishing

    #[test]
    fn a_login_purges_the_expired_sessions_of_everyone() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = OperatorSessions::new();
        table.establish(&[], key(1), holder("a"), t0, w0);
        table.establish(&[], key(2), holder("b"), t0 + secs(1000), w0 + secs(1000));
        let later = IDLE_LIMIT + secs(1);
        table.establish(&[], key(3), holder("c"), t0 + later, w0 + later);
        // Session 1 was idle for 30 min and 1 s: gone. Session 2 had 1000 s of the 1800.
        assert_eq!(table.look(&key(1), t0, w0), Lookup::Unknown);
        assert!(matches!(
            table.look(&key(2), t0 + later, w0 + later),
            Lookup::Live(_)
        ));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn presented_sessions_are_removed_and_the_new_one_is_a_new_token() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = OperatorSessions::new();
        table.establish(&[], key(1), holder("op"), t0, w0);
        table.establish(&[], key(2), holder("op"), t0, w0);
        table.establish(&[key(1), key(2), key(9)], key(3), holder("op"), t0, w0);
        assert_eq!(table.look(&key(1), t0, w0), Lookup::Unknown);
        assert_eq!(table.look(&key(2), t0, w0), Lookup::Unknown);
        assert!(matches!(table.look(&key(3), t0, w0), Lookup::Live(_)));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn at_sixteen_the_least_recently_used_of_that_operator_is_dropped() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = OperatorSessions::new();
        table.establish(&[], key(200), holder("other"), t0, w0);
        for n in 0..16u8 {
            let at = secs(u64::from(n));
            table.establish(&[], key(n), holder("op"), t0 + at, w0 + at);
        }
        assert_eq!(table.len(), 17);
        // Session 0 is used last, so session 1 is now the least recently used.
        assert!(table.refresh(&key(0), t0 + secs(100), w0 + secs(100)));
        table.establish(&[], key(50), holder("op"), t0 + secs(101), w0 + secs(101));
        let at = t0 + secs(101);
        let wall = w0 + secs(101);
        assert_eq!(table.look(&key(1), at, wall), Lookup::Unknown);
        for n in [0u8, 2, 15, 50, 200] {
            assert!(
                matches!(table.look(&key(n), at, wall), Lookup::Live(_)),
                "{n}"
            );
        }
        assert_eq!(table.len(), 17, "the other operator's session is untouched");
    }

    #[test]
    fn a_replaced_session_frees_its_place_before_the_cap_counts() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = OperatorSessions::new();
        for n in 0..16u8 {
            let at = secs(u64::from(n));
            table.establish(&[], key(n), holder("op"), t0 + at, w0 + at);
        }
        // Presenting session 5 removes it, so nobody else is dropped.
        table.establish(
            &[key(5)],
            key(60),
            holder("op"),
            t0 + secs(20),
            w0 + secs(20),
        );
        assert_eq!(table.len(), 16);
        assert!(matches!(
            table.look(&key(0), t0 + secs(20), w0 + secs(20)),
            Lookup::Live(_)
        ));
    }

    #[test]
    fn debug_shows_the_count_and_nothing_else() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let table = one(t0, w0);
        let text = format!("{table:?}");
        assert_eq!(text, "OperatorSessions { count: 1 }");
    }

    // ----- the cookie

    fn headers(values: &[&[u8]]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(header::COOKIE, HeaderValue::from_bytes(value).unwrap());
        }
        map
    }

    const GOOD: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

    #[test]
    fn the_session_pair_is_found_among_foreign_and_malformed_ones() {
        let h = headers(&[format!("a=b; {COOKIE_NAME}={GOOD}; c=d").as_bytes()]);
        assert_eq!(cookie_values(&h), vec![GOOD.as_bytes()]);
        // Malformed and non-ASCII foreign pairs, spacing and tabs.
        let mut raw = b"junk; =x; \xff\xfe=\x80; ".to_vec();
        raw.extend_from_slice(format!("\t{COOKIE_NAME}={GOOD}\t; tail").as_bytes());
        assert_eq!(cookie_values(&headers(&[&raw])), vec![GOOD.as_bytes()]);
        // The name is exact: case, prefix and suffix do not match.
        for name in [
            "NGR_SESSION",
            "xngr_session",
            "ngr_sessionx",
            "ngr_session ",
        ] {
            let h = headers(&[format!("{name}={GOOD}").as_bytes()]);
            // A trailing space is trimmed before the '=' is found only at the pair's ends, so
            // "name =" is a different name.
            assert!(cookie_values(&h).is_empty(), "{name:?}");
        }
        assert!(cookie_values(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn a_repeated_pair_is_seen_in_one_header_and_across_headers() {
        let one = headers(&[format!("{COOKIE_NAME}={GOOD}; {COOKIE_NAME}={GOOD}").as_bytes()]);
        assert_eq!(cookie_values(&one).len(), 2);
        let two = headers(&[
            format!("{COOKIE_NAME}={GOOD}").as_bytes(),
            format!("x=y; {COOKIE_NAME}=").as_bytes(),
        ]);
        assert_eq!(cookie_values(&two).len(), 2);
    }

    #[test]
    fn a_token_is_43_canonical_base64url_characters() {
        let token = decode_token(GOOD.as_bytes()).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(token), GOOD);
        for bad in [
            "".to_owned(),
            GOOD[1..].to_owned(),
            format!("{GOOD}A"),
            format!("{GOOD}="),
            GOOD.replace('A', "+"),
            GOOD.replace('B', "/"),
            // Non-zero unused bits in the last character.
            format!("{}B", &GOOD[..42]),
            GOOD.replace('C', " "),
        ] {
            assert!(decode_token(bad.as_bytes()).is_none(), "{bad:?}");
        }
        assert!(decode_token(&[0xff; 43]).is_none());
    }

    #[test]
    fn the_cookies_have_the_fixed_attributes() {
        let text = set_cookie(&[0; 32]);
        let text = text.to_str().unwrap();
        assert_eq!(
            text,
            format!(
                "ngr_session={}; HttpOnly; SameSite=Strict; Path=/",
                "A".repeat(43)
            )
        );
        assert!(!text.contains("Max-Age") && !text.contains("Secure"));
        assert_eq!(
            cleared_cookie().to_str().unwrap(),
            "ngr_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"
        );
    }

    #[test]
    fn the_fingerprint_is_the_sha256_of_the_hash_text() {
        assert_ne!(fingerprint("a"), fingerprint("b"));
        assert_eq!(fingerprint("a"), <[u8; 32]>::from(Sha256::digest(b"a")));
    }

    // ----- the layer, over a pool that points at nothing

    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use sqlx::postgres::{PgConnectOptions, PgSslMode};
    use tower::Service;

    const PORT: u16 = 4321;

    fn dead_auth() -> OperatorAuth {
        let options = PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(1)
            .ssl_mode(PgSslMode::Disable);
        OperatorAuth::new(
            crate::db::pool_lazy(options, Duration::from_secs(1)),
            PORT,
            OperatorSessions::new(),
            Hasher::new(),
        )
    }

    /// A route that takes GET and POST behind the layer, which `/me` alone cannot show.
    fn probe(auth: &OperatorAuth) -> Router {
        Router::new().route(
            "/probe",
            get(|| async { "probe" })
                .post(|| async { "probe" })
                .route_layer(axum::middleware::from_fn_with_state(
                    auth.clone(),
                    require_session,
                )),
        )
    }

    /// A session of `n`, made now; the cookie that presents it.
    fn live(auth: &OperatorAuth, n: u8) -> String {
        auth.0.sessions.establish(
            &[],
            table_key(&[n; 32]),
            holder("op"),
            session::now(),
            wall(),
        );
        format!("{COOKIE_NAME}={}", URL_SAFE_NO_PAD.encode([n; 32]))
    }

    async fn send(
        router: &Router,
        method: &str,
        path: &str,
        origin: Option<&str>,
        cookies: &[&[u8]],
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        for cookie in cookies {
            builder = builder.header(header::COOKIE, HeaderValue::from_bytes(cookie).unwrap());
        }
        let mut router = router.clone();
        router
            .call(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn pinned() -> String {
        pinned_origin(PORT)
    }

    fn set_cookie_of(response: &Response) -> Option<&str> {
        response
            .headers()
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap())
    }

    const CLEARED: &str = "ngr_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict";

    #[tokio::test(start_paused = true)]
    async fn the_layer_runs_after_routing_so_404_and_405_come_first() {
        let auth = dead_auth();
        let router = probe(&auth);
        // No Origin, no cookie: an unknown path and a wrong method are still 404 and 405.
        let r = send(&router, "GET", "/nowhere", None, &[]).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = send(&router, "DELETE", "/probe", None, &[]).await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(r.headers().get(header::ALLOW).is_some());
        assert!(set_cookie_of(&r).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_is_not_get_or_head_needs_the_pinned_origin_before_anything_else() {
        let auth = dead_auth();
        let router = probe(&auth);
        let cookie = live(&auth, 1);
        let wrong = "http://127.0.0.1:9";
        let two = [&pinned()[..], &pinned()[..]];
        for (origin, what) in [(None, "none"), (Some(wrong), "foreign")] {
            // With a live session, with none, with a malformed cookie: always 403, no Set-Cookie.
            for cookies in [&[cookie.as_bytes()][..], &[], &[b"ngr_session=x"]] {
                let r = send(&router, "POST", "/probe", origin, cookies).await;
                assert_eq!(r.status(), StatusCode::FORBIDDEN, "{what}");
                assert_eq!(
                    r.headers().get(header::CONTENT_TYPE).unwrap(),
                    "application/problem+json"
                );
                assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
                assert!(set_cookie_of(&r).is_none(), "{what}");
            }
        }
        // Two Origin headers are not one pinned origin.
        let mut request = Request::builder().method("POST").uri("/probe");
        for origin in two {
            request = request.header(header::ORIGIN, origin);
        }
        let mut svc = router.clone();
        let r = svc
            .call(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // GET and HEAD need none: they go on to the cookie (401 for no session).
        for method in ["GET", "HEAD"] {
            let r = send(&router, method, "/probe", None, &[]).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{method}");
        }
        // With the pinned origin the session is judged: the row read fails on this pool (503).
        let r = send(
            &router,
            "POST",
            "/probe",
            Some(&pinned()),
            &[cookie.as_bytes()],
        )
        .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(auth.0.sessions.len(), 1, "nothing removed a session");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_session_pair_the_answer_is_401_and_sets_no_cookie() {
        let auth = dead_auth();
        let router = probe(&auth);
        let cases: [&[&[u8]]; 4] = [
            &[],
            &[b"theme=dark"],
            &[b"NGR_SESSION=abc; xngr_session=abc"],
            &[b"\xff\xfe=\x80; broken"],
        ];
        for cookies in cases {
            let r = send(&router, "GET", "/probe", None, cookies).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
            assert!(set_cookie_of(&r).is_none());
            assert_eq!(
                r.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/problem+json"
            );
            assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
            let challenge = r.headers().get(header::WWW_AUTHENTICATE).unwrap();
            assert!(!challenge.to_str().unwrap().starts_with("Basic"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_malformed_unknown_or_repeated_token_is_401_and_the_cookie_is_cleared() {
        let auth = dead_auth();
        let router = probe(&auth);
        let good = live(&auth, 1);
        let unknown = format!("{COOKIE_NAME}={}", URL_SAFE_NO_PAD.encode([9u8; 32]));
        for cookie in [
            format!("{COOKIE_NAME}=short"),
            format!("{COOKIE_NAME}="),
            format!("{COOKIE_NAME}={}=", URL_SAFE_NO_PAD.encode([1u8; 32])),
            unknown,
        ] {
            let r = send(&router, "GET", "/probe", None, &[cookie.as_bytes()]).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{cookie}");
            assert_eq!(set_cookie_of(&r), Some(CLEARED), "{cookie}");
        }
        // Repeated: in one header and across headers, even when each is the live token.
        let twice = format!("{good}; {good}");
        for cookies in [
            vec![twice.as_bytes()],
            vec![good.as_bytes(), good.as_bytes()],
        ] {
            let r = send(&router, "GET", "/probe", None, &cookies).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(set_cookie_of(&r), Some(CLEARED));
        }
        // No lookup happened: an expired session is still in the table, which a lookup would
        // have removed.
        tokio::time::advance(IDLE_LIMIT + secs(1)).await;
        let r = send(&router, "GET", "/probe", None, &[twice.as_bytes()]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(auth.0.sessions.len(), 1);
        let r = send(&router, "GET", "/probe", None, &[good.as_bytes()]).await;
        assert_eq!(set_cookie_of(&r), Some(CLEARED));
        assert_eq!(auth.0.sessions.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_good_token_is_used_beside_malformed_and_non_ascii_foreign_pairs() {
        let auth = dead_auth();
        let router = probe(&auth);
        let good = live(&auth, 1);
        let mut raw = b"junk; \xff\xfe=\x80; =; ".to_vec();
        raw.extend_from_slice(good.as_bytes());
        raw.extend_from_slice(b"; tail=\xc3\x28");
        // The session passes the cookie and the table and reaches the row read, which fails on
        // this pool: 503, not 401.
        let r = send(&router, "GET", "/probe", None, &[&raw]).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_row_read_keeps_the_session_and_does_not_refresh_it() {
        let auth = dead_auth();
        let router = probe(&auth);
        let cookie = live(&auth, 1);
        tokio::time::advance(secs(29 * 60)).await;
        let r = send(&router, "GET", "/probe", None, &[cookie.as_bytes()]).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        assert!(set_cookie_of(&r).is_none());
        assert_eq!(auth.0.sessions.len(), 1);
        // 58 minutes after the login and 29 after the failed request: had that refreshed the
        // session it would still be live.
        tokio::time::advance(secs(29 * 60)).await;
        let r = send(&router, "GET", "/probe", None, &[cookie.as_bytes()]).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(set_cookie_of(&r), Some(CLEARED));
        assert_eq!(auth.0.sessions.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_session_is_401_cleared_and_removed_at_each_limit() {
        for (wait, what) in [(IDLE_LIMIT, "idle"), (ABSOLUTE_LIFETIME, "absolute")] {
            let auth = dead_auth();
            let router = probe(&auth);
            let cookie = live(&auth, 1);
            if wait == ABSOLUTE_LIFETIME {
                // Kept busy, so that only the absolute limit can end it.
                tokio::time::advance(wait - secs(60)).await;
                let key = table_key(&[1; 32]);
                assert!(auth.0.sessions.refresh(&key, session::now(), wall()));
                tokio::time::advance(secs(60) - NANO).await;
            } else {
                tokio::time::advance(wait - NANO).await;
            }
            // One nanosecond before the limit the session is judged on (503: the row read).
            let r = send(&router, "GET", "/probe", None, &[cookie.as_bytes()]).await;
            assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{what}");
            tokio::time::advance(NANO).await;
            let r = send(&router, "GET", "/probe", None, &[cookie.as_bytes()]).await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{what}");
            assert_eq!(set_cookie_of(&r), Some(CLEARED), "{what}");
            assert_eq!(auth.0.sessions.len(), 0, "{what}");
        }
    }
}

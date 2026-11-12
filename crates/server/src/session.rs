//! `POST /api/v1/agent/challenge` and `POST /api/v1/agent/session`: the server's side of the
//! agent's connection proof (design 6.4, 14.4, 16.3; ADR-466 items 1 to 4, ADR-467).
//!
//! The challenge route hands out a random value and remembers it for 60 seconds. The session
//! route takes the agent's proof over that value and, when it holds, a bearer token that the
//! control channel accepts for 300 seconds ([`Sessions::authenticate`]).
//!
//! The session route is judged in this order, and the first step that applies answers: the
//! request guard (before routing), the method (405), the body (408, 413), the shape (400,
//! which consumes nothing), the challenge (401), the agent's row (503, 500), then the key and
//! the proof (403, or 429 past ten refusals within a minute), and last the token. The challenge
//! is removed when it is presented, whatever the outcome after that (ADR-466 item 2); a 401 is
//! never counted and never becomes a 429 (item 3). A refusal is one 403 document whatever the
//! reason, and the reason goes to the log only. The challenge route answers a cap of
//! [`ISSUE_LIMIT`] per [`ISSUE_WINDOW`] with 429, which keeps the unauthenticated database
//! reads of the session route bounded (ADR-467).
//!
//! The stores read tokio's clock through [`now`] and take the instant as a parameter, so a test
//! passes explicit instants and the route test moves the clock.
//!
//! Never logged: a token, a token hash, a challenge, a nonce, a proof or a request body.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use shared_crypto::proof::{Audience, ProofTranscript, VALUE_LEN, verify_proof};
use shared_crypto::{PublicKey, RandomError, Signature, random_32};
use shared_proto::AgentId;
use sqlx::PgPool;

use crate::db;
use crate::health::may_log;
use crate::local_guard::pinned_origin;
use crate::problem::Problem;
use crate::register::{Admission, Limiter, bounded_body, is_json, strict_strings};

/// The challenge route's path.
pub const CHALLENGE_PATH: &str = "/api/v1/agent/challenge";
/// The session route's path.
pub const SESSION_PATH: &str = "/api/v1/agent/session";
/// How long a challenge may wait for its proof; exactly this old is expired.
pub const CHALLENGE_LIFETIME: Duration = Duration::from_secs(60);
/// The most challenges the store holds; at the bound the oldest is dropped (ADR-466 item 2).
pub const MAX_OUTSTANDING: usize = 1024;
/// How many challenges one router issues within [`ISSUE_WINDOW`].
pub const ISSUE_LIMIT: usize = 60;
/// The sliding window of the issuing cap.
pub const ISSUE_WINDOW: Duration = Duration::from_secs(60);
/// How long a session token is valid, in seconds; exactly this old is expired.
pub const TOKEN_LIFETIME_SECS: u64 = 300;
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

const MAX_AGENT_ID: usize = 36;
const MAX_VALUE_TEXT: usize = 64;
const MAX_PROOF_TEXT: usize = 128;
const PROOF_BYTES: usize = 64;
const TOKEN_TEXT: usize = 44;

/// The clock of this module: tokio's, so that a test that moves it moves the stores, the
/// limiters and the log gates together. Without `test-util` it is the monotonic system clock.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

// ---------------------------------------------------------------------------------------------
// The challenge store

/// What a presented challenge turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Issued here, presented once and less than [`CHALLENGE_LIFETIME`] old.
    Live,
    /// Issued here but too old.
    Expired,
    /// Never issued, already presented, or dropped as the oldest at the bound.
    Absent,
}

/// The outstanding challenges of one router, oldest first. The mutex is never held across an
/// await, and its poisoning is ignored, as in the health route.
///
/// No constant-time comparison: a challenge is given to any caller and grants nothing without
/// the agent's key, so learning one only burns it.
#[derive(Default)]
pub struct Challenges {
    live: Mutex<VecDeque<([u8; VALUE_LEN], Instant)>>,
}

impl fmt::Debug for Challenges {
    /// The count only: no challenge is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.live.lock().unwrap_or_else(|e| e.into_inner()).len();
        f.debug_struct("Challenges").field("count", &count).finish()
    }
}

impl Challenges {
    /// Remembers `value`, issued at `now`: expired entries are dropped from the front first,
    /// then the oldest while [`MAX_OUTSTANDING`] remain.
    pub fn issue(&self, value: [u8; VALUE_LEN], now: Instant) {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        while live
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= CHALLENGE_LIFETIME)
        {
            live.pop_front();
        }
        while live.len() >= MAX_OUTSTANDING {
            live.pop_front();
        }
        live.push_back((value, now));
    }

    /// Removes `value` if it is held and says whether it was live at `now`. Whatever the answer,
    /// the value is gone afterwards.
    pub fn take(&self, value: &[u8; VALUE_LEN], now: Instant) -> Taken {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = live.iter().position(|(held, _)| held == value) else {
            return Taken::Absent;
        };
        let (_, issued) = live.remove(index).expect("the index was just found");
        if now.saturating_duration_since(issued) < CHALLENGE_LIFETIME {
            Taken::Live
        } else {
            Taken::Expired
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ---------------------------------------------------------------------------------------------
// The token table

struct Session {
    agent: AgentId,
    expires: Instant,
}

/// The session tokens this server has issued, keyed by the SHA-256 of the token's 32 bytes (the
/// token itself is never stored). A token is a multi-use bearer for [`TOKEN_LIFETIME_SECS`];
/// an agent may hold several, and nothing replaces an earlier one (ADR-466 item 2).
///
/// The table has no bound of its own: every token costs one challenge, so at most
/// [`ISSUE_LIMIT`] a minute are issued, and with the 300 seconds a token lives and the 60 a
/// challenge may wait at most 360 are live (ADR-467). Expired tokens are dropped at each issue.
///
/// No constant-time comparison: the key is a digest of a 256-bit secret, so a timing difference
/// on the digest says nothing usable about the token.
#[derive(Clone, Default)]
pub struct Sessions(Arc<Mutex<HashMap<[u8; 32], Session>>>);

impl fmt::Debug for Sessions {
    /// The count only: neither a token nor its hash is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.lock().unwrap_or_else(|e| e.into_inner()).len();
        f.debug_struct("Sessions").field("count", &count).finish()
    }
}

/// The 32 bytes a token's text stands for, or `None` unless it is strict standard padded base64
/// of exactly 32 bytes.
fn decode_token(text: &str) -> Option<[u8; 32]> {
    if text.len() != TOKEN_TEXT {
        return None;
    }
    STANDARD.decode(text).ok()?.try_into().ok()
}

fn table_key(token: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(token).into()
}

impl Sessions {
    pub fn new() -> Sessions {
        Sessions::default()
    }

    /// The agent a bearer token (the token text alone, without a scheme word) was issued to, or
    /// `None` for an unknown, expired or malformed token. A token may be used any number of
    /// times until it expires.
    pub fn authenticate(&self, bearer_token: &str) -> Option<AgentId> {
        self.authenticate_at(bearer_token, now())
    }

    /// Drops every token of `agent` and returns how many (ADR-466 item 5).
    pub fn revoke_agent(&self, agent: &AgentId) -> usize {
        let mut table = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let before = table.len();
        table.retain(|_, session| session.agent != *agent);
        before - table.len()
    }

    pub(crate) fn authenticate_at(&self, bearer_token: &str, now: Instant) -> Option<AgentId> {
        let key = table_key(&decode_token(bearer_token)?);
        let mut table = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let session = table.get(&key)?;
        if now < session.expires {
            return Some(session.agent.clone());
        }
        table.remove(&key);
        None
    }

    /// Issues a token for `agent` at `now`, after dropping the expired ones.
    pub(crate) fn mint(&self, agent: &AgentId, now: Instant) -> Result<String, RandomError> {
        let token = random_32()?;
        let mut table = self.0.lock().unwrap_or_else(|e| e.into_inner());
        table.retain(|_, session| now < session.expires);
        table.insert(
            table_key(&token),
            Session {
                agent: agent.clone(),
                expires: now + Duration::from_secs(TOKEN_LIFETIME_SECS),
            },
        );
        Ok(STANDARD.encode(token))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

// ---------------------------------------------------------------------------------------------
// The shape of a session request

/// Which rule of the request's shape failed. Each is a 400; none touches the challenge store or
/// the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeError {
    ContentType,
    Json,
    AgentId,
    Challenge,
    Nonce,
    Proof,
}

impl ShapeError {
    fn title(self) -> &'static str {
        match self {
            ShapeError::ContentType => "Content-Type must be application/json",
            ShapeError::Json => "The body is not the expected JSON object",
            ShapeError::AgentId => "Invalid agent ID",
            ShapeError::Challenge => "Invalid challenge",
            ShapeError::Nonce => "Invalid nonce",
            ShapeError::Proof => "Invalid proof",
        }
    }
}

/// A request that passed the shape rules. The proof is 64 bytes, not yet a signature.
struct Shaped {
    agent_id: String,
    challenge: [u8; VALUE_LEN],
    nonce: [u8; VALUE_LEN],
    proof: [u8; PROOF_BYTES],
}

/// True for the lower-case hyphenated UUID text that `agents.id::text` returns: 36 bytes, hex
/// digits, hyphens at 8, 13, 18 and 23.
fn is_agent_id(text: &str) -> bool {
    text.len() == MAX_AGENT_ID
        && text.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// Strict standard padded base64 of exactly `N` bytes, in at most `max_text` characters.
fn decode_fixed<const N: usize>(text: &str, max_text: usize) -> Option<[u8; N]> {
    if text.len() > max_text {
        return None;
    }
    STANDARD.decode(text).ok()?.try_into().ok()
}

fn shape(headers: &HeaderMap, body: &[u8]) -> Result<Shaped, ShapeError> {
    if !is_json(headers) {
        return Err(ShapeError::ContentType);
    }
    let [agent_id, challenge, nonce, proof] =
        strict_strings(body, ["agentId", "challenge", "nonce", "proof"]).ok_or(ShapeError::Json)?;
    if !is_agent_id(&agent_id) {
        return Err(ShapeError::AgentId);
    }
    let challenge = decode_fixed(&challenge, MAX_VALUE_TEXT).ok_or(ShapeError::Challenge)?;
    let nonce = decode_fixed(&nonce, MAX_VALUE_TEXT).ok_or(ShapeError::Nonce)?;
    let proof = decode_fixed(&proof, MAX_PROOF_TEXT).ok_or(ShapeError::Proof)?;
    Ok(Shaped {
        agent_id,
        challenge,
        nonce,
        proof,
    })
}

// ---------------------------------------------------------------------------------------------
// The judgement

/// Why a session was refused; the log's word for it, never the client's. A proof for another
/// audience cannot be told from a bad signature here (the server builds the transcript with its
/// own audience), so it is `Proof` too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    UnknownAgent,
    Revoked,
    Proof,
}

impl Reason {
    fn word(self) -> &'static str {
        match self {
            Reason::UnknownAgent => "unknown_agent",
            Reason::Revoked => "revoked",
            Reason::Proof => "proof",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Judged {
    Refused(Reason),
    /// The server's own data is unusable.
    Internal(&'static str),
}

/// Decides on the agent's row (`public_key`, revoked) and the proof, with no I/O. An unknown or
/// revoked agent is refused without checking the signature.
fn judge(row: Option<(Vec<u8>, bool)>, shaped: &Shaped, audience: &Audience) -> Result<(), Judged> {
    let Some((der, revoked)) = row else {
        return Err(Judged::Refused(Reason::UnknownAgent));
    };
    if revoked {
        return Err(Judged::Refused(Reason::Revoked));
    }
    let key = PublicKey::from_spki_der(&der)
        .map_err(|_| Judged::Internal("the stored public key does not decode"))?;
    let transcript =
        ProofTranscript::new(audience, &shaped.agent_id, shaped.challenge, shaped.nonce)
            .map_err(|_| Judged::Internal("the transcript could not be built"))?;
    let signature =
        Signature::from_bytes(&shaped.proof).map_err(|_| Judged::Refused(Reason::Proof))?;
    verify_proof(&key, &transcript, &signature).map_err(|_| Judged::Refused(Reason::Proof))
}

// ---------------------------------------------------------------------------------------------
// Log gates

const SLOT_UNKNOWN_AGENT: usize = 0;
const SLOT_REVOKED: usize = 1;
const SLOT_PROOF: usize = 2;
const SLOT_UNAVAILABLE: usize = 3;
const SLOT_INTERNAL: usize = 4;
const SLOT_ISSUE_LIMITED: usize = 5;
const SLOT_UNAUTHORIZED: usize = 6;
const SLOTS: usize = 7;

impl Reason {
    fn slot(self) -> usize {
        match self {
            Reason::UnknownAgent => SLOT_UNKNOWN_AGENT,
            Reason::Revoked => SLOT_REVOKED,
            Reason::Proof => SLOT_PROOF,
        }
    }
}

/// The log gates of one router: one per refusal reason and one each for the other events, so
/// that a flood of one cannot silence another.
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
// The routes

/// The state of the two routes: the pool, the challenge store, the token table, the audience
/// and the limiters and log gates of this router.
#[derive(Clone)]
pub struct Exchange(Arc<Inner>);

struct Inner {
    pool: PgPool,
    sessions: Sessions,
    audience: Audience,
    challenges: Challenges,
    issuing: Limiter,
    refusals: Limiter,
    gates: Gates,
}

impl Exchange {
    /// `port` is the listener's bound port: the audience is the pinned origin for it, never a
    /// value from a request. A port the pinned origin cannot be canonical for (0) is a caller's
    /// fault.
    pub fn new(pool: PgPool, port: u16, sessions: Sessions) -> Exchange {
        let audience = Audience::parse_canonical(&pinned_origin(port))
            .expect("the pinned origin of a bound port is a canonical audience");
        Exchange(Arc::new(Inner {
            pool,
            sessions,
            audience,
            challenges: Challenges::default(),
            issuing: Limiter::new(ISSUE_LIMIT, ISSUE_WINDOW),
            refusals: Limiter::default(),
            gates: Gates::new(),
        }))
    }
}

/// A 200 document: uncacheable, since the token is a credential.
fn ok(body: serde_json::Value) -> Response {
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn too_many(retry_after_secs: u64, title: &'static str) -> Response {
    let mut response = Problem::new(StatusCode::TOO_MANY_REQUESTS, title).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
}

/// `POST /api/v1/agent/challenge`: 200 `{"challenge": <base64 of 32 random bytes>}`. The body
/// must be empty; `Content-Type` is not looked at.
pub async fn challenge(State(exchange): State<Exchange>, request: Request) -> Response {
    let inner = &*exchange.0;
    let (_, body) = request.into_parts();
    match bounded_body(body).await {
        Ok(bytes) if bytes.is_empty() => {}
        Ok(_) => {
            return Problem::new(StatusCode::BAD_REQUEST, "The body must be empty").into_response();
        }
        Err(response) => return response,
    }
    // One instant for the cap and the store.
    let now = now();
    if let Admission::Limited { retry_after_secs } = inner.issuing.admit(now) {
        if inner.gates.allow(SLOT_ISSUE_LIMITED, now) {
            tracing::warn!(retry_after_secs, "challenge issuing limited");
        }
        return too_many(retry_after_secs, "Too many challenges requested");
    }
    let Ok(value) = random_32() else {
        if inner.gates.allow(SLOT_INTERNAL, now) {
            tracing::error!("challenge not issued: the random source failed");
        }
        return Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Challenge not issued")
            .into_response();
    };
    inner.challenges.issue(value, now);
    ok(serde_json::json!({ "challenge": STANDARD.encode(value) }))
}

/// `POST /api/v1/agent/session`: 200 `{"token", "expiresInSecs"}` for a valid proof.
pub async fn session(State(exchange): State<Exchange>, request: Request) -> Response {
    let inner = &*exchange.0;
    let (parts, body) = request.into_parts();
    let bytes = match bounded_body(body).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let shaped = match shape(&parts.headers, &bytes) {
        Ok(shaped) => shaped,
        Err(error) => return Problem::new(StatusCode::BAD_REQUEST, error.title()).into_response(),
    };

    // One instant for the challenge take and the answers before the row read.
    let now = now();
    // The challenge is presented here: it is gone afterwards, whatever follows. A stale or
    // unknown one answers before any database access and is neither counted nor limited.
    if inner.challenges.take(&shaped.challenge, now) != Taken::Live {
        return inner.unauthorized(now);
    }

    let row = match inner.read_agent(&shaped.agent_id).await {
        Ok(row) => row,
        Err(error) => {
            let agent = Some(shaped.agent_id.as_str());
            return match db::failure(&error) {
                db::Failure::Unavailable(what) => inner.unavailable(agent, &what, now),
                db::Failure::Internal(what) => inner.internal(agent, &what, now),
            };
        }
    };
    match judge(row, &shaped, &inner.audience) {
        Ok(()) => {}
        // The clock is read again after the row read, so instants enter the refusal limiter in
        // order, as in registration.
        Err(Judged::Refused(reason)) => {
            return inner.refuse(reason, &shaped.agent_id, self::now());
        }
        Err(Judged::Internal(what)) => return inner.internal(Some(&shaped.agent_id), what, now),
    }

    // The token's lifetime starts when it is minted: the clock is read again after the row read,
    // so a wait for a connection or a lock does not shorten the advertised `expiresInSecs`.
    let agent = AgentId(shaped.agent_id.clone());
    let now = self::now();
    match inner.sessions.mint(&agent, now) {
        Ok(token) => {
            tracing::info!(agent_id = %shaped.agent_id, "session issued");
            ok(serde_json::json!({ "token": token, "expiresInSecs": TOKEN_LIFETIME_SECS }))
        }
        Err(_) => inner.internal(Some(&shaped.agent_id), "the random source failed", now),
    }
}

impl Inner {
    /// 401 for a challenge that is missing, expired or already used: the agent's ordinary cue to
    /// fetch a new one. One body for all three, and no count.
    fn unauthorized(&self, now: Instant) -> Response {
        if self.gates.allow(SLOT_UNAUTHORIZED, now) {
            tracing::debug!("session refused: no live challenge");
        }
        let mut response =
            Problem::new(StatusCode::UNAUTHORIZED, "No live challenge").into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        response
    }

    /// Logs a refusal through its reason's gate, then answers 403, or 429 when the limiter says
    /// so.
    fn refuse(&self, reason: Reason, agent_id: &str, now: Instant) -> Response {
        if self.gates.allow(reason.slot(), now) {
            tracing::warn!(reason = reason.word(), %agent_id, "session refused");
        }
        match self.refusals.admit(now) {
            Admission::Counted => {
                Problem::new(StatusCode::FORBIDDEN, "Session refused").into_response()
            }
            Admission::Limited { retry_after_secs } => {
                too_many(retry_after_secs, "Too many refused sessions")
            }
        }
    }

    fn unavailable(&self, agent_id: Option<&str>, what: &str, now: Instant) -> Response {
        if self.gates.allow(SLOT_UNAVAILABLE, now) {
            tracing::warn!(agent_id = agent_id.unwrap_or(""), error = %what, "session unavailable");
        }
        Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Session unavailable").into_response()
    }

    fn internal(&self, agent_id: Option<&str>, what: &str, now: Instant) -> Response {
        if self.gates.allow(SLOT_INTERNAL, now) {
            tracing::error!(agent_id = agent_id.unwrap_or(""), error = %what, "session failed");
        }
        Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Session failed").into_response()
    }

    /// The agent's key and whether its row is revoked: one read in a transaction that is dropped,
    /// so nothing is written. `agent_id` passed the shape rule, so the cast cannot fail.
    async fn read_agent(&self, agent_id: &str) -> Result<Option<(Vec<u8>, bool)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        db::statement_timeouts(&mut tx).await?;
        sqlx::query_as("SELECT public_key, revoked_at IS NOT NULL FROM agents WHERE id = $1::uuid")
            .bind(agent_id)
            .fetch_optional(&mut *tx)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use shared_crypto::proof::sign_proof;
    use shared_crypto::{Signer, SigningKey};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn fresh() -> [u8; 32] {
        random_32().unwrap()
    }

    // ----- the challenge store

    #[test]
    fn a_challenge_is_live_once_and_a_stranger_is_absent() {
        let store = Challenges::default();
        let t0 = Instant::now();
        let (a, stranger) = (fresh(), fresh());
        store.issue(a, t0);
        assert_eq!(store.take(&a, t0), Taken::Live);
        assert_eq!(store.take(&a, t0), Taken::Absent);
        assert_eq!(store.take(&stranger, t0), Taken::Absent);
    }

    #[test]
    fn a_challenge_expires_at_exactly_sixty_seconds_and_is_removed() {
        let store = Challenges::default();
        let t0 = Instant::now();
        let (a, b) = (fresh(), fresh());
        store.issue(a, t0);
        store.issue(b, t0);
        assert_eq!(
            store.take(&a, t0 + secs(60) - Duration::from_millis(1)),
            Taken::Live
        );
        assert_eq!(store.take(&b, t0 + secs(60)), Taken::Expired);
        assert_eq!(store.take(&b, t0), Taken::Absent);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn the_store_drops_the_oldest_at_1024() {
        let store = Challenges::default();
        let t0 = Instant::now();
        let values: Vec<[u8; 32]> = (0..=MAX_OUTSTANDING).map(|_| fresh()).collect();
        for value in &values {
            store.issue(*value, t0);
        }
        assert_eq!(store.len(), MAX_OUTSTANDING);
        assert_eq!(store.take(&values[0], t0), Taken::Absent);
        assert_eq!(store.take(&values[1], t0), Taken::Live);
        assert_eq!(store.take(&values[MAX_OUTSTANDING], t0), Taken::Live);
    }

    #[test]
    fn expired_entries_are_purged_at_issue_and_a_taken_one_frees_its_place() {
        let store = Challenges::default();
        let t0 = Instant::now();
        let old: Vec<[u8; 32]> = (0..MAX_OUTSTANDING).map(|_| fresh()).collect();
        for value in &old {
            store.issue(*value, t0);
        }
        // A new one a minute later finds every old entry expired and drops none as the oldest.
        let late = fresh();
        store.issue(late, t0 + secs(60));
        assert_eq!(store.len(), 1);
        assert_eq!(store.take(&late, t0 + secs(60)), Taken::Live);

        let full: Vec<[u8; 32]> = (0..MAX_OUTSTANDING).map(|_| fresh()).collect();
        for value in &full {
            store.issue(*value, t0);
        }
        assert_eq!(store.take(&full[5], t0), Taken::Live);
        store.issue(fresh(), t0);
        // The place freed by the take was used; the oldest was not pushed out.
        assert_eq!(store.take(&full[0], t0), Taken::Live);
    }

    #[test]
    fn of_fifty_threads_presenting_one_challenge_exactly_one_gets_it() {
        let store = Arc::new(Challenges::default());
        let t0 = Instant::now();
        let value = fresh();
        store.issue(value, t0);
        let threads: Vec<_> = (0..50)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || store.take(&value, t0))
            })
            .collect();
        let live = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|t| *t == Taken::Live)
            .count();
        assert_eq!(live, 1);
    }

    // ----- the token table

    fn agent(n: u8) -> AgentId {
        AgentId(format!("{n:08x}-0000-0000-0000-000000000000"))
    }

    #[test]
    fn a_token_authenticates_repeatedly_and_malformed_ones_are_none() {
        let table = Sessions::new();
        let t0 = Instant::now();
        let a = agent(1);
        let token = table.mint(&a, t0).unwrap();
        assert_eq!(token.len(), 44);
        assert_eq!(table.authenticate_at(&token, t0), Some(a.clone()));
        assert_eq!(table.authenticate_at(&token, t0 + secs(1)), Some(a));

        let raw = STANDARD.decode(&token).unwrap();
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode([0xfbu8; 32]);
        let bad: Vec<String> = vec![
            "garbage".to_owned(),
            String::new(),
            token[..43].to_owned(),
            format!("{token}A"),
            token.trim_end_matches('=').to_owned(),
            url_safe,
            STANDARD.encode(&raw[..31]),
            STANDARD.encode([raw.as_slice(), &[0u8]].concat()),
            format!(" {}", &token[..43]),
        ];
        for (i, text) in bad.iter().enumerate() {
            assert_eq!(table.authenticate_at(text, t0), None, "case {i}");
        }
    }

    #[test]
    fn a_token_expires_at_exactly_three_hundred_seconds_and_mint_sweeps() {
        let table = Sessions::new();
        let t0 = Instant::now();
        let a = agent(1);
        let first = table.mint(&a, t0).unwrap();
        let second = table.mint(&a, t0).unwrap();
        assert_eq!(
            table.authenticate_at(&first, t0 + secs(300) - Duration::from_millis(1)),
            Some(a.clone())
        );
        assert_eq!(table.authenticate_at(&first, t0 + secs(300)), None);
        assert_eq!(table.len(), 1, "the expired hit was removed");
        // The second is expired but still held until the next issue sweeps it.
        let third = table.mint(&a, t0 + secs(300)).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.authenticate_at(&second, t0 + secs(300)), None);
        assert_eq!(table.authenticate_at(&third, t0 + secs(300)), Some(a));
    }

    #[test]
    fn two_tokens_of_one_agent_live_and_revoke_agent_drops_only_that_agents() {
        let table = Sessions::new();
        let t0 = Instant::now();
        let (a, b) = (agent(1), agent(2));
        let (a1, a2, b1) = (
            table.mint(&a, t0).unwrap(),
            table.mint(&a, t0).unwrap(),
            table.mint(&b, t0).unwrap(),
        );
        assert_ne!(a1, a2);
        assert_eq!(table.authenticate_at(&a1, t0), Some(a.clone()));
        assert_eq!(table.authenticate_at(&a2, t0), Some(a.clone()));
        assert_eq!(table.revoke_agent(&a), 2);
        assert_eq!(table.revoke_agent(&a), 0);
        assert_eq!(table.authenticate_at(&a1, t0), None);
        assert_eq!(table.authenticate_at(&a2, t0), None);
        assert_eq!(table.authenticate_at(&b1, t0), Some(b));
    }

    #[test]
    fn the_table_is_keyed_by_the_hash_and_debug_shows_no_secret() {
        let table = Sessions::new();
        let t0 = Instant::now();
        let token = table.mint(&agent(1), t0).unwrap();
        let raw: [u8; 32] = STANDARD.decode(&token).unwrap().try_into().unwrap();
        {
            let held = table.0.lock().unwrap();
            assert!(held.contains_key(&table_key(&raw)));
            assert!(!held.contains_key(&raw));
        }
        let shown = format!("{table:?}");
        assert!(shown.contains("count"), "{shown}");
        assert!(!shown.contains(&token));
        let hash = table_key(&raw);
        assert!(!shown.contains(&STANDARD.encode(hash)));
        assert!(!shown.contains(&format!("{:?}", &hash[..4])));
    }

    // ----- the issuing cap

    #[test]
    fn the_issuing_cap_admits_sixty_and_limits_the_next_with_the_exact_seconds() {
        let cap = Limiter::new(ISSUE_LIMIT, ISSUE_WINDOW);
        let t0 = Instant::now();
        for i in 0..60 {
            assert_eq!(
                cap.admit(t0 + Duration::from_millis(i)),
                Admission::Counted,
                "{i}"
            );
        }
        assert_eq!(
            cap.admit(t0 + secs(10)),
            Admission::Limited {
                retry_after_secs: 50
            }
        );
        // Limited calls do not extend the window.
        for i in 11..40 {
            assert!(matches!(cap.admit(t0 + secs(i)), Admission::Limited { .. }));
        }
        assert_eq!(cap.admit(t0 + secs(60)), Admission::Counted);
        assert!(matches!(
            cap.admit(t0 + secs(60)),
            Admission::Limited { .. }
        ));
    }

    // ----- the shape

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h
    }

    const ID: &str = "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab";

    fn body(id: &str, challenge: &str, nonce: &str, proof: &str) -> Vec<u8> {
        serde_json::json!({"agentId": id, "challenge": challenge, "nonce": nonce, "proof": proof})
            .to_string()
            .into_bytes()
    }

    fn b64(n: usize) -> String {
        STANDARD.encode(vec![7u8; n])
    }

    fn shape_of(headers: &HeaderMap, body: &[u8]) -> Result<(), ShapeError> {
        shape(headers, body).map(|_| ())
    }

    #[test]
    fn the_shape_rules_each_have_their_own_error() {
        let h = json_headers();
        let (c, n, p) = (b64(32), b64(32), b64(64));
        assert_eq!(shape_of(&h, &body(ID, &c, &n, &p)), Ok(()));
        // Parameters on the media type and any case pass.
        let mut with_params = HeaderMap::new();
        with_params.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Application/JSON; charset=utf-8"),
        );
        assert_eq!(shape_of(&with_params, &body(ID, &c, &n, &p)), Ok(()));

        // Content-Type: none, wrong, two.
        let good = body(ID, &c, &n, &p);
        assert_eq!(
            shape_of(&HeaderMap::new(), &good),
            Err(ShapeError::ContentType)
        );
        let mut text = HeaderMap::new();
        text.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert_eq!(shape_of(&text, &good), Err(ShapeError::ContentType));
        let mut two = json_headers();
        two.append(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(shape_of(&two, &good), Err(ShapeError::ContentType));

        // The JSON object.
        let json_cases: [(&str, Vec<u8>); 8] = [
            ("not json", b"nope".to_vec()),
            ("array", b"[]".to_vec()),
            ("empty", b"{}".to_vec()),
            (
                "missing",
                serde_json::json!({"agentId": ID, "challenge": c, "nonce": n})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "unknown",
                serde_json::json!({"agentId": ID, "challenge": c, "nonce": n, "proof": p, "x": "y"})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "duplicate",
                format!(
                    r#"{{"agentId":"{ID}","agentId":"{ID}","challenge":"{c}","nonce":"{n}","proof":"{p}"}}"#
                )
                .into_bytes(),
            ),
            (
                "non-string",
                format!(
                    r#"{{"agentId":"{ID}","challenge":"{c}","nonce":"{n}","proof":5}}"#
                )
                .into_bytes(),
            ),
            ("trailing", [good.clone(), b" x".to_vec()].concat()),
        ];
        for (name, bytes) in &json_cases {
            assert_eq!(shape_of(&h, bytes), Err(ShapeError::Json), "{name}");
        }
        // Whitespace after the value is not bytes of another value.
        assert_eq!(
            shape_of(&h, &[good.clone(), b"\n ".to_vec()].concat()),
            Ok(())
        );

        // The agent ID.
        let ids = [
            "0A1B2C3D-4E5F-4A6B-8C7D-0123456789AB",
            "{0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab}",
            "0a1b2c3d4e5f4a6b8c7d0123456789ab",
            "0a1b2c3d-4e5f-4a6b-8c7d-0123456789a",
            "0a1b2c3d-4e5f-4a6b-8c7d-0123456789abc",
            "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ag",
            "0a1b2c3-d4e5f-4a6b-8c7d-0123456789ab",
            "",
        ];
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(
                shape_of(&h, &body(id, &c, &n, &p)),
                Err(ShapeError::AgentId),
                "id case {i}"
            );
        }

        // Challenge, nonce and proof: length, alphabet, padding, size.
        let long = "A".repeat(65);
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode([0xfbu8; 32]);
        let bad_values = [
            b64(31),
            b64(33),
            b64(32).trim_end_matches('=').to_owned(),
            "!".repeat(44),
            url_safe,
            long,
            String::new(),
        ];
        for (i, v) in bad_values.iter().enumerate() {
            assert_eq!(
                shape_of(&h, &body(ID, v, &n, &p)),
                Err(ShapeError::Challenge),
                "challenge case {i}"
            );
            assert_eq!(
                shape_of(&h, &body(ID, &c, v, &p)),
                Err(ShapeError::Nonce),
                "nonce case {i}"
            );
        }
        let bad_proofs = [
            b64(63),
            b64(65),
            b64(64).trim_end_matches('=').to_owned(),
            "A".repeat(129),
            String::new(),
        ];
        for (i, v) in bad_proofs.iter().enumerate() {
            assert_eq!(
                shape_of(&h, &body(ID, &c, &n, v)),
                Err(ShapeError::Proof),
                "proof case {i}"
            );
        }
        // The last data character's spare bits must be zero.
        let mut bent = c.clone().into_bytes();
        let last = bent.len() - 2;
        assert_eq!(
            bent[last], b'c',
            "32 sevens end in a character with zero spare bits"
        );
        bent[last] = b'd';
        assert_eq!(
            shape_of(&h, &body(ID, std::str::from_utf8(&bent).unwrap(), &n, &p)),
            Err(ShapeError::Challenge)
        );
    }

    // ----- the judgement

    struct Case {
        key: SigningKey,
        audience: Audience,
        shaped: Shaped,
    }

    fn audience(port: u16) -> Audience {
        Audience::parse_canonical(&pinned_origin(port)).unwrap()
    }

    fn sign(key: &SigningKey, audience: &Audience, id: &str, c: [u8; 32], n: [u8; 32]) -> [u8; 64] {
        let transcript = ProofTranscript::new(audience, id, c, n).unwrap();
        sign_proof(key, &transcript).unwrap().to_bytes()
    }

    fn case() -> Case {
        let key = SigningKey::generate().unwrap();
        let audience = audience(8080);
        let (challenge, nonce) = (fresh(), fresh());
        let proof = sign(&key, &audience, ID, challenge, nonce);
        Case {
            key,
            audience,
            shaped: Shaped {
                agent_id: ID.to_owned(),
                challenge,
                nonce,
                proof,
            },
        }
    }

    fn row(key: &SigningKey, revoked: bool) -> Option<(Vec<u8>, bool)> {
        Some((key.public_key().to_spki_der(), revoked))
    }

    #[test]
    fn judge_decides_on_the_row_and_the_proof() {
        let c = case();
        assert_eq!(judge(row(&c.key, false), &c.shaped, &c.audience), Ok(()));
        // No row, and a revoked row whose signature is valid.
        assert_eq!(
            judge(None, &c.shaped, &c.audience),
            Err(Judged::Refused(Reason::UnknownAgent))
        );
        assert_eq!(
            judge(row(&c.key, true), &c.shaped, &c.audience),
            Err(Judged::Refused(Reason::Revoked))
        );
        let proof_refused = Err(Judged::Refused(Reason::Proof));
        // Another key.
        let other = SigningKey::generate().unwrap();
        assert_eq!(
            judge(row(&other, false), &c.shaped, &c.audience),
            proof_refused
        );
        // Another audience (the server's own is another port).
        assert_eq!(
            judge(row(&c.key, false), &c.shaped, &audience(8081)),
            proof_refused
        );
        // Another agent ID, challenge and nonce in the request than in the proof.
        let id2 = "ffffffff-4e5f-4a6b-8c7d-0123456789ab";
        let with = |f: &dyn Fn(&mut Shaped)| {
            // Each field named: Rust 1.94 (the MSRV) hits an internal compiler error on the
            // struct-update form here.
            let mut s = Shaped {
                agent_id: c.shaped.agent_id.clone(),
                challenge: c.shaped.challenge,
                nonce: c.shaped.nonce,
                proof: c.shaped.proof,
            };
            f(&mut s);
            judge(row(&c.key, false), &s, &c.audience)
        };
        assert_eq!(with(&|s| s.agent_id = id2.to_owned()), proof_refused);
        assert_eq!(with(&|s| s.challenge = fresh()), proof_refused);
        assert_eq!(with(&|s| s.nonce = fresh()), proof_refused);
        // The same signature over the proof tag's other purpose cannot be told apart from a bad
        // one; the all-zero pair is no signature at all.
        assert_eq!(with(&|s| s.proof = [0u8; 64]), proof_refused);
        // The high-S twin of the valid signature.
        assert_eq!(with(&|s| s.proof = high_s(s.proof)), proof_refused);
        // A stored key that does not decode is the server's own fault.
        assert!(matches!(
            judge(Some((vec![1, 2, 3], false)), &c.shaped, &c.audience),
            Err(Judged::Internal(_))
        ));
        // A revoked row is judged before its key is decoded.
        assert_eq!(
            judge(Some((vec![1, 2, 3], true)), &c.shaped, &c.audience),
            Err(Judged::Refused(Reason::Revoked))
        );
    }

    /// The signature with `s` replaced by the group order minus `s`.
    fn high_s(signature: [u8; 64]) -> [u8; 64] {
        const ORDER: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let mut out = signature;
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = i16::from(ORDER[i]) - i16::from(signature[32 + i]) - borrow;
            borrow = i16::from(d < 0);
            out[32 + i] = u8::try_from(d.rem_euclid(256)).unwrap();
        }
        out
    }

    #[test]
    fn the_pinned_origin_is_a_canonical_audience_for_every_port() {
        for port in 1..=u16::MAX {
            let text = pinned_origin(port);
            let audience = Audience::parse_canonical(&text).expect("canonical");
            assert_eq!(audience.as_str(), text, "port {port}");
        }
    }

    #[test]
    fn each_refusal_reason_has_its_own_log_gate() {
        let gates = Gates::new();
        let t0 = Instant::now();
        for reason in [Reason::UnknownAgent, Reason::Revoked, Reason::Proof] {
            assert!(gates.allow(reason.slot(), t0), "{}", reason.word());
            assert!(!gates.allow(reason.slot(), t0 + secs(59)));
        }
        for slot in [
            SLOT_UNAVAILABLE,
            SLOT_INTERNAL,
            SLOT_ISSUE_LIMITED,
            SLOT_UNAUTHORIZED,
        ] {
            assert!(gates.allow(slot, t0), "slot {slot}");
        }
        assert!(gates.allow(SLOT_PROOF, t0 + secs(60)));
    }
}

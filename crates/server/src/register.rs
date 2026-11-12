//! `POST /api/v1/agents/register`: the redemption of a registration code (design 6.4, 6.7, 14.4;
//! ADR-463, ADR-465 "Redemption design"). Issuance is in [`crate::registration`].
//!
//! A request is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the body (read within [`BODY_TIMEOUT`], 408; at most
//! [`MAX_BODY`] bytes, 413), the Content-Type and the shape (400, with no database lookup), the
//! proof (checked against this server's in-memory key ID, with no database lookup), then one
//! transaction that consumes the code, registers the agent and signs the response before it
//! commits. A refused request is the same 403 document whatever the reason; the reason goes to
//! the log only. The [`Limiter`] turns the refusals past the tenth within a minute into 429. A
//! redemption never consults it, and neither does any answer but a refusal.
//!
//! The code, the display name and the owner are never logged; a code appears only through its
//! redacted `Debug` (the first bytes of its hash).

use std::collections::VecDeque;
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
use http_body_util::BodyExt;
use serde::Deserialize;
use serde::de::{self, MapAccess, Visitor};
use shared_crypto::registration::{
    Mode, Preset, RegistrationCode, RegistrationTranscript, verify_registration,
};
use shared_crypto::{PublicKey, Signature};
use sqlx::PgPool;

use crate::health::may_log;
use crate::instruction_key::{InstructionKey, RegistrationSignError};
use crate::problem::Problem;

/// The route's path.
pub const PATH: &str = "/api/v1/agents/register";
/// How long the body may take to arrive in full.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest body accepted, in bytes; exactly this many pass.
pub const MAX_BODY: usize = 4096;
/// How many refusals the window holds.
pub const LIMIT: usize = 10;
/// The window the refusals count over.
pub const WINDOW: Duration = Duration::from_secs(60);
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// The first of the two `int4` keys of the per-key advisory lock. The two-integer form cannot
/// collide with sqlx's migration lock, which uses the single-`bigint` form.
const LOCK_CLASS: i32 = 0x4e47_5231;

const MAX_CODE: usize = 64;
const MAX_PUBLIC_KEY: usize = 512;
const MAX_PROOF: usize = 128;
const PROOF_BYTES: usize = 64;

// ---------------------------------------------------------------------------------------------
// The limiter

/// What the limiter says about one would-be refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Answer 403; the refusal is now recorded.
    Counted,
    /// Answer 429 with this `Retry-After`; nothing is recorded.
    Limited { retry_after_secs: u64 },
}

/// A sliding window over the last [`LIMIT`] counted refusals of one router (ADR-465). The mutex
/// is never held across an await, and its poisoning is ignored, as in the health route.
#[derive(Debug, Default)]
pub struct Limiter {
    counted: Mutex<VecDeque<Instant>>,
}

impl Limiter {
    /// Judges one would-be refusal at `now`. With [`LIMIT`] refusals recorded and the oldest
    /// younger than [`WINDOW`], it is limited, with the seconds until that oldest is a window
    /// old, rounded up (1 to 60); otherwise it is recorded and counted.
    pub fn refusal(&self, now: Instant) -> Admission {
        let mut counted = self.counted.lock().unwrap_or_else(|e| e.into_inner());
        if counted.len() >= LIMIT {
            let oldest = *counted.front().expect("the window is full");
            let age = now.saturating_duration_since(oldest);
            if age < WINDOW {
                let left = WINDOW - age;
                let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                return Admission::Limited {
                    retry_after_secs: secs.clamp(1, WINDOW.as_secs()),
                };
            }
            counted.pop_front();
        }
        counted.push_back(now);
        Admission::Counted
    }
}

// ---------------------------------------------------------------------------------------------
// The shape of a request

/// Which rule of the request's shape failed. Each is a 400; none touches the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeError {
    ContentType,
    Json,
    Code,
    PublicKey,
    Mode,
    Proof,
}

impl ShapeError {
    fn title(self) -> &'static str {
        match self {
            ShapeError::ContentType => "Content-Type must be application/json",
            ShapeError::Json => "The body is not the expected JSON object",
            ShapeError::Code => "Invalid registration code format",
            ShapeError::PublicKey => "Invalid public key",
            ShapeError::Mode => "Invalid mode",
            ShapeError::Proof => "Invalid proof",
        }
    }
}

/// A request that passed the shape rules. The proof is 64 bytes, not yet a signature.
struct Shaped {
    code: RegistrationCode,
    key: PublicKey,
    mode: Mode,
    proof: Vec<u8>,
}

struct Fields {
    code: String,
    public_key: String,
    mode: String,
    proof: String,
}

impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// Accepts a JSON object only (a derived struct would also accept an array), with each of the
/// four fields exactly once, as a string, and no other.
struct FieldsVisitor;

impl<'de> Visitor<'de> for FieldsVisitor {
    type Value = Fields;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object with the four registration fields")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
        let (mut code, mut public_key, mut mode, mut proof) = (None, None, None, None);
        while let Some(name) = map.next_key::<String>()? {
            let slot = match name.as_str() {
                "code" => &mut code,
                "publicKey" => &mut public_key,
                "mode" => &mut mode,
                "proof" => &mut proof,
                _ => return Err(de::Error::custom("unknown field")),
            };
            if slot.is_some() {
                return Err(de::Error::custom("duplicate field"));
            }
            *slot = Some(map.next_value::<String>()?);
        }
        match (code, public_key, mode, proof) {
            (Some(code), Some(public_key), Some(mode), Some(proof)) => Ok(Fields {
                code,
                public_key,
                mode,
                proof,
            }),
            _ => Err(de::Error::custom("missing field")),
        }
    }
}

/// True when the headers carry exactly one Content-Type whose media type is `application/json`
/// (parameters allowed).
fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// The shape rules H5 to H10 of ADR-465, in order, on a body of at most [`MAX_BODY`] bytes.
fn shape(headers: &HeaderMap, body: &[u8]) -> Result<Shaped, ShapeError> {
    if !is_json(headers) {
        return Err(ShapeError::ContentType);
    }
    let fields: Fields = serde_json::from_slice(body).map_err(|_| ShapeError::Json)?;
    if fields.code.len() > MAX_CODE {
        return Err(ShapeError::Code);
    }
    let code = RegistrationCode::parse(&fields.code).map_err(|_| ShapeError::Code)?;
    if fields.public_key.len() > MAX_PUBLIC_KEY {
        return Err(ShapeError::PublicKey);
    }
    let key = PublicKey::from_spki_pem(&fields.public_key).map_err(|_| ShapeError::PublicKey)?;
    let mode = Mode::parse(&fields.mode).ok_or(ShapeError::Mode)?;
    if fields.proof.len() > MAX_PROOF {
        return Err(ShapeError::Proof);
    }
    let proof = STANDARD
        .decode(&fields.proof)
        .ok()
        .filter(|bytes| bytes.len() == PROOF_BYTES)
        .ok_or(ShapeError::Proof)?;
    Ok(Shaped {
        code,
        key,
        mode,
        proof,
    })
}

/// True when the proof is a valid signature by the request's key over the transcript of its code,
/// key and mode and this server's key ID (V1). A 64-byte value that is no signature fails here,
/// as a refusal, not as a shape error.
fn proof_holds(shaped: &Shaped, server_key_id: [u8; 32]) -> bool {
    let Ok(signature) = Signature::from_bytes(&shaped.proof) else {
        return false;
    };
    let transcript =
        RegistrationTranscript::new(&shaped.code, &shaped.key, shaped.mode, server_key_id);
    verify_registration(&shaped.key, &transcript, &signature).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Refusal reasons and their log gates

/// Why a request was refused; the log's word for it, never the client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Proof,
    Absent,
    Revoked,
    Used,
    Expired,
    OtherKey,
    InstructionKey,
    Owner,
    StateChanged,
}

impl Reason {
    const COUNT: usize = 9;

    fn word(self) -> &'static str {
        match self {
            Reason::Proof => "proof",
            Reason::Absent => "absent",
            Reason::Revoked => "revoked",
            Reason::Used => "used",
            Reason::Expired => "expired",
            Reason::OtherKey => "other_key",
            Reason::InstructionKey => "instruction_key",
            Reason::Owner => "owner",
            Reason::StateChanged => "state_changed",
        }
    }
}

/// The log gates of one router: one per refusal reason and one each for the unavailable (503)
/// and the internal (500) failure, so that a flood of one cannot silence another.
struct Gates {
    slots: [Mutex<Option<Instant>>; Reason::COUNT + 2],
}

const SLOT_UNAVAILABLE: usize = Reason::COUNT;
const SLOT_INTERNAL: usize = Reason::COUNT + 1;

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
// The route

/// The route's state: the pool, the key, and the limiter and log gates of this router.
#[derive(Clone)]
pub struct Registrar(Arc<Inner>);

struct Inner {
    pool: PgPool,
    key: Arc<InstructionKey>,
    limiter: Limiter,
    gates: Gates,
}

impl Registrar {
    pub fn new(pool: PgPool, key: Arc<InstructionKey>) -> Registrar {
        Registrar(Arc::new(Inner {
            pool,
            key,
            limiter: Limiter::default(),
            gates: Gates::new(),
        }))
    }
}

/// What a failed redemption is.
enum RedeemError {
    Refused(Reason),
    /// The pool, the connection or a statement failed, or timed out. The text is coarse.
    Unavailable(String),
    /// The signer failed, or anything else.
    Internal(String),
}

impl From<sqlx::Error> for RedeemError {
    fn from(error: sqlx::Error) -> Self {
        // Never the error's own text: a constraint failure's detail can name a value.
        RedeemError::Unavailable(match &error {
            sqlx::Error::Database(db) => match db.code() {
                Some(code) => format!("database error, SQLSTATE {code}"),
                None => "database error".to_owned(),
            },
            sqlx::Error::PoolTimedOut => "no connection available in time".to_owned(),
            sqlx::Error::PoolClosed => "the pool is closed".to_owned(),
            sqlx::Error::Io(_) => "connection failed".to_owned(),
            _ => "database failure".to_owned(),
        })
    }
}

impl From<RegistrationSignError> for RedeemError {
    fn from(error: RegistrationSignError) -> Self {
        RedeemError::Internal(error.to_string())
    }
}

/// What a redemption returns to the client.
struct Registered {
    agent_id: String,
    owner_id: String,
    revoked: u64,
    signature: Signature,
}

enum BodyError {
    TooLarge,
    Timeout,
    Broken,
}

/// Reads the body in full within [`BODY_TIMEOUT`], refusing more than [`MAX_BODY`] bytes as soon
/// as they have arrived.
async fn read_body(body: axum::body::Body) -> Result<Vec<u8>, BodyError> {
    let read = async {
        let mut body = body;
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| BodyError::Broken)?;
            if let Ok(data) = frame.into_data() {
                if bytes.len() + data.len() > MAX_BODY {
                    return Err(BodyError::TooLarge);
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok(bytes)
    };
    tokio::time::timeout(BODY_TIMEOUT, read)
        .await
        .unwrap_or(Err(BodyError::Timeout))
}

/// An answer that ends the connection: the body may be unread.
fn closing(status: StatusCode, title: &'static str) -> Response {
    let mut response = Problem::new(status, title).into_response();
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

/// `POST /api/v1/agents/register`.
pub async fn register(State(registrar): State<Registrar>, request: Request) -> Response {
    let inner = &*registrar.0;
    let (parts, body) = request.into_parts();
    let bytes = match read_body(body).await {
        Ok(bytes) => bytes,
        Err(BodyError::Timeout) => {
            return closing(
                StatusCode::REQUEST_TIMEOUT,
                "The request body took too long",
            );
        }
        Err(BodyError::TooLarge) => {
            return closing(
                StatusCode::PAYLOAD_TOO_LARGE,
                "The request body is too large",
            );
        }
        Err(BodyError::Broken) => {
            return closing(StatusCode::BAD_REQUEST, ShapeError::Json.title());
        }
    };
    let shaped = match shape(&parts.headers, &bytes) {
        Ok(shaped) => shaped,
        Err(error) => return Problem::new(StatusCode::BAD_REQUEST, error.title()).into_response(),
    };

    // The key is read once: its ID, its PEM and the transcript all come from this value.
    let server_key = inner.key.public_key();
    let server_key_id = server_key.fingerprint_hex();
    if !proof_holds(&shaped, server_key.fingerprint()) {
        return inner.refuse(Reason::Proof, &shaped.code);
    }

    match inner.redeem(&shaped, &server_key_id).await {
        Ok(done) => {
            tracing::info!(
                agent_id = %done.agent_id,
                revoked = done.revoked,
                "agent registered"
            );
            let mut response = (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "agentId": done.agent_id,
                    "ownerId": done.owner_id,
                    "preset": Preset::Personal.as_str(),
                    "allowRemote": false,
                    "instructionKey": server_key.to_spki_pem(),
                    "instructionKeyId": server_key_id,
                    "signature": STANDARD.encode(done.signature.to_bytes()),
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(RedeemError::Refused(reason)) => inner.refuse(reason, &shaped.code),
        Err(RedeemError::Unavailable(what)) => {
            if inner.gates.allow(SLOT_UNAVAILABLE, Instant::now()) {
                tracing::warn!(code = ?shaped.code, error = %what, "registration unavailable");
            }
            Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Registration unavailable")
                .into_response()
        }
        Err(RedeemError::Internal(what)) => {
            if inner.gates.allow(SLOT_INTERNAL, Instant::now()) {
                tracing::error!(code = ?shaped.code, error = %what, "registration failed");
            }
            Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Registration failed").into_response()
        }
    }
}

impl Inner {
    /// Logs a refusal through its reason's gate, then answers 403, or 429 when the limiter says
    /// so.
    fn refuse(&self, reason: Reason, code: &RegistrationCode) -> Response {
        let now = Instant::now();
        if self.gates.allow(reason as usize, now) {
            tracing::warn!(reason = reason.word(), code = ?code, "registration refused");
        }
        match self.limiter.refusal(now) {
            Admission::Counted => {
                Problem::new(StatusCode::FORBIDDEN, "Registration refused").into_response()
            }
            Admission::Limited { retry_after_secs } => {
                let mut response = Problem::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many refused registrations",
                )
                .into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
                response
            }
        }
    }

    /// The transaction of ADR-465, in lock order: the code row, the key's advisory lock, the
    /// agent rows. Any early return drops the transaction, which rolls back.
    async fn redeem(
        &self,
        shaped: &Shaped,
        server_key_id: &str,
    ) -> Result<Registered, RedeemError> {
        let hash = shaped.code.sha256();
        let agent_key_id = shaped.key.fingerprint_hex();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "SELECT set_config('lock_timeout', '5s', true), \
                    set_config('statement_timeout', '5s', true)",
        )
        .execute(&mut *tx)
        .await?;

        let consumed: Option<(String, String, String)> = sqlx::query_as(
            "UPDATE agent_registration_codes c SET used_at = now() \
             WHERE c.code_sha256 = $1 AND c.used_at IS NULL AND c.revoked_at IS NULL \
               AND c.expires_at > now() AND c.agent_key_id = $2 AND c.instruction_key_id = $3 \
               AND EXISTS (SELECT 1 FROM instruction_keys k \
                           WHERE k.key_id = c.instruction_key_id AND k.retired_at IS NULL) \
               AND EXISTS (SELECT 1 FROM operators o \
                           WHERE o.id = c.owner_id AND o.tenant_id = c.tenant_id \
                             AND o.disabled_at IS NULL) \
             RETURNING c.tenant_id::text, c.owner_id::text, c.display_name",
        )
        .bind(&hash[..])
        .bind(&agent_key_id)
        .bind(server_key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant_id, owner_id, display_name)) = consumed else {
            let reason = self
                .classify(&mut tx, &hash, &agent_key_id, server_key_id)
                .await?;
            return Err(RedeemError::Refused(reason));
        };

        // The key's first four bytes: a collision only serialises two keys needlessly.
        let fingerprint = shaped.key.fingerprint();
        let lock_key = i32::from_be_bytes([
            fingerprint[0],
            fingerprint[1],
            fingerprint[2],
            fingerprint[3],
        ]);
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(LOCK_CLASS)
            .bind(lock_key)
            .execute(&mut *tx)
            .await?;

        let der = shaped.key.to_spki_der();
        let revoked = sqlx::query(
            "UPDATE agents SET revoked_at = now() WHERE public_key = $1 AND revoked_at IS NULL",
        )
        .bind(&der)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        // The preset and remote use are the values that are signed, not column defaults.
        let preset = Preset::Personal;
        let allow_remote = false;
        let agent_id: String = sqlx::query_scalar(
            "INSERT INTO agents \
                 (id, tenant_id, display_name, mode, public_key, preset, allow_remote, owner_id) \
             VALUES (gen_random_uuid(), $1::uuid, $2, $3, $4, $5, $6, $7::uuid) \
             RETURNING id::text",
        )
        .bind(&tenant_id)
        .bind(&display_name)
        .bind(shaped.mode.as_str())
        .bind(&der)
        .bind(preset.as_str())
        .bind(allow_remote)
        .bind(&owner_id)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO agent_allowed_operators (tenant_id, agent_id, operator_id) \
             VALUES ($1::uuid, $2::uuid, $3::uuid)",
        )
        .bind(&tenant_id)
        .bind(&agent_id)
        .bind(&owner_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE agent_registration_codes SET agent_id = $2::uuid WHERE code_sha256 = $1",
        )
        .bind(&hash[..])
        .bind(&agent_id)
        .execute(&mut *tx)
        .await?;

        // Signed before the commit: a failure here leaves the code redeemable.
        let signature = self
            .key
            .sign_registration_response(
                &shaped.code,
                &shaped.key,
                &agent_id,
                &owner_id,
                preset,
                allow_remote,
            )
            .await?;

        tx.commit().await?;
        Ok(Registered {
            agent_id,
            owner_id,
            revoked,
            signature,
        })
    }

    /// Why the consuming statement matched no row: one read of the row's flags, for the log only.
    /// The caller then drops the transaction.
    async fn classify(
        &self,
        tx: &mut sqlx::PgConnection,
        hash: &[u8; 32],
        agent_key_id: &str,
        server_key_id: &str,
    ) -> Result<Reason, sqlx::Error> {
        let flags: Option<(bool, bool, bool, bool, bool, bool)> = sqlx::query_as(
            "SELECT c.revoked_at IS NOT NULL, c.used_at IS NOT NULL, c.expires_at <= now(), \
                    c.agent_key_id <> $2, \
                    c.instruction_key_id <> $3 OR NOT EXISTS (SELECT 1 FROM instruction_keys k \
                        WHERE k.key_id = c.instruction_key_id AND k.retired_at IS NULL), \
                    NOT EXISTS (SELECT 1 FROM operators o \
                        WHERE o.id = c.owner_id AND o.tenant_id = c.tenant_id \
                          AND o.disabled_at IS NULL) \
             FROM agent_registration_codes c WHERE c.code_sha256 = $1",
        )
        .bind(&hash[..])
        .bind(agent_key_id)
        .bind(server_key_id)
        .fetch_optional(tx)
        .await?;
        Ok(match flags {
            None => Reason::Absent,
            Some((true, ..)) => Reason::Revoked,
            Some((_, true, ..)) => Reason::Used,
            Some((_, _, true, ..)) => Reason::Expired,
            Some((_, _, _, true, ..)) => Reason::OtherKey,
            Some((_, _, _, _, true, _)) => Reason::InstructionKey,
            Some((_, _, _, _, _, true)) => Reason::Owner,
            Some(_) => Reason::StateChanged,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use shared_crypto::registration::sign_registration;
    use shared_crypto::{Signer, SigningKey, random_32};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    // ----- the limiter

    #[test]
    fn ten_refusals_count_and_the_eleventh_is_limited_with_the_exact_seconds() {
        let limiter = Limiter::default();
        let t0 = Instant::now();
        for i in 0..10 {
            assert_eq!(limiter.refusal(t0 + secs(i)), Admission::Counted, "{i}");
        }
        // The oldest is t0: at t0 + 10 it is 10 s old, so 50 s remain.
        assert_eq!(
            limiter.refusal(t0 + secs(10)),
            Admission::Limited {
                retry_after_secs: 50
            }
        );
        // Rounded up, and never below one.
        assert_eq!(
            limiter.refusal(t0 + secs(59) + Duration::from_millis(500)),
            Admission::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(
            limiter.refusal(t0),
            Admission::Limited {
                retry_after_secs: 60
            }
        );
    }

    #[test]
    fn a_limited_call_does_not_extend_the_window() {
        let limiter = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..10 {
            limiter.refusal(t0);
        }
        for i in 1..30 {
            assert!(matches!(
                limiter.refusal(t0 + secs(i)),
                Admission::Limited { .. }
            ));
        }
        // Thirty limited calls later, the oldest still decides.
        assert_eq!(limiter.refusal(t0 + secs(60)), Admission::Counted);
    }

    #[test]
    fn one_slot_frees_at_exactly_sixty_seconds() {
        let limiter = Limiter::default();
        let t0 = Instant::now();
        for i in 0..10 {
            limiter.refusal(t0 + secs(i));
        }
        assert_eq!(
            limiter.refusal(t0 + secs(60) - Duration::from_nanos(1)),
            Admission::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(limiter.refusal(t0 + secs(60)), Admission::Counted);
        // That took the slot: the oldest is now t0 + 1 s, which is 59 s old.
        assert_eq!(
            limiter.refusal(t0 + secs(60)),
            Admission::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(limiter.refusal(t0 + secs(61)), Admission::Counted);
    }

    #[test]
    fn fifty_threads_at_one_instant_give_exactly_ten_counted() {
        let limiter = Arc::new(Limiter::default());
        let now = Instant::now();
        let handles: Vec<_> = (0..50)
            .map(|_| {
                let limiter = Arc::clone(&limiter);
                std::thread::spawn(move || limiter.refusal(now))
            })
            .collect();
        let counted = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|a| *a == Admission::Counted)
            .count();
        assert_eq!(counted, 10);
    }

    #[test]
    fn a_poisoned_limiter_still_answers() {
        let limiter = Arc::new(Limiter::default());
        let other = Arc::clone(&limiter);
        let _ = std::thread::spawn(move || {
            let _guard = other.counted.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert_eq!(limiter.refusal(Instant::now()), Admission::Counted);
    }

    // ----- the shape

    struct Parts {
        code: String,
        public_key: String,
        proof: String,
    }

    fn parts() -> Parts {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let code = RegistrationCode::generate().unwrap();
        let server = random_32().unwrap();
        let signature = sign_registration(
            &key,
            &RegistrationTranscript::new(&code, &public, Mode::User, server),
        )
        .unwrap();
        Parts {
            code: code.as_str().to_owned(),
            public_key: public.to_spki_pem(),
            proof: STANDARD.encode(signature.to_bytes()),
        }
    }

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h
    }

    fn body(code: &str, key: &str, mode: &str, proof: &str) -> Vec<u8> {
        serde_json::json!({"code": code, "publicKey": key, "mode": mode, "proof": proof})
            .to_string()
            .into_bytes()
    }

    fn shape_of(headers: &HeaderMap, body: &[u8]) -> Result<(), ShapeError> {
        shape(headers, body).map(|_| ())
    }

    #[test]
    fn a_well_formed_request_passes() {
        let p = parts();
        let b = body(&p.code, &p.public_key, "user", &p.proof);
        assert_eq!(shape_of(&json_headers(), &b), Ok(()));
        // Parameters on the media type, any case, and a typed code.
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Application/JSON ; charset=utf-8"),
        );
        let typed = body(
            &format!(" {}\n", p.code.to_lowercase()),
            &p.public_key,
            "device",
            &p.proof,
        );
        assert_eq!(shape_of(&h, &typed), Ok(()));
    }

    #[test]
    fn the_content_type_must_be_exactly_one_json_media_type() {
        let p = parts();
        let b = body(&p.code, &p.public_key, "user", &p.proof);
        let mut cases: Vec<HeaderMap> = vec![HeaderMap::new()];
        for value in [
            "text/plain",
            "application/jsonx",
            "application/json-seq",
            "",
            "json",
        ] {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            cases.push(h);
        }
        let mut twice = json_headers();
        twice.append(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        cases.push(twice);
        for h in cases {
            assert_eq!(shape_of(&h, &b), Err(ShapeError::ContentType), "{h:?}");
        }
    }

    #[test]
    fn the_body_must_be_one_object_of_four_strings() {
        let p = parts();
        let good = body(&p.code, &p.public_key, "user", &p.proof);
        let text = String::from_utf8(good.clone()).unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("not json", b"nope".to_vec()),
            ("array", format!("[{:?},{:?},\"user\",{:?}]", p.code, p.public_key, p.proof).into_bytes()),
            ("string", b"\"x\"".to_vec()),
            ("null", b"null".to_vec()),
            ("trailing", format!("{text} x").into_bytes()),
            ("two values", format!("{text}{text}").into_bytes()),
            ("bom", [b"\xef\xbb\xbf".as_slice(), &good].concat()),
            (
                "missing proof",
                serde_json::json!({"code": p.code, "publicKey": p.public_key, "mode": "user"})
                    .to_string()
                    .into_bytes(),
            ),
            (
                "unknown field",
                serde_json::json!({"code": p.code, "publicKey": p.public_key, "mode": "user",
                    "proof": p.proof, "extra": "x"})
                .to_string()
                .into_bytes(),
            ),
            (
                "duplicate field",
                format!(
                    "{{\"code\":{:?},\"code\":{:?},\"publicKey\":{:?},\"mode\":\"user\",\"proof\":{:?}}}",
                    p.code, p.code, p.public_key, p.proof
                )
                .into_bytes(),
            ),
            (
                "number value",
                format!(
                    "{{\"code\":{:?},\"publicKey\":{:?},\"mode\":1,\"proof\":{:?}}}",
                    p.code, p.public_key, p.proof
                )
                .into_bytes(),
            ),
            (
                "null value",
                format!(
                    "{{\"code\":null,\"publicKey\":{:?},\"mode\":\"user\",\"proof\":{:?}}}",
                    p.public_key, p.proof
                )
                .into_bytes(),
            ),
        ];
        for (name, b) in cases {
            assert_eq!(
                shape_of(&json_headers(), &b),
                Err(ShapeError::Json),
                "{name}"
            );
        }
        // Whitespace after the value is part of a JSON text.
        let spaced = [good.as_slice(), b" \n"].concat();
        assert_eq!(shape_of(&json_headers(), &spaced), Ok(()));
    }

    #[test]
    fn each_field_has_its_own_rule() {
        let p = parts();
        let h = json_headers();
        let check = |code: &str, key: &str, mode: &str, proof: &str, want: ShapeError| {
            assert_eq!(
                shape_of(&h, &body(code, key, mode, proof)),
                Err(want),
                "{code:?} {mode:?} {proof:?}"
            );
        };
        let good_proof = &p.proof;
        // code
        check("", &p.public_key, "user", good_proof, ShapeError::Code);
        check(
            &"a".repeat(65),
            &p.public_key,
            "user",
            good_proof,
            ShapeError::Code,
        );
        check(
            &p.code[1..],
            &p.public_key,
            "user",
            good_proof,
            ShapeError::Code,
        );
        check(
            &format!("{}0", p.code),
            &p.public_key,
            "user",
            good_proof,
            ShapeError::Code,
        );
        // publicKey
        check(&p.code, "", "user", good_proof, ShapeError::PublicKey);
        check(
            &p.code,
            "not a pem",
            "user",
            good_proof,
            ShapeError::PublicKey,
        );
        check(
            &p.code,
            &"A".repeat(513),
            "user",
            good_proof,
            ShapeError::PublicKey,
        );
        let der_less = p.public_key.replace("PUBLIC KEY", "PRIVATE KEY");
        check(
            &p.code,
            &der_less,
            "user",
            good_proof,
            ShapeError::PublicKey,
        );
        // mode
        for mode in ["", "User", "USER", "machine", "user ", " device", "admin"] {
            check(&p.code, &p.public_key, mode, good_proof, ShapeError::Mode);
        }
        // proof
        let short = STANDARD.encode(random_32().unwrap());
        let long = STANDARD.encode(
            [
                random_32().unwrap(),
                random_32().unwrap(),
                random_32().unwrap(),
            ]
            .concat(),
        );
        let unpadded = good_proof.trim_end_matches('=').to_owned();
        let url_safe = good_proof.replace('+', "-").replace('/', "_");
        for proof in [
            "",
            "!!!!",
            short.as_str(),
            long.as_str(),
            unpadded.as_str(),
            &format!("{good_proof}\n"),
            &format!(" {good_proof}"),
            &"A".repeat(129),
        ] {
            check(&p.code, &p.public_key, "user", proof, ShapeError::Proof);
        }
        // The URL-safe alphabet differs only when the signature has '+' or '/'; the strict
        // decoder refuses those characters outright.
        if url_safe != *good_proof {
            check(&p.code, &p.public_key, "user", &url_safe, ShapeError::Proof);
        }
        assert_eq!(
            shape_of(&h, &body(&p.code, &p.public_key, "device", good_proof)),
            Ok(())
        );
    }

    #[test]
    fn a_64_byte_value_that_is_no_signature_is_not_a_shape_error() {
        let p = parts();
        // All-zero r and s is no valid signature; the shape rules accept 64 bytes.
        let zero = STANDARD.encode([0u8; 64]);
        let shaped = shape(
            &json_headers(),
            &body(&p.code, &p.public_key, "user", &zero),
        )
        .expect("shape passes");
        assert!(!proof_holds(&shaped, random_32().unwrap()));
    }

    #[test]
    fn the_proof_covers_the_key_mode_code_and_server_key_id() {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let code = RegistrationCode::generate().unwrap();
        let server = random_32().unwrap();
        let sign = |code: &RegistrationCode, mode, server| {
            sign_registration(
                &key,
                &RegistrationTranscript::new(code, &public, mode, server),
            )
            .unwrap()
            .to_bytes()
            .to_vec()
        };
        let shaped = |code: RegistrationCode, mode, proof| Shaped {
            code,
            key: public,
            mode,
            proof,
        };
        let good = sign(&code, Mode::User, server);
        assert!(proof_holds(
            &shaped(code.clone(), Mode::User, good.clone()),
            server
        ));
        assert!(!proof_holds(
            &shaped(code.clone(), Mode::Device, good.clone()),
            server
        ));
        assert!(!proof_holds(
            &shaped(code.clone(), Mode::User, good.clone()),
            random_32().unwrap()
        ));
        let other = RegistrationCode::generate().unwrap();
        assert!(!proof_holds(&shaped(other, Mode::User, good), server));
        let stranger = SigningKey::generate().unwrap();
        let theirs = sign_registration(
            &stranger,
            &RegistrationTranscript::new(&code, &public, Mode::User, server),
        )
        .unwrap()
        .to_bytes()
        .to_vec();
        assert!(!proof_holds(&shaped(code, Mode::User, theirs), server));
    }

    // ----- the log gates

    #[test]
    fn each_reason_has_its_own_gate_of_sixty_seconds() {
        let gates = Gates::new();
        let t0 = Instant::now();
        let all = [
            Reason::Proof,
            Reason::Absent,
            Reason::Revoked,
            Reason::Used,
            Reason::Expired,
            Reason::OtherKey,
            Reason::InstructionKey,
            Reason::Owner,
            Reason::StateChanged,
        ];
        assert_eq!(all.len(), Reason::COUNT);
        let words: std::collections::HashSet<_> = all.iter().map(|r| r.word()).collect();
        assert_eq!(words.len(), Reason::COUNT, "one word per reason");
        for reason in all {
            assert!(gates.allow(reason as usize, t0), "{}", reason.word());
        }
        assert!(gates.allow(SLOT_UNAVAILABLE, t0));
        assert!(gates.allow(SLOT_INTERNAL, t0));
        // A flood of one reason is silent, and does not silence another.
        assert!(!gates.allow(Reason::Used as usize, t0 + secs(59)));
        assert!(!gates.allow(SLOT_UNAVAILABLE, t0 + secs(59)));
        assert!(gates.allow(Reason::Used as usize, t0 + secs(61)));
        assert!(!gates.allow(Reason::Used as usize, t0 + secs(62)));
        assert!(gates.allow(Reason::Absent as usize, t0 + secs(62)));
    }

    #[test]
    fn two_registrars_do_not_share_a_limiter_or_gates() {
        let a = Gates::new();
        let b = Gates::new();
        let now = Instant::now();
        assert!(a.allow(0, now));
        assert!(b.allow(0, now));
        let (x, y) = (Limiter::default(), Limiter::default());
        for _ in 0..10 {
            x.refusal(now);
        }
        assert_eq!(y.refusal(now), Admission::Counted);
    }
}

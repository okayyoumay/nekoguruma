//! `POST /api/v1/jobs`: the creation of a job (design 4.4, 5.1, 12; ADR-477). The delivery of the
//! stored instruction is the queue's ([`crate::job_queue`]).
//!
//! A request is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the session layer (403 for the Origin, 401, 503, 500), the
//! body (at most [`MAX_BODY`] bytes, 413; within 5 seconds, 408; broken, 400), the Content-Type,
//! the idempotency key and the shape of the JSON (400, with no database access), then one
//! transaction: the key's row (a replay or a conflict answers here, before any permission is
//! judged), the caller's permission (403), the bounds of a `startDeadline` against the issue time
//! (422, ADR-487), the active instruction key (503), the kind's confirmation level from
//! `confirmation_defaults` (500 without a row, ADR-488), the signing of the instruction and the
//! insert, which loses to a concurrent insert of the same key and then answers like the first
//! read. A 400 means the request can never be valid, a 422 that the key belongs to another request
//! or that the deadline lies outside the bounds. A job's ID is made here, never taken from the client.
//!
//! What the 403 refused goes to the log only, through a gate per reason. Never logged: the
//! body, the idempotency key, the VCI ID and the VIN.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Extension, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use shared_crypto::random_32;
use shared_proto::{
    AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId, Vin, is_canonical_uuid,
    is_well_formed_vin,
};
use sqlx::{PgConnection, PgPool};

use crate::agent_events::epoch_seconds;
use crate::audit;
use crate::db;
use crate::health::may_log;
use crate::instruction_key::InstructionKey;
use crate::operator_session::SessionOperator;
use crate::problem::Problem;
use crate::register::{bounded_body_within, is_json};

/// The route's path.
pub const PATH: &str = "/api/v1/jobs";
/// The largest body accepted, in bytes; exactly this many pass.
pub const MAX_BODY: usize = 16 * 1024;
/// The longest idempotency key, in bytes.
pub const MAX_KEY: usize = 128;
/// The longest VCI ID, in bytes: the instruction's string bound.
pub const MAX_VCI: usize = 1024;
/// The most IR documents one job names.
pub const MAX_DIGESTS: usize = 128;
/// How long after its issue a job may still be started (design 5.3), in seconds.
pub const START_WINDOW_SECS: u64 = 900;
/// The least a request's `startDeadline` may lie after the job's issue time, in seconds
/// (design 5.3, ADR-487), inclusive.
pub const MIN_START_SECS: i64 = 60;
/// The most a request's `startDeadline` may lie after the job's issue time, in seconds (24
/// hours), inclusive.
pub const MAX_START_SECS: i64 = 86_400;
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// The job instruction layout this route writes, and the ownership generation of a new job
/// (ADR-229).
const FIRST_GENERATION: u64 = 1;
/// The domain tag at the start of a request's hash.
const HASH_TAG: &[u8] = b"ngr-job-request-v1";

// ---------------------------------------------------------------------------------------------
// The shape of a request

/// Which rule of the request failed before the database was asked. Each is a 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeError {
    ContentType,
    Key,
    Json,
    Kind,
    KindUnsupported,
    AgentId,
    VciId,
    Vin,
    IrDigests,
    ArtifactDigest,
    StartDeadline,
    Unsupported,
}

impl ShapeError {
    fn title(self) -> &'static str {
        match self {
            ShapeError::ContentType => "Content-Type must be application/json",
            ShapeError::Key => "Idempotency-Key must be one header of 1 to 128 visible ASCII bytes",
            ShapeError::Json => "The body is not the expected JSON object",
            ShapeError::Kind => "Invalid kind",
            ShapeError::KindUnsupported => "This kind of job is not supported",
            ShapeError::AgentId => "Invalid agentId",
            ShapeError::VciId => "Invalid vciId",
            ShapeError::Vin => "Invalid vin",
            ShapeError::IrDigests => "Invalid irDigests",
            ShapeError::ArtifactDigest => "Invalid artifactDigest",
            ShapeError::StartDeadline => "Invalid startDeadline",
            ShapeError::Unsupported => "A property of the request is not supported",
        }
    }
}

/// The kinds this route creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Acquire,
    WriteSettings,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Acquire => "acquire",
            Kind::WriteSettings => "writeSettings",
        }
    }

    fn job_kind(self) -> JobKind {
        match self {
            Kind::Acquire => JobKind::Acquire,
            Kind::WriteSettings => JobKind::WriteSettings,
        }
    }

    /// How long the job may hold its vehicle's lock after its start deadline (design 8.8): the
    /// run allowance of the kind. Each kind that this route learns to create adds its arm here.
    fn lock_secs(self) -> u64 {
        match self {
            Kind::Acquire => 600,
            Kind::WriteSettings => 1800,
        }
    }
}

/// A request that passed the shape rules, normalized: the digests are sorted and unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub agent_id: String,
    pub vci_id: String,
    pub vin: Option<String>,
    pub hashes: Vec<[u8; 32]>,
    pub artifact: Option<[u8; 32]>,
    /// The request's `startDeadline` as seconds since the epoch (UTC, the fraction floored), when
    /// it has one; the bounds against the issue time are judged in the transaction.
    pub start_deadline: Option<i64>,
}

impl Spec {
    /// The SHA-256 of the normalized request, over a tagged, length-prefixed encoding in a fixed
    /// order: the kind, the agent, the VCI, the VIN (presence, then value), the document hashes
    /// (count, then each) and the artifact (presence, then value). The operator is not in it:
    /// the row's own operator is compared. A property a later change adds enters the encoding
    /// only when the request has it, so a hash made before it stays valid: the start deadline
    /// (ADR-487) follows the artifact as a named field, its normalized seconds, so a request with
    /// no deadline and one that names the default are different requests.
    pub fn request_hash(&self) -> [u8; 32] {
        fn put(hasher: &mut Sha256, bytes: &[u8]) {
            hasher.update((bytes.len() as u32).to_be_bytes());
            hasher.update(bytes);
        }
        let mut hasher = Sha256::new();
        put(&mut hasher, HASH_TAG);
        put(&mut hasher, self.kind.name().as_bytes());
        put(&mut hasher, self.agent_id.as_bytes());
        put(&mut hasher, self.vci_id.as_bytes());
        match &self.vin {
            Some(vin) => {
                hasher.update([1]);
                put(&mut hasher, vin.as_bytes());
            }
            None => hasher.update([0]),
        }
        hasher.update((self.hashes.len() as u32).to_be_bytes());
        for hash in &self.hashes {
            hasher.update(hash);
        }
        match &self.artifact {
            Some(hash) => {
                hasher.update([1]);
                hasher.update(hash);
            }
            None => hasher.update([0]),
        }
        if let Some(deadline) = self.start_deadline {
            put(&mut hasher, b"startDeadline");
            hasher.update(deadline.to_be_bytes());
        }
        hasher.finalize().into()
    }
}

/// The eleven properties of a `JobRequest`, as typed; nothing is judged yet.
#[derive(Default)]
struct Raw {
    kind: Option<String>,
    agent_id: Option<String>,
    vci_id: Option<String>,
    vin: Option<String>,
    ir_digests: Option<Vec<String>>,
    artifact_digest: Option<String>,
    parameters: Option<Map<String, Value>>,
    start_deadline: Option<String>,
    execution_window: Option<Map<String, Value>>,
    remote: Option<bool>,
    on_site_contact: Option<String>,
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Raw, D::Error> {
        struct RawVisitor;

        impl<'de> Visitor<'de> for RawVisitor {
            type Value = Raw;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a job request object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
                fn put<T, E: de::Error>(slot: &mut Option<T>, value: T) -> Result<(), E> {
                    match slot.replace(value) {
                        None => Ok(()),
                        Some(_) => Err(E::custom("duplicate property")),
                    }
                }
                let mut raw = Raw::default();
                while let Some(name) = map.next_key::<String>()? {
                    match name.as_str() {
                        "kind" => put(&mut raw.kind, map.next_value()?)?,
                        "agentId" => put(&mut raw.agent_id, map.next_value()?)?,
                        "vciId" => put(&mut raw.vci_id, map.next_value()?)?,
                        "vin" => put(&mut raw.vin, map.next_value()?)?,
                        "irDigests" => put(&mut raw.ir_digests, map.next_value()?)?,
                        "artifactDigest" => put(&mut raw.artifact_digest, map.next_value()?)?,
                        "parameters" => put(&mut raw.parameters, map.next_value()?)?,
                        "startDeadline" => put(&mut raw.start_deadline, map.next_value()?)?,
                        "executionWindow" => put(&mut raw.execution_window, map.next_value()?)?,
                        "remote" => put(&mut raw.remote, map.next_value()?)?,
                        "onSiteContact" => put(&mut raw.on_site_contact, map.next_value()?)?,
                        _ => return Err(de::Error::custom("unknown property")),
                    }
                }
                Ok(raw)
            }
        }

        deserializer.deserialize_map(RawVisitor)
    }
}

/// Reads the body as one object of the known properties, each at most once and none null, with
/// nothing after it.
fn parse(body: &[u8]) -> Option<Raw> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let raw = Raw::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(raw)
}

/// 64 lowercase hex digits as 32 bytes.
fn parse_digest(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks(2)) {
        *slot = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The Idempotency-Key header: exactly one, 1 to [`MAX_KEY`] bytes, each a visible ASCII
/// character. It is compared as bytes, so a case difference is another key.
fn idempotency_key(headers: &HeaderMap) -> Result<String, ShapeError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(ShapeError::Key);
    };
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_KEY || !bytes.iter().all(|b| (0x21..=0x7e).contains(b))
    {
        return Err(ShapeError::Key);
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// The rules on the values, in order. Properties the route does not support are refused last.
fn judge(raw: Raw) -> Result<Spec, ShapeError> {
    let kind = match raw.kind.as_deref() {
        Some("acquire") => Kind::Acquire,
        Some("writeSettings") => Kind::WriteSettings,
        Some("monitorSession" | "monitorCapture" | "runSequence" | "reprogram" | "identify") => {
            return Err(ShapeError::KindUnsupported);
        }
        _ => return Err(ShapeError::Kind),
    };
    let agent_id = raw
        .agent_id
        .filter(|id| is_canonical_uuid(id))
        .ok_or(ShapeError::AgentId)?;
    let vci_id = raw
        .vci_id
        .filter(|id| (1..=MAX_VCI).contains(&id.len()) && !id.chars().any(char::is_control))
        .ok_or(ShapeError::VciId)?;
    if raw
        .vin
        .as_deref()
        .is_some_and(|vin| !is_well_formed_vin(vin))
    {
        return Err(ShapeError::Vin);
    }
    let texts = raw.ir_digests.ok_or(ShapeError::IrDigests)?;
    if !(1..=MAX_DIGESTS).contains(&texts.len()) {
        return Err(ShapeError::IrDigests);
    }
    let mut hashes = texts
        .iter()
        .map(|text| parse_digest(text))
        .collect::<Option<Vec<_>>>()
        .ok_or(ShapeError::IrDigests)?;
    hashes.sort_unstable();
    hashes.dedup();
    let artifact = match raw.artifact_digest {
        None => None,
        Some(text) => Some(parse_digest(&text).ok_or(ShapeError::ArtifactDigest)?),
    };
    let start_deadline = match raw.start_deadline {
        None => None,
        Some(text) => Some(epoch_seconds(&text).ok_or(ShapeError::StartDeadline)?),
    };
    if raw.remote == Some(true)
        || raw
            .parameters
            .is_some_and(|parameters| !parameters.is_empty())
        || raw.execution_window.is_some()
        || raw.on_site_contact.is_some()
    {
        return Err(ShapeError::Unsupported);
    }
    Ok(Spec {
        kind,
        agent_id,
        vci_id,
        vin: raw.vin,
        hashes,
        artifact,
        start_deadline,
    })
}

/// Steps 3 to 6 of the order: the Content-Type, the idempotency key and the body. The body is
/// already read within its bounds.
pub fn shape(headers: &HeaderMap, body: &[u8]) -> Result<(String, Spec), ShapeError> {
    if !is_json(headers) {
        return Err(ShapeError::ContentType);
    }
    let key = idempotency_key(headers)?;
    let spec = judge(parse(body).ok_or(ShapeError::Json)?)?;
    Ok((key, spec))
}

/// The hash of the request a body stands for, when the body passes the shape rules. Tests only.
#[doc(hidden)]
pub fn body_hash(body: &[u8]) -> Option<[u8; 32]> {
    Some(judge(parse(body)?).ok()?.request_hash())
}

// ---------------------------------------------------------------------------------------------
// The job

/// The UUIDv7 of a job from the database's clock in milliseconds and ten random bytes: the
/// 48-bit time, the version, the variant and 74 random bits.
pub fn job_id(ms: u64, random: [u8; 10]) -> String {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (random[0] & 0x0f);
    b[7] = random[1];
    b[8] = 0x80 | (random[2] & 0x3f);
    b[9..].copy_from_slice(&random[3..]);
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// The instruction of a new job (design 11.1): version 1, generation 1, no plan, issued at
/// `issued_at` and startable until the request's deadline, or for [`START_WINDOW_SECS`] after the
/// issue time when the request names none. The caller has judged the request's deadline against
/// the bounds ([`deadline_in_bounds`]).
pub fn instruction(id: &str, operator_id: &str, spec: &Spec, issued_at: u64) -> JobInstruction {
    JobInstruction {
        version: INSTRUCTION_VERSION,
        job_id: JobId(id.to_owned()),
        ownership_generation: FIRST_GENERATION,
        agent_id: AgentId(spec.agent_id.clone()),
        operator_id: operator_id.to_owned(),
        kind: spec.kind.job_kind(),
        vci: VciId(spec.vci_id.clone()),
        vin: spec.vin.clone().map(Vin),
        document_hashes: spec.hashes.clone(),
        artifact_hash: spec.artifact,
        plan_hash: None,
        issued_at,
        start_deadline: match spec.start_deadline {
            // The bounds are judged before signing (`deadline_in_bounds`), so a request's deadline
            // is never negative here.
            Some(deadline) => {
                debug_assert!(deadline >= 0, "a deadline is checked before it is signed");
                u64::try_from(deadline).unwrap_or(issued_at + START_WINDOW_SECS)
            }
            None => issued_at + START_WINDOW_SECS,
        },
    }
}

/// Whether a request's deadline lies between [`MIN_START_SECS`] and [`MAX_START_SECS`] after the
/// issue time, both inclusive (ADR-487). A request with no deadline is always within bounds.
fn deadline_in_bounds(deadline: Option<i64>, issued_at: u64) -> bool {
    let Some(deadline) = deadline else {
        return true;
    };
    i64::try_from(issued_at)
        .ok()
        .and_then(|issued| deadline.checked_sub(issued))
        .is_some_and(|after| (MIN_START_SECS..=MAX_START_SECS).contains(&after))
}

/// The job columns every read and the insert return, in the order of [`Stored`].
const JOB_COLUMNS: &str = "id::text, kind, state, progress_permille, confirmation_level, \
     operator_id::text, request_hash, failure_code, procedure_code";

type StoredRow = (
    String,
    String,
    String,
    i16,
    String,
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
);

/// A job row, as far as the answer and the key's decision need it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    id: String,
    kind: String,
    state: String,
    progress_permille: i16,
    level: String,
    operator_id: String,
    request_hash: Option<Vec<u8>>,
    /// The failure code of a job that ended as failed (design 5.6, ADR-481).
    failure_code: Option<String>,
    /// The procedure's own code, with the failure code `procedure`.
    procedure_code: Option<String>,
}

impl From<StoredRow> for Stored {
    fn from(row: StoredRow) -> Stored {
        Stored {
            id: row.0,
            kind: row.1,
            state: row.2,
            progress_permille: row.3,
            level: row.4,
            operator_id: row.5,
            request_hash: row.6,
            failure_code: row.7,
            procedure_code: row.8,
        }
    }
}

impl Stored {
    /// The `Job` document, the same for a creation and a replay (a replay shows the current
    /// state). A job with a failure code shows `failure`: the code and, with the code
    /// `procedure`, the procedure's own. Never the failure's detail, which is vehicle data a
    /// replay would show without the result's permission (ADR-477 item 5, ADR-481).
    fn document(&self) -> Value {
        let mut document = serde_json::json!({
            "jobId": self.id,
            "kind": self.kind,
            "state": self.state,
            "progressPermille": self.progress_permille,
            "confirmation": { "level": self.level },
        });
        if let Some(code) = &self.failure_code {
            let mut failure = serde_json::json!({ "code": code });
            if let Some(procedure) = &self.procedure_code {
                failure["procedureCode"] = Value::from(procedure.as_str());
            }
            document["failure"] = failure;
        }
        document
    }
}

/// Whether a request that finds its key taken replays the stored job: the same operator and an
/// equal request hash. A row without a hash replays for nobody.
fn replays(row_operator: &str, row_hash: Option<&[u8]>, caller: &str, hash: &[u8; 32]) -> bool {
    row_operator == caller && row_hash == Some(&hash[..])
}

// ---------------------------------------------------------------------------------------------
// Permission

/// What the permission read returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Facts {
    /// The caller holds the role `worker` with an empty scope.
    worker: bool,
    /// An agent of the caller's tenant has the ID.
    agent_found: bool,
    agent_unrevoked: bool,
    /// The agent's allow-list names the caller.
    allowed: bool,
}

/// Why a request was refused permission; the log's word for it, never the client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denial {
    NotWorker,
    AgentUnknown,
    AgentRevoked,
    NotAllowed,
}

impl Denial {
    fn word(self) -> &'static str {
        match self {
            Denial::NotWorker => "not_worker",
            Denial::AgentUnknown => "agent_unknown",
            Denial::AgentRevoked => "agent_revoked",
            Denial::NotAllowed => "not_allowed",
        }
    }
}

/// The role, then the agent, then the allow-list: the first that fails is the reason.
fn authorize(facts: Facts) -> Result<(), Denial> {
    if !facts.worker {
        Err(Denial::NotWorker)
    } else if !facts.agent_found {
        Err(Denial::AgentUnknown)
    } else if !facts.agent_unrevoked {
        Err(Denial::AgentRevoked)
    } else if !facts.allowed {
        Err(Denial::NotAllowed)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The route

/// The log gates of one router: one per refusal reason, one for an instruction key that is not
/// the active one, one each for the unavailable (503) and the internal (500) failure, and one
/// for a refusal because the vehicle is locked (409).
struct Gates {
    slots: [Mutex<Option<Instant>>; 8],
}

const SLOT_KEY_INACTIVE: usize = 4;
const SLOT_UNAVAILABLE: usize = 5;
const SLOT_INTERNAL: usize = 6;
const SLOT_LOCKED: usize = 7;

impl Gates {
    fn allow(&self, slot: usize, now: Instant) -> bool {
        may_log(&self.slots[slot], now, LOG_EVERY)
    }
}

/// The route's state: the pool, the key and the log gates of this router.
#[derive(Clone)]
pub struct Jobs(Arc<Inner>);

struct Inner {
    pool: PgPool,
    key: Arc<InstructionKey>,
    gates: Gates,
}

impl Jobs {
    pub fn new(pool: PgPool, key: Arc<InstructionKey>) -> Jobs {
        Jobs(Arc::new(Inner {
            pool,
            key,
            gates: Gates {
                slots: std::array::from_fn(|_| Mutex::new(None)),
            },
        }))
    }
}

/// What the transaction decided.
enum Outcome {
    Created(Stored),
    Replayed(Stored),
    /// The key belongs to another request or operator.
    Conflict,
    Denied(Denial),
    /// The process's key is not the active row of `instruction_keys`.
    KeyNotActive,
    /// Another job holds the vehicle's lock (design 8.8); nothing was written.
    VehicleLocked,
    /// The request's `startDeadline` is not between 1 minute and 24 hours after the issue time
    /// (ADR-487); nothing was written.
    DeadlineOutOfBounds,
}

/// What a failed creation is.
pub(crate) enum JobError {
    /// The pool, the connection or a statement failed, or timed out.
    Unavailable(String),
    /// The signer, the random source or the database client failed.
    Internal(String),
}

impl From<sqlx::Error> for JobError {
    fn from(error: sqlx::Error) -> Self {
        match db::failure(&error) {
            db::Failure::Unavailable(what) => JobError::Unavailable(what),
            db::Failure::Internal(what) => JobError::Internal(what),
        }
    }
}

/// `POST /api/v1/jobs`: 202 with the `Job`, which a replay of the same request repeats.
pub async fn create(
    State(jobs): State<Jobs>,
    Extension(operator): Extension<SessionOperator>,
    request: Request,
) -> Response {
    let inner = &*jobs.0;
    let (parts, body) = request.into_parts();
    let bytes = match bounded_body_within(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let (key, spec) = match shape(&parts.headers, &bytes) {
        Ok(shaped) => shaped,
        Err(error) => return Problem::new(StatusCode::BAD_REQUEST, error.title()).into_response(),
    };
    let now = Instant::now();
    match inner.run(&operator, &key, &spec).await {
        Ok(Outcome::Created(job)) => {
            tracing::info!(job_id = %job.id, operator_id = %operator.operator_id, "job created");
            accepted(&job)
        }
        Ok(Outcome::Replayed(job)) => accepted(&job),
        Ok(Outcome::Conflict) => Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The idempotency key belongs to another request",
        )
        .into_response(),
        Ok(Outcome::Denied(reason)) => {
            if inner.gates.allow(reason as usize, now) {
                tracing::warn!(
                    reason = reason.word(),
                    operator_id = %operator.operator_id,
                    "job refused"
                );
            }
            Problem::new(StatusCode::FORBIDDEN, "Not permitted to create this job").into_response()
        }
        Ok(Outcome::VehicleLocked) => {
            // The refusal is logged without the VIN or the holder.
            if inner.gates.allow(SLOT_LOCKED, now) {
                tracing::info!(
                    operator_id = %operator.operator_id,
                    "job refused: the vehicle is locked"
                );
            }
            Problem::with_code(
                StatusCode::CONFLICT,
                "Another job holds this vehicle",
                "VEHICLE_LOCKED",
            )
            .into_response()
        }
        Ok(Outcome::DeadlineOutOfBounds) => Problem::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "startDeadline must be 1 minute to 24 hours after the issue time",
            "START_DEADLINE_OUT_OF_BOUNDS",
        )
        .into_response(),
        Ok(Outcome::KeyNotActive) => {
            if inner.gates.allow(SLOT_KEY_INACTIVE, now) {
                tracing::error!("the instruction key is not the active key; no job is created");
            }
            unavailable()
        }
        Err(JobError::Unavailable(what)) => {
            if inner.gates.allow(SLOT_UNAVAILABLE, now) {
                tracing::warn!(error = %what, "job creation unavailable");
            }
            unavailable()
        }
        Err(JobError::Internal(what)) => {
            if inner.gates.allow(SLOT_INTERNAL, now) {
                tracing::error!(error = %what, "job creation failed");
            }
            Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "Job creation failed").into_response()
        }
    }
}

fn unavailable() -> Response {
    Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Job creation unavailable").into_response()
}

fn accepted(job: &Stored) -> Response {
    let mut response = (StatusCode::ACCEPTED, Json(job.document())).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

impl Inner {
    /// The transaction (step 7 of the order). Read committed is stated, not assumed: the
    /// re-read after a lost insert must see the winner's row. Any early return drops the
    /// transaction, which rolls back.
    async fn run(
        &self,
        operator: &SessionOperator,
        key: &str,
        spec: &Spec,
    ) -> Result<Outcome, JobError> {
        let hash = spec.request_hash();
        let mut tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await?;
        db::statement_timeouts(&mut tx).await?;

        // The key is judged before the permission: a replay is of a job this operator created.
        if let Some(stored) = read_by_key(&mut tx, &operator.tenant_id, key).await? {
            return Ok(decide(stored, &operator.operator_id, &hash));
        }

        let key_id = self.key.key_id();
        let (facts, key_active, now_ms): (Facts, bool, i64) =
            self.permission(&mut tx, operator, spec, &key_id).await?;
        if let Err(reason) = authorize(facts) {
            return Ok(Outcome::Denied(reason));
        }
        // The deadline's bounds are judged against the database's time of the permission read,
        // after the permission and before the key (ADR-487): a refused caller learns nothing
        // about them.
        let now_ms = u64::try_from(now_ms).map_err(|_| JobError::Internal("clock".to_owned()))?;
        let issued_at = now_ms / 1000;
        if !deadline_in_bounds(spec.start_deadline, issued_at) {
            return Ok(Outcome::DeadlineOutOfBounds);
        }
        if !key_active {
            return Ok(Outcome::KeyNotActive);
        }
        // The kind's confirmation level (design 6.3), read from the product-wide defaults in the
        // creating transaction (ADR-488), before anything is
        // signed. A `gracePeriod` is recorded and not enforced. A kind
        // without a row is a fault of the installation, not a refusal of the request: it fails
        // the creation as 500, and the transaction's end leaves nothing inserted.
        let level: Option<(String,)> =
            sqlx::query_as("SELECT level FROM confirmation_defaults WHERE kind = $1")
                .bind(spec.kind.name())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((level,)) = level else {
            return Err(JobError::Internal(format!(
                "no confirmation default for kind {}",
                spec.kind.name()
            )));
        };

        let random = random_32().map_err(|e| JobError::Internal(e.to_string()))?;
        let mut tail = [0u8; 10];
        tail.copy_from_slice(&random[..10]);
        let id = job_id(now_ms, tail);
        let instruction = instruction(&id, &operator.operator_id, spec, issued_at);
        let sealed = self
            .key
            .seal(&instruction)
            .await
            .map_err(|e| JobError::Internal(e.to_string()))?;

        let digests: Vec<String> = spec.hashes.iter().map(|h| hex(h)).collect();
        let inserted: Option<StoredRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, vin, \
                 ir_digests, artifact_digest, confirmation_level, start_deadline, signed_job, \
                 signing_key_id, idempotency_key, request_hash) \
             VALUES ($1::uuid, $2::uuid, $3, $4::uuid, $5::uuid, $6, $7, $8, $9, $10, \
                 to_timestamp($11::bigint::float8), $12, $13, $14, $15) \
             ON CONFLICT (tenant_id, idempotency_key) DO NOTHING RETURNING {JOB_COLUMNS}"
        )))
        .bind(&operator.tenant_id)
        .bind(&id)
        .bind(spec.kind.name())
        .bind(&spec.agent_id)
        .bind(&operator.operator_id)
        .bind(&spec.vci_id)
        .bind(&spec.vin)
        .bind(&digests)
        .bind(spec.artifact.map(|h| hex(&h)))
        .bind(&level)
        .bind(instruction.start_deadline as i64)
        .bind(&sealed)
        .bind(&key_id)
        .bind(key)
        .bind(&hash[..])
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = inserted {
            // The creation's audit row (design 8.2.3, ADR-484), the first statement of the arm
            // that won the insert: a replay and every refusal below write none, since each rolls
            // this transaction back. A failed insert fails the creation.
            audit::job_created(&mut tx, &operator.tenant_id, &id).await?;
            // The vehicle's lock (design 8.8), taken by the creation that won the insert: a
            // replay never reaches here. The replace condition reads only the lock row, so a
            // concurrent take of the same VIN waits for this transaction and re-checks the row
            // it left. A job without a VIN takes none.
            if let Some(vin) = &spec.vin {
                let lock_secs = spec.kind.lock_secs() as f64;
                let taken: Option<(i32,)> = sqlx::query_as(
                    "INSERT INTO vehicle_locks AS l (tenant_id, vin, job_id, agent_id, expires_at) \
                     VALUES ($1::uuid, $2, $3::uuid, $4::uuid, \
                         to_timestamp($5::bigint::float8) + make_interval(secs => $6::float8)) \
                     ON CONFLICT (tenant_id, vin) DO UPDATE SET job_id = EXCLUDED.job_id, \
                         agent_id = EXCLUDED.agent_id, acquired_at = now(), \
                         expires_at = EXCLUDED.expires_at \
                     WHERE l.expires_at <= now() RETURNING 1",
                )
                .bind(&operator.tenant_id)
                .bind(vin)
                .bind(&id)
                .bind(&spec.agent_id)
                .bind(instruction.start_deadline as i64)
                .bind(lock_secs)
                .fetch_optional(&mut *tx)
                .await?;
                if taken.is_none() {
                    return Ok(Outcome::VehicleLocked);
                }
            }
            // Hold the agent's row and the active key's row shared until the commit, taken after
            // the insert so that an insert waiting behind another insert of the same key holds
            // neither: a revocation or a retirement then waits for this commit, or has committed
            // before these reads, and is seen here. The agent is locked first, then the key.
            let agent_locked: Option<(i32,)> = sqlx::query_as(
                "SELECT 1 FROM agents WHERE id = $1::uuid AND tenant_id = $2::uuid \
                   AND revoked_at IS NULL FOR SHARE",
            )
            .bind(&spec.agent_id)
            .bind(&operator.tenant_id)
            .fetch_optional(&mut *tx)
            .await?;
            if agent_locked.is_none() {
                return Ok(Outcome::Denied(Denial::AgentRevoked));
            }
            let key_locked: Option<(i32,)> = sqlx::query_as(
                "SELECT 1 FROM instruction_keys WHERE key_id = $1 AND retired_at IS NULL \
                 FOR SHARE",
            )
            .bind(&key_id)
            .fetch_optional(&mut *tx)
            .await?;
            if key_locked.is_none() {
                return Ok(Outcome::KeyNotActive);
            }
            tx.commit().await?;
            return Ok(Outcome::Created(row.into()));
        }

        // Another insert of the key won: its row is committed now, and this statement sees it.
        match read_by_key(&mut tx, &operator.tenant_id, key).await? {
            Some(stored) => Ok(decide(stored, &operator.operator_id, &hash)),
            None => Err(JobError::Unavailable("the job row is gone".to_owned())),
        }
    }

    /// One read of everything permission needs, and of the database's clock.
    async fn permission(
        &self,
        tx: &mut PgConnection,
        operator: &SessionOperator,
        spec: &Spec,
        key_id: &str,
    ) -> Result<(Facts, bool, i64), sqlx::Error> {
        let (worker, found, unrevoked, allowed, active, now_ms): (
            bool,
            bool,
            bool,
            bool,
            bool,
            i64,
        ) = sqlx::query_as(concat!(
            "SELECT ",
            db::worker_unscoped!(),
            ", \
               EXISTS (SELECT 1 FROM agents a WHERE a.id = $3::uuid AND a.tenant_id = $1::uuid), \
               EXISTS (SELECT 1 FROM agents a WHERE a.id = $3::uuid AND a.tenant_id = $1::uuid \
                       AND a.revoked_at IS NULL), \
               EXISTS (SELECT 1 FROM agent_allowed_operators l WHERE l.tenant_id = $1::uuid \
                       AND l.agent_id = $3::uuid AND l.operator_id = $2::uuid), \
               EXISTS (SELECT 1 FROM instruction_keys k WHERE k.key_id = $4 \
                       AND k.retired_at IS NULL), \
               floor(extract(epoch FROM now()) * 1000)::bigint"
        ))
        .bind(&operator.tenant_id)
        .bind(&operator.operator_id)
        .bind(&spec.agent_id)
        .bind(key_id)
        .fetch_one(tx)
        .await?;
        Ok((
            Facts {
                worker,
                agent_found: found,
                agent_unrevoked: unrevoked,
                allowed,
            },
            active,
            now_ms,
        ))
    }
}

async fn read_by_key(
    tx: &mut PgConnection,
    tenant_id: &str,
    key: &str,
) -> Result<Option<Stored>, sqlx::Error> {
    let row: Option<StoredRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE tenant_id = $1::uuid AND idempotency_key = $2"
    )))
    .bind(tenant_id)
    .bind(key)
    .fetch_optional(tx)
    .await?;
    Ok(row.map(Stored::from))
}

/// The answer for a key that is taken: the stored job for the same operator and request, else a
/// conflict that carries no job data.
fn decide(stored: Stored, caller: &str, hash: &[u8; 32]) -> Outcome {
    if replays(
        &stored.operator_id,
        stored.request_hash.as_deref(),
        caller,
        hash,
    ) {
        Outcome::Replayed(stored)
    } else {
        Outcome::Conflict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use shared_crypto::SigningKey;

    const AGENT: &str = "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab";
    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const VIN: &str = "1HGCM82633A004352";

    fn headers(content_type: &[&str], keys: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in content_type {
            headers.append(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
        }
        for value in keys {
            headers.append("idempotency-key", HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn good_headers() -> HeaderMap {
        headers(&["application/json"], &["key-1"])
    }

    fn body(extra: Value) -> Vec<u8> {
        let mut object = serde_json::json!({
            "kind": "acquire",
            "agentId": AGENT,
            "vciId": "vci-1",
            "irDigests": [DIGEST_A],
        });
        for (name, value) in extra.as_object().unwrap() {
            if value.is_null() {
                object.as_object_mut().unwrap().remove(name);
            } else {
                object[name] = value.clone();
            }
        }
        object.to_string().into_bytes()
    }

    fn shaped(extra: Value) -> Result<Spec, ShapeError> {
        shape(&good_headers(), &body(extra)).map(|(_, spec)| spec)
    }

    fn shaped_raw(bytes: &[u8]) -> Result<Spec, ShapeError> {
        shape(&good_headers(), bytes).map(|(_, spec)| spec)
    }

    // R2
    #[test]
    fn the_content_type_must_be_exactly_one_json_media_type() {
        let ok = |h: &HeaderMap| shape(h, &body(serde_json::json!({}))).map(|_| ());
        assert_eq!(ok(&headers(&["application/json"], &["k"])), Ok(()));
        assert_eq!(
            ok(&headers(&["application/json; charset=utf-8"], &["k"])),
            Ok(())
        );
        for content_type in [
            vec![],
            vec!["text/plain"],
            vec!["application/json", "application/json"],
        ] {
            assert_eq!(
                ok(&headers(&content_type, &["k"])),
                Err(ShapeError::ContentType),
                "{content_type:?}"
            );
        }
    }

    // R3
    #[test]
    fn the_idempotency_key_is_one_header_of_one_to_128_visible_bytes_compared_as_bytes() {
        let key_of = |keys: &[&str]| {
            shape(
                &headers(&["application/json"], keys),
                &body(serde_json::json!({})),
            )
            .map(|(key, _)| key)
        };
        assert_eq!(key_of(&["k"]).as_deref(), Ok("k"));
        let longest = "k".repeat(128);
        assert_eq!(key_of(&[&longest]), Ok(longest.clone()));
        assert_eq!(key_of(&["~!"]).as_deref(), Ok("~!"));
        assert_eq!(key_of(&["Key"]).as_deref(), Ok("Key"));
        assert_ne!(key_of(&["Key"]), key_of(&["key"]));
        let too_long = "k".repeat(129);
        for keys in [
            vec![],
            vec![""],
            vec!["a", "a"],
            vec![too_long.as_str()],
            vec!["a b"],
        ] {
            assert_eq!(key_of(&keys), Err(ShapeError::Key), "{keys:?}");
        }
        // A byte over 0x7e is not visible ASCII, and one below 0x21 (a space) is not either.
        let mut high = HeaderMap::new();
        high.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        high.insert(
            "idempotency-key",
            HeaderValue::from_bytes(b"caf\xe9").unwrap(),
        );
        assert_eq!(
            shape(&high, &body(serde_json::json!({}))),
            Err(ShapeError::Key)
        );
        // The key is judged before the body.
        assert_eq!(
            shape(&headers(&["application/json"], &[]), b"nope"),
            Err(ShapeError::Key)
        );
    }

    // R4
    #[test]
    fn the_body_is_one_object_of_the_known_properties_typed_right() {
        assert!(shaped(serde_json::json!({})).is_ok());
        let bad = [
            "".to_owned(),
            "[]".to_owned(),
            "null".to_owned(),
            "\"x\"".to_owned(),
            "{}".to_owned() + " x",
            String::from_utf8(body(serde_json::json!({}))).unwrap() + "{}",
            format!(
                r#"{{"kind":"acquire","kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"]}}"#
            ),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"extra":1}}"#
            ),
            format!(
                r#"{{"kind":null,"agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"]}}"#
            ),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"vin":null}}"#
            ),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":1,"irDigests":["{DIGEST_A}"]}}"#
            ),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":"{DIGEST_A}"}}"#
            ),
            format!(r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":[1]}}"#),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"remote":"no"}}"#
            ),
            format!(
                r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"parameters":[]}}"#
            ),
        ];
        for text in &bad {
            assert_eq!(shaped_raw(text.as_bytes()), Err(ShapeError::Json), "{text}");
        }
        // Key order and whitespace are free.
        let reordered = format!(
            r#" {{ "irDigests" : ["{DIGEST_A}"], "vciId" : "vci-1", "agentId" : "{AGENT}", "kind" : "acquire" }} "#
        );
        assert_eq!(
            shaped_raw(reordered.as_bytes()),
            shaped(serde_json::json!({}))
        );
    }

    // R5
    #[test]
    fn only_acquire_and_write_settings_are_created_and_the_others_are_not_supported() {
        assert_eq!(
            shaped(serde_json::json!({"kind": "acquire"})).unwrap().kind,
            Kind::Acquire
        );
        assert_eq!(
            shaped(serde_json::json!({"kind": "writeSettings"}))
                .unwrap()
                .kind,
            Kind::WriteSettings
        );
        for kind in [
            "monitorSession",
            "monitorCapture",
            "runSequence",
            "reprogram",
            "identify",
        ] {
            assert_eq!(
                shaped(serde_json::json!({ "kind": kind })),
                Err(ShapeError::KindUnsupported),
                "{kind}"
            );
        }
        for kind in ["", "Acquire", "unknown", "write_settings"] {
            assert_eq!(
                shaped(serde_json::json!({ "kind": kind })),
                Err(ShapeError::Kind),
                "{kind}"
            );
        }
        assert_eq!(
            shaped(serde_json::json!({"kind": null})),
            Err(ShapeError::Kind)
        );
        assert_ne!(
            ShapeError::Kind.title(),
            ShapeError::KindUnsupported.title()
        );
    }

    // R6
    #[test]
    fn the_agent_id_is_a_canonical_lowercase_uuid() {
        let upper = AGENT.to_uppercase();
        let bare = AGENT.replace('-', "");
        for id in [upper.as_str(), bare.as_str(), &AGENT[..35], ""] {
            assert_eq!(
                shaped(serde_json::json!({ "agentId": id })),
                Err(ShapeError::AgentId),
                "{id}"
            );
        }
        assert_eq!(
            shaped(serde_json::json!({"agentId": null})),
            Err(ShapeError::AgentId)
        );
    }

    // R7
    #[test]
    fn the_vci_id_is_one_to_1024_bytes_without_a_control_character() {
        let longest = "v".repeat(1024);
        assert_eq!(
            shaped(serde_json::json!({ "vciId": longest }))
                .unwrap()
                .vci_id,
            longest
        );
        let too_long = "v".repeat(1025);
        for id in ["", too_long.as_str(), "a\nb", "a\u{0}b", "a\u{7f}b"] {
            assert_eq!(
                shaped(serde_json::json!({ "vciId": id })),
                Err(ShapeError::VciId),
                "{id:?}"
            );
        }
        assert_eq!(
            shaped(serde_json::json!({"vciId": null})),
            Err(ShapeError::VciId)
        );
    }

    // R8
    #[test]
    fn a_vin_is_well_formed_when_present_and_may_be_absent() {
        assert_eq!(
            shaped(serde_json::json!({ "vin": VIN }))
                .unwrap()
                .vin
                .as_deref(),
            Some(VIN)
        );
        assert_eq!(shaped(serde_json::json!({})).unwrap().vin, None);
        for vin in [
            "1HGCM82633A00435",
            "1HGCM82633A0043521",
            "1hgcm82633a004352",
            "1HGCM82633A00435I",
            "1HGCM82633A00435O",
            "1HGCM82633A00435Q",
            "",
        ] {
            assert_eq!(
                shaped(serde_json::json!({ "vin": vin })),
                Err(ShapeError::Vin),
                "{vin}"
            );
        }
    }

    // R9
    #[test]
    fn the_digest_list_is_one_to_128_lowercase_hex_digests_in_any_order_with_duplicates() {
        assert_eq!(
            shaped(serde_json::json!({ "irDigests": [DIGEST_B, DIGEST_A, DIGEST_B] }))
                .unwrap()
                .hashes,
            vec![[0xaa; 32], [0xbb; 32]]
        );
        let many: Vec<String> = (0..128).map(|n| format!("{n:064x}")).collect();
        assert_eq!(
            shaped(serde_json::json!({ "irDigests": many }))
                .unwrap()
                .hashes
                .len(),
            128
        );
        let too_many: Vec<String> = (0..129).map(|n| format!("{n:064x}")).collect();
        let upper = DIGEST_A.to_uppercase();
        for list in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!(too_many),
            serde_json::json!([upper]),
            serde_json::json!([&DIGEST_A[..63]]),
            serde_json::json!([format!("{DIGEST_A}a")]),
            serde_json::json!([format!("{}g", &DIGEST_A[..63])]),
            serde_json::json!([DIGEST_A, "x"]),
        ] {
            assert_eq!(
                shaped(serde_json::json!({ "irDigests": list })),
                Err(ShapeError::IrDigests),
                "{list}"
            );
        }
    }

    // R10
    #[test]
    fn the_artifact_digest_is_one_lowercase_hex_digest_when_present() {
        assert_eq!(
            shaped(serde_json::json!({ "artifactDigest": DIGEST_B }))
                .unwrap()
                .artifact,
            Some([0xbb; 32])
        );
        let upper = DIGEST_A.to_uppercase();
        for digest in ["", &DIGEST_A[..63], upper.as_str(), "sha256:abc"] {
            assert_eq!(
                shaped(serde_json::json!({ "artifactDigest": digest })),
                Err(ShapeError::ArtifactDigest),
                "{digest}"
            );
        }
    }

    // R11
    #[test]
    fn the_properties_the_route_does_not_support_are_refused_and_empty_ones_count_as_absent() {
        for extra in [
            serde_json::json!({"remote": true}),
            serde_json::json!({"parameters": {"a": 1}}),
            serde_json::json!({"executionWindow": {}}),
            serde_json::json!({"executionWindow": {"from": "2030-01-01T00:00:00Z"}}),
            serde_json::json!({"onSiteContact": "x"}),
        ] {
            assert_eq!(
                shaped(extra.clone()),
                Err(ShapeError::Unsupported),
                "{extra}"
            );
        }
        let plain = shaped(serde_json::json!({})).unwrap();
        assert_eq!(
            shaped(serde_json::json!({"remote": false})),
            Ok(plain.clone())
        );
        assert_eq!(shaped(serde_json::json!({"parameters": {}})), Ok(plain));
    }

    // R12
    #[test]
    fn the_hash_is_the_same_for_the_same_request_and_differs_in_each_part() {
        let spec = |extra: Value| shaped(extra).unwrap().request_hash();
        let base = spec(serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B }));
        let same = [
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "remote": false, "parameters": {} }),
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "irDigests": [DIGEST_A, DIGEST_A] }),
        ];
        for extra in same {
            assert_eq!(spec(extra), base);
        }
        // Key order and whitespace do not reach the hash: it is of the parsed request.
        let spaced = format!(
            r#"{{ "artifactDigest": "{DIGEST_B}", "vin": "{VIN}", "irDigests": ["{DIGEST_A}"], "vciId": "vci-1", "agentId": "{AGENT}", "kind": "acquire" }}"#
        );
        assert_eq!(body_hash(spaced.as_bytes()), Some(base));
        // Digest order and duplicates do not either.
        let two = spec(serde_json::json!({ "irDigests": [DIGEST_A, DIGEST_B] }));
        assert_eq!(
            spec(serde_json::json!({ "irDigests": [DIGEST_B, DIGEST_A, DIGEST_B] })),
            two
        );
        let differs = [
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "kind": "writeSettings" }),
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "agentId": "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ac" }),
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "vciId": "vci-2" }),
            serde_json::json!({ "artifactDigest": DIGEST_B }),
            serde_json::json!({ "vin": "2HGCM82633A004352", "artifactDigest": DIGEST_B }),
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_B, "irDigests": [DIGEST_A, DIGEST_B] }),
            serde_json::json!({ "vin": VIN }),
            serde_json::json!({ "vin": VIN, "artifactDigest": DIGEST_A }),
        ];
        for extra in differs {
            assert_ne!(spec(extra.clone()), base, "{extra}");
        }
        assert_ne!(two, spec(serde_json::json!({})));
    }

    // F1: a request with no deadline hashes as it did before the field existed. The expected
    // value is the SHA-256 of the encoding in `Spec::request_hash`'s documentation, made without
    // this code.
    #[test]
    fn a_request_without_a_deadline_hashes_as_before_the_field_existed() {
        let hash = shaped(serde_json::json!({})).unwrap().request_hash();
        assert_eq!(
            hex(&hash),
            "45777d016bfcb067d6d6ef46833d13e372f29c8bc79562b15fda50dc1ea57d8a"
        );
    }

    // F6, F7, F10: the normalized second is the field, so one instant is one request and the
    // presence of the field is part of it.
    #[test]
    fn the_deadline_enters_the_hash_as_its_normalized_second_and_its_presence_counts() {
        let spec = |at: &str| {
            shaped(serde_json::json!({ "startDeadline": at }))
                .unwrap()
                .request_hash()
        };
        let base = spec("2030-01-01T10:00:00Z");
        assert_eq!(spec("2030-01-01T19:00:00+09:00"), base);
        assert_eq!(spec("2030-01-01T10:00:00-00:00"), base);
        assert_eq!(spec("2030-01-01T10:00:00.999Z"), base);
        assert_ne!(spec("2030-01-01T10:00:01Z"), base);
        let absent = shaped(serde_json::json!({})).unwrap().request_hash();
        assert_ne!(base, absent);
        // The default's own second is another request than none at all.
        let mut named = shaped(serde_json::json!({})).unwrap();
        named.start_deadline = Some(1_900_000_000);
        assert_ne!(named.request_hash(), absent);
        assert_eq!(
            shaped(serde_json::json!({ "startDeadline": "2030-03-17T17:46:40Z" }))
                .unwrap()
                .start_deadline,
            Some(1_900_000_000)
        );
    }

    // F5
    #[test]
    fn a_malformed_deadline_is_a_shape_error_and_a_well_formed_one_is_kept_as_seconds() {
        for bad in [
            serde_json::json!(5),
            serde_json::json!(true),
            serde_json::json!(["2030-01-01T00:00:00Z"]),
            serde_json::json!(""),
            serde_json::json!("2030-01-01"),
            serde_json::json!("2030-01-01T00:00:00"),
            serde_json::json!("2030-01-01t00:00:00Z"),
            serde_json::json!("2030-01-01T00:00:00z"),
            serde_json::json!("2030-01-01T00:00:60Z"),
            serde_json::json!("2030-02-30T00:00:00Z"),
            serde_json::json!("2030-01-01T00:00:00+16:00"),
            serde_json::json!("2030-01-01T00:00:00.1234567890Z"),
            serde_json::json!(" 2030-01-01T00:00:00Z"),
        ] {
            let result = shape(
                &good_headers(),
                &body(serde_json::json!({ "startDeadline": bad })),
            );
            assert_eq!(
                result.map(|_| ()),
                Err(if bad.is_string() {
                    ShapeError::StartDeadline
                } else {
                    ShapeError::Json
                }),
                "{bad}"
            );
        }
        // The helper drops a null property, so null is sent as raw text.
        let null = format!(
            r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"startDeadline":null}}"#
        );
        assert_eq!(shaped_raw(null.as_bytes()), Err(ShapeError::Json));
        let twice = format!(
            r#"{{"kind":"acquire","agentId":"{AGENT}","vciId":"v","irDigests":["{DIGEST_A}"],"startDeadline":"2030-01-01T00:00:00Z","startDeadline":"2030-01-01T00:00:00Z"}}"#
        );
        assert_eq!(shaped_raw(twice.as_bytes()), Err(ShapeError::Json));
        let ok = shaped(serde_json::json!({ "startDeadline": "2030-01-01T00:00:00Z" })).unwrap();
        assert_eq!(ok.start_deadline, Some(1_893_456_000));
    }

    // F3, F4, F7
    #[test]
    fn the_bounds_are_a_minute_to_a_day_after_the_issue_time_inclusive() {
        let issued = 1_800_000_000_u64;
        let at = |after: i64| deadline_in_bounds(Some(issued as i64 + after), issued);
        assert!(at(60) && at(86_400) && at(900));
        assert!(!at(59) && !at(86_401) && !at(0) && !at(-5) && !at(i64::MIN / 2));
        assert!(deadline_in_bounds(None, issued));
        // The year 1 and the year 9999 are well formed and out of bounds.
        assert!(!deadline_in_bounds(Some(-62_135_596_800), issued));
        assert!(!deadline_in_bounds(Some(253_402_300_799), issued));
    }

    // F1, F2: the instruction carries the request's second, else the default window.
    #[test]
    fn the_instruction_carries_the_requests_deadline_else_the_default_window() {
        let mut spec = shaped(serde_json::json!({})).unwrap();
        let operator = "4e475200-0000-4000-8000-000000000001";
        let id = job_id(1, [0; 10]);
        let issued = 1_800_000_000;
        assert_eq!(
            instruction(&id, operator, &spec, issued).start_deadline,
            issued + 900
        );
        spec.start_deadline = Some(issued as i64 + 61);
        let named = instruction(&id, operator, &spec, issued);
        assert_eq!(named.start_deadline, issued + 61);
        assert_eq!(named.issued_at, issued);
    }

    // R13
    #[tokio::test]
    async fn the_largest_accepted_request_fits_the_instruction_and_the_command_frame() {
        let digests: Vec<String> = (0..128).map(|n| format!("{n:064x}")).collect();
        let request = body(serde_json::json!({
            "vciId": "v".repeat(1024),
            "vin": VIN,
            "irDigests": digests,
            "artifactDigest": DIGEST_B,
        }));
        assert!(request.len() <= MAX_BODY, "{} bytes", request.len());
        let spec = shape(&good_headers(), &request).unwrap().1;
        let operator = "4e475200-0000-4000-8000-000000000001";
        let instruction = instruction(
            &job_id(u64::MAX >> 16, [0xff; 10]),
            operator,
            &spec,
            u64::MAX / 2,
        );
        let payload = instruction.encode().unwrap();
        assert!(payload.len() <= shared_proto::MAX_INSTRUCTION_LEN);
        let key = InstructionKey::from_signer(Arc::new(SigningKey::generate().unwrap()));
        let envelope = key.seal(&instruction).await.unwrap();
        let frame = shared_proto::ServerMessage::JobCommand {
            command_id: "c".repeat(36),
            signed_job: Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &envelope,
            )),
        }
        .encode()
        .unwrap();
        assert!(
            frame.len() <= shared_proto::MAX_CONTROL_MESSAGE_LEN,
            "{} bytes",
            frame.len()
        );
    }

    // R14
    #[test]
    fn a_job_id_is_a_version_7_uuid_of_the_given_time_and_random_bytes() {
        let ms = 0x0194_f0a2_7c3e_u64;
        let id = job_id(ms, [0xff, 0xab, 0xff, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(id, "0194f0a2-7c3e-7fab-bf01-020304050607");
        assert!(is_canonical_uuid(&id));
        let low = job_id(ms, [0; 10]);
        assert_eq!(low, "0194f0a2-7c3e-7000-8000-000000000000");
        // Bits above the 48-bit time are dropped.
        assert_eq!(job_id(1 << 48 | ms, [0; 10]), low);
    }

    #[test]
    fn permission_fails_on_the_first_missing_fact_in_the_order_role_agent_allow_list() {
        let all = Facts {
            worker: true,
            agent_found: true,
            agent_unrevoked: true,
            allowed: true,
        };
        assert_eq!(authorize(all), Ok(()));
        let none = Facts {
            worker: false,
            agent_found: false,
            agent_unrevoked: false,
            allowed: false,
        };
        assert_eq!(authorize(none), Err(Denial::NotWorker));
        assert_eq!(
            authorize(Facts {
                worker: true,
                ..none
            }),
            Err(Denial::AgentUnknown)
        );
        assert_eq!(
            authorize(Facts {
                worker: true,
                agent_found: true,
                ..none
            }),
            Err(Denial::AgentRevoked)
        );
        assert_eq!(
            authorize(Facts {
                allowed: false,
                ..all
            }),
            Err(Denial::NotAllowed)
        );
    }

    #[test]
    fn a_key_replays_for_the_same_operator_and_hash_and_otherwise_conflicts() {
        let hash = [7u8; 32];
        assert!(replays("op", Some(&hash), "op", &hash));
        assert!(!replays("op", Some(&hash), "other", &hash));
        assert!(!replays("op", Some(&[8u8; 32]), "op", &hash));
        assert!(!replays("op", None, "op", &hash));
        assert!(!replays("op", Some(&hash[..31]), "op", &hash));
    }

    #[test]
    fn the_job_document_names_the_state_and_the_confirmation() {
        let stored = Stored {
            id: "i".into(),
            kind: "acquire".into(),
            state: "received".into(),
            progress_permille: 0,
            level: "notifyOnly".into(),
            operator_id: "o".into(),
            request_hash: None,
            failure_code: None,
            procedure_code: None,
        };
        assert_eq!(
            stored.document(),
            serde_json::json!({
                "jobId": "i", "kind": "acquire", "state": "received",
                "progressPermille": 0, "confirmation": { "level": "notifyOnly" }
            })
        );
    }

    // J17 (the document's half): the failure shows its code and procedure code, never a detail.
    #[test]
    fn a_failed_job_document_shows_the_failure_code_and_the_procedure_code_only() {
        let mut stored = Stored {
            id: "i".into(),
            kind: "runSequence".into(),
            state: "failed".into(),
            progress_permille: 0,
            level: "notifyOnly".into(),
            operator_id: "o".into(),
            request_hash: None,
            failure_code: Some("procedure".into()),
            procedure_code: Some("P-17".into()),
        };
        assert_eq!(
            stored.document()["failure"],
            serde_json::json!({ "code": "procedure", "procedureCode": "P-17" })
        );
        stored.procedure_code = None;
        stored.failure_code = Some("vinMismatch".into());
        assert_eq!(
            stored.document()["failure"],
            serde_json::json!({ "code": "vinMismatch" })
        );
        stored.failure_code = None;
        assert!(stored.document().get("failure").is_none());
    }

    // V1: the run allowance after the start deadline of each kind this route creates.
    #[test]
    fn each_kind_has_its_lock_allowance() {
        assert_eq!(Kind::Acquire.lock_secs(), 600);
        assert_eq!(Kind::WriteSettings.lock_secs(), 1800);
    }
}

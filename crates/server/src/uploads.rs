//! The data channel's three first routes (design 5.2, 13.3, 14.3; ADR-493 items 1 to 5, 7, 8, 15
//! to 18): `POST /api/v1/agent/uploads` (begin), `POST /api/v1/agent/uploads/{uploadId}/parts/{part}`
//! (one part URL) and `GET /api/v1/agent/uploads/{uploadId}` (status). The completion is not
//! served yet.
//!
//! A request is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the body (at most [`MAX_BODY`] bytes, 413; within 5 seconds,
//! 408; broken, 400), the shape (400: for begin the JSON object, for the part route the part
//! number, for the other two an empty body), for begin the declared length (413), the bearer token
//! (401 with `WWW-Authenticate: Bearer`), the agent's row (401 and the agent's tokens dropped when
//! it is revoked or missing, 503, 500), the job or the upload (404), the request against the row
//! (400 for a part outside the count, 409), the object store (503, 500) and the answer.
//!
//! Every database call is a short transaction of its own, and none is open while the object store
//! is called. The three writers of `job_objects` are this module's INSERT (state `open`), and,
//! with the completion, its updates; nothing here writes any other state.
//!
//! Never logged: a token, a body, a digest, an object key, an upload ID, a URL or the text of a
//! database error (the SQLSTATE only).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Value, json};
use shared_proto::{AgentId, JobInstruction, is_canonical_uuid};
use sqlx::PgPool;

use crate::control::{bearer_token, unauthorized};
use crate::db;
use crate::health::may_log;
use crate::problem::Problem;
use crate::register::{bounded_body_within, closing, is_json};
use crate::session::{Sessions, now};
use crate::storage::{
    MAX_PARTS, MultipartStore, ObjectKey, PART_URL_EXPIRY, Part, StoreError, Upload, UploadId,
};

/// The begin route's path.
pub const BEGIN_PATH: &str = "/api/v1/agent/uploads";
/// The part URL route's path, as the router and `api/openapi.yaml` write it.
pub const PART_PATH: &str = "/api/v1/agent/uploads/{uploadId}/parts/{part}";
/// The status route's path.
pub const STATUS_PATH: &str = "/api/v1/agent/uploads/{uploadId}";

/// The largest body of these routes, in bytes; exactly this many pass.
pub const MAX_BODY: usize = 4096;
/// The most an object may declare: 4 GiB.
pub const MAX_SIZE: u64 = 4 * 1024 * 1024 * 1024;
/// The size of a part, which is the store's minimum part (ADR-493 item 3). A row keeps the size
/// it was begun with.
pub const PART_BYTES: u64 = 5 * 1024 * 1024;
/// A failure is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// How many parts an object of `size` bytes has with parts of `part_bytes`: the length divided by
/// the part size, rounded up, and at least 1 (an empty object is one empty part).
pub const fn part_count(size: u64, part_bytes: u64) -> u64 {
    let count = size.div_ceil(part_bytes);
    if count == 0 { 1 } else { count }
}

/// The size of the last part: what remains after the full parts before it. It is `part_bytes` when
/// the size is a multiple of it, and 0 for an empty object.
pub const fn last_part_size(size: u64, part_bytes: u64) -> u64 {
    size - (part_count(size, part_bytes) - 1) * part_bytes
}

// The largest object has at most the store's part limit of parts, and its last part is from 1
// byte to a whole part. A change of a constant that breaks the formula fails the build.
const _: () = {
    assert!(part_count(MAX_SIZE, PART_BYTES) <= MAX_PARTS as u64);
    assert!(part_count(0, PART_BYTES) == 1 && last_part_size(0, PART_BYTES) == 0);
    assert!(last_part_size(MAX_SIZE, PART_BYTES) >= 1);
    assert!(last_part_size(MAX_SIZE, PART_BYTES) <= PART_BYTES);
    assert!(last_part_size(PART_BYTES, PART_BYTES) == PART_BYTES);
    assert!(part_count(PART_BYTES + 1, PART_BYTES) == 2);
    assert!(last_part_size(PART_BYTES + 1, PART_BYTES) == 1);
};

/// What the routes use of the object store, and the part size. `Debug` shows nothing of the store.
#[derive(Clone)]
pub struct Backend {
    store: Option<Arc<dyn MultipartStore>>,
    part_bytes: u64,
}

impl Backend {
    /// The store the routes call (`None`: a server without an object server, whose routes answer
    /// 503 where they need it) and the part size of [`PART_BYTES`].
    pub fn new(store: Option<Arc<dyn MultipartStore>>) -> Backend {
        Backend {
            store,
            part_bytes: PART_BYTES,
        }
    }

    /// Another part size, so that a test can send small parts. A size under which the largest
    /// object would pass the store's part limit is refused. Tests only.
    #[doc(hidden)]
    pub fn with_part_bytes(mut self, part_bytes: u64) -> Backend {
        assert!(
            part_bytes > 0 && part_count(MAX_SIZE, part_bytes) <= u64::from(MAX_PARTS),
            "the part size would let an object pass the part limit"
        );
        self.part_bytes = part_bytes;
        self
    }
}

impl Default for Backend {
    fn default() -> Backend {
        Backend::new(None)
    }
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("store", &self.store.is_some())
            .field("part_bytes", &self.part_bytes)
            .finish()
    }
}

/// The state of the three routes.
#[derive(Clone)]
pub struct Uploads {
    pool: PgPool,
    sessions: Sessions,
    backend: Backend,
    gate: Arc<Mutex<Option<Instant>>>,
}

impl Uploads {
    pub fn new(pool: PgPool, sessions: Sessions, backend: Backend) -> Uploads {
        Uploads {
            pool,
            sessions,
            backend,
            gate: Arc::new(Mutex::new(None)),
        }
    }

    fn warn(&self, what: &str, why: &str) {
        if may_log(&self.gate, now(), LOG_EVERY) {
            tracing::warn!(why, "upload route: {what}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Answers

fn json_answer(status: StatusCode, value: Value) -> Response {
    let mut response = (status, value.to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn problem(status: StatusCode, title: &'static str) -> Response {
    Problem::new(status, title).into_response()
}

fn not_found() -> Response {
    problem(StatusCode::NOT_FOUND, "No such upload")
}

fn bad_request(title: &'static str) -> Response {
    problem(StatusCode::BAD_REQUEST, title)
}

fn db_failure(u: &Uploads, error: &sqlx::Error) -> Response {
    match db::failure(error) {
        db::Failure::Unavailable(why) => {
            u.warn("the database is unavailable", &why);
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "The database is unavailable",
            )
        }
        db::Failure::Internal(why) => {
            u.warn("a statement failed", &why);
            problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed")
        }
    }
}

fn store_failure(u: &Uploads, error: &StoreError) -> Response {
    // The store's messages are fixed text: no key, URL or upload ID.
    u.warn("the object store failed", &error.to_string());
    match error {
        StoreError::Random => problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed"),
        _ => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "The object store is unavailable",
        ),
    }
}

fn no_store() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "The object store is unavailable",
    )
}

// ---------------------------------------------------------------------------------------------
// The database

/// A row of `job_objects` as the routes read it.
struct Row {
    id: String,
    agent_id: String,
    storage_key: String,
    upload_id: String,
    part_bytes: u64,
    size_bytes: u64,
    digest: String,
    state: String,
}

type RawRow = (String, String, String, String, i64, i64, String, String);

const ROW_COLUMNS: &str =
    "id::text, agent_id::text, storage_key, upload_id, part_bytes, size_bytes, digest, state";

impl Row {
    fn from_raw(raw: RawRow) -> Option<Row> {
        Some(Row {
            id: raw.0,
            agent_id: raw.1,
            storage_key: raw.2,
            upload_id: raw.3,
            part_bytes: u64::try_from(raw.4).ok().filter(|b| *b > 0)?,
            size_bytes: u64::try_from(raw.5).ok()?,
            digest: raw.6,
            state: raw.7,
        })
    }

    fn count(&self) -> u64 {
        part_count(self.size_bytes, self.part_bytes)
    }

    fn answer(&self, status: StatusCode) -> Response {
        json_answer(
            status,
            json!({
                "uploadId": self.id,
                "partBytes": self.part_bytes,
                "partCount": self.count(),
                "state": self.state,
            }),
        )
    }

    /// The row's store handle. A row that does not parse is the server's fault.
    fn upload(&self) -> Option<Upload> {
        Some(Upload {
            key: ObjectKey::parse(&self.storage_key).ok()?,
            id: UploadId::parse(&self.upload_id).ok()?,
        })
    }
}

/// The agent's tenant, and whether its row is revoked; `None` when there is no row.
async fn agent_standing(pool: &PgPool, agent: &str) -> Result<Option<(String, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT tenant_id::text, revoked_at IS NOT NULL FROM agents WHERE id = $1::uuid",
    )
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(row)
}

enum JobLookup {
    /// No job of that ID belongs to the agent.
    NoJob,
    /// The stored instruction does not decode.
    Undecodable,
    /// The job's generation is not the one declared.
    OtherGeneration,
    Found(Option<Row>),
}

/// The instruction's ownership generation of the job, then the row that holds the declaration, in
/// one transaction.
async fn lookup_job(
    pool: &PgPool,
    tenant: &str,
    agent: &str,
    declared: &Declared,
) -> Result<JobLookup, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let signed: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT signed_job FROM jobs WHERE tenant_id = $1::uuid AND id = $2::uuid \
         AND agent_id = $3::uuid",
    )
    .bind(tenant)
    .bind(&declared.job_id)
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((signed,)) = signed else {
        return Ok(JobLookup::NoJob);
    };
    let Some(generation) = instruction_generation(&signed) else {
        return Ok(JobLookup::Undecodable);
    };
    if generation != declared.generation {
        return Ok(JobLookup::OtherGeneration);
    }
    let row = live_row(&mut tx, tenant, declared).await?;
    Ok(JobLookup::Found(row))
}

/// The row that is not `failed` for the declared job, generation and kind.
async fn live_row(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    declared: &Declared,
) -> Result<Option<Row>, sqlx::Error> {
    let raw: Option<RawRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM job_objects WHERE tenant_id = $1::uuid AND job_id = $2::uuid \
         AND generation = $3 AND kind = $4 AND state <> 'failed'"
    )))
    .bind(tenant)
    .bind(&declared.job_id)
    .bind(declared.generation as i64)
    .bind(declared.kind)
    .fetch_optional(tx)
    .await?;
    Ok(raw.and_then(Row::from_raw))
}

/// The instruction's `ownership_generation` out of a stored envelope; `None` if it does not
/// decode. The server wrote the envelope, so its signature is not checked again.
fn instruction_generation(signed: &[u8]) -> Option<u64> {
    let envelope = shared_crypto::instruction::Envelope::split(signed).ok()?;
    let instruction = JobInstruction::decode(envelope.payload()).ok()?;
    Some(instruction.ownership_generation)
}

/// Why [`insert_row`] failed. The two differ in what is known of the row: a failed statement (or
/// one that never ran) left nothing, while a failed commit may have committed, the answer lost.
enum InsertFailure {
    Statement(sqlx::Error),
    Commit(sqlx::Error),
}

/// Inserts the row as `open`. The conflict is the partial unique index's: a row that is not
/// `failed` already holds the declaration, which `DO NOTHING` answers as no row, whatever the
/// concurrent transaction's progress (READ COMMITTED waits for it to end). `Some` is the new ID.
async fn insert_row(
    pool: &PgPool,
    tenant: &str,
    agent: &str,
    declared: &Declared,
    upload: &Upload,
    part_bytes: u64,
) -> Result<Option<String>, InsertFailure> {
    insert_statement(pool, tenant, agent, declared, upload, part_bytes).await
}

async fn insert_statement(
    pool: &PgPool,
    tenant: &str,
    agent: &str,
    declared: &Declared,
    upload: &Upload,
    part_bytes: u64,
) -> Result<Option<String>, InsertFailure> {
    use InsertFailure::{Commit, Statement};
    let mut tx = pool.begin().await.map_err(Statement)?;
    db::statement_timeouts(&mut tx).await.map_err(Statement)?;
    let id: Option<(String,)> = sqlx::query_as(
        "INSERT INTO job_objects (tenant_id, id, job_id, generation, kind, agent_id, storage_key, \
             upload_id, part_bytes, size_bytes, digest, state) \
         VALUES ($1::uuid, gen_random_uuid(), $2::uuid, $3, $4, $5::uuid, $6, $7, $8, $9, $10, 'open') \
         ON CONFLICT (tenant_id, job_id, generation, kind) WHERE state <> 'failed' DO NOTHING \
         RETURNING id::text",
    )
    .bind(tenant)
    .bind(&declared.job_id)
    .bind(declared.generation as i64)
    .bind(declared.kind)
    .bind(agent)
    .bind(upload.key.as_str())
    .bind(upload.id.expose())
    .bind(part_bytes as i64)
    .bind(declared.size as i64)
    .bind(&declared.digest)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Statement)?;
    tx.commit().await.map_err(Commit)?;
    Ok(id.map(|(id,)| id))
}

/// The ID of the row that names `storage_key`, if one does.
async fn row_of_key(
    pool: &PgPool,
    tenant: &str,
    storage_key: &str,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let id: Option<(String,)> = sqlx::query_as(
        "SELECT id::text FROM job_objects WHERE tenant_id = $1::uuid AND storage_key = $2",
    )
    .bind(tenant)
    .bind(storage_key)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(id.map(|(id,)| id))
}

/// Re-reads the declaration's row after a lost insert.
async fn reread(
    pool: &PgPool,
    tenant: &str,
    declared: &Declared,
) -> Result<Option<Row>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    live_row(&mut tx, tenant, declared).await
}

/// The caller's row by its ID: `None` for an unknown ID and for another agent's or tenant's.
async fn upload_row(
    pool: &PgPool,
    tenant: &str,
    agent: &str,
    id: &str,
) -> Result<Option<Row>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let raw: Option<RawRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM job_objects WHERE tenant_id = $1::uuid AND id = $2::uuid \
         AND agent_id = $3::uuid"
    )))
    .bind(tenant)
    .bind(id)
    .bind(agent)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(raw.and_then(Row::from_raw))
}

// ---------------------------------------------------------------------------------------------
// The caller

struct Caller {
    agent: AgentId,
    tenant: String,
}

/// Judges the token, then reads the agent's row (ADR-493 item 2): the answer to a token that is
/// unknown or expired and to a row that is revoked or missing is the same 401, and the latter
/// drops the agent's tokens.
async fn authenticate(u: &Uploads, headers: &HeaderMap) -> Result<Caller, Box<Response>> {
    let Some(agent) = bearer_token(headers).and_then(|token| u.sessions.authenticate(token)) else {
        return Err(Box::new(unauthorized()));
    };
    match agent_standing(&u.pool, &agent.0).await {
        Ok(Some((tenant, false))) => Ok(Caller { agent, tenant }),
        Ok(_) => {
            u.sessions.revoke_agent(&agent);
            tracing::info!(agent_id = %agent.0, "upload request refused: the agent is revoked or unknown");
            Err(Box::new(unauthorized()))
        }
        Err(error) => Err(Box::new(db_failure(u, &error))),
    }
}

/// Reads the body within the bounds; the routes without a body refuse any byte of one (400).
async fn empty_body(body: axum::body::Body) -> Result<(), Box<Response>> {
    let bytes = bounded_body_within(body, MAX_BODY)
        .await
        .map_err(Box::new)?;
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(Box::new(closing(
            StatusCode::BAD_REQUEST,
            "The body must be empty",
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// The shape of begin

/// A number of the body as the visitor sees it: serde_json turns an integer past `u64` into a
/// float, and a length that large is over the limit, not malformed. A number beyond a double is
/// refused by the reader itself, which makes the whole body a 400.
enum Number {
    Int(u64),
    /// An integral value of 2^64 or more (written as a big integer or in exponent form).
    Huge,
    /// Negative, fractional or not finite.
    Bad,
}

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Number, D::Error> {
        struct NumberVisitor;

        impl Visitor<'_> for NumberVisitor {
            type Value = Number;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Number, E> {
                Ok(Number::Int(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Number, E> {
                Ok(u64::try_from(v).map_or(Number::Bad, Number::Int))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Number, E> {
                // 2^64 as a float.
                Ok(
                    if v.is_finite() && v.fract() == 0.0 && v >= 18_446_744_073_709_551_616.0 {
                        Number::Huge
                    } else {
                        Number::Bad
                    },
                )
            }
        }

        deserializer.deserialize_any(NumberVisitor)
    }
}

/// The five properties of the body, typed; nothing is judged yet.
#[derive(Default)]
struct Raw {
    job_id: Option<String>,
    generation: Option<Number>,
    kind: Option<String>,
    size_bytes: Option<Number>,
    sha256: Option<String>,
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Raw, D::Error> {
        struct RawVisitor;

        impl<'de> Visitor<'de> for RawVisitor {
            type Value = Raw;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an upload declaration")
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
                        "jobId" => put(&mut raw.job_id, map.next_value()?)?,
                        "generation" => put(&mut raw.generation, map.next_value()?)?,
                        "kind" => put(&mut raw.kind, map.next_value()?)?,
                        "sizeBytes" => put(&mut raw.size_bytes, map.next_value()?)?,
                        "sha256" => put(&mut raw.sha256, map.next_value()?)?,
                        _ => return Err(de::Error::custom("unknown property")),
                    }
                }
                Ok(raw)
            }
        }

        deserializer.deserialize_map(RawVisitor)
    }
}

/// What begin declares, judged for its shape. A length past [`MAX_SIZE`] is a shape that is
/// valid: the caller answers it 413 once the whole shape has passed.
struct Declared {
    job_id: String,
    generation: u64,
    kind: &'static str,
    size: u64,
    over: bool,
    digest: String,
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn shape(headers: &HeaderMap, body: &[u8]) -> Option<Declared> {
    if !is_json(headers) {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let raw = Raw::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    let job_id = raw.job_id.filter(|id| is_canonical_uuid(id))?;
    let generation = match raw.generation? {
        Number::Int(g) if i64::try_from(g).is_ok() => g,
        _ => return None,
    };
    let kind = match raw.kind?.as_str() {
        "result" => "result",
        "commLog" => "commLog",
        _ => return None,
    };
    let (size, over) = match raw.size_bytes? {
        Number::Int(s) => (s, s > MAX_SIZE),
        Number::Huge => (u64::MAX, true),
        Number::Bad => return None,
    };
    let digest = raw.sha256.filter(|d| is_digest(d))?;
    Some(Declared {
        job_id,
        generation,
        kind,
        size,
        over,
        digest,
    })
}

// ---------------------------------------------------------------------------------------------
// Begin

/// Starts the best-effort removal of an upload this request began and will not use. A failure is
/// logged (fixed words) and does not change the answer: the upload is then left for the cleanup
/// the backlog names.
async fn abort(store: &dyn MultipartStore, upload: &Upload) {
    if let Err(error) = store.delete(&upload.key).await {
        tracing::warn!(
            %error,
            "an upload that was begun and not used could not be aborted"
        );
    }
}

/// The answer for a row that holds the declaration: the same declaration is the row (200),
/// another one is a conflict (409).
fn existing(row: &Row, agent: &AgentId, declared: &Declared) -> Response {
    if row.agent_id == agent.0 && row.size_bytes == declared.size && row.digest == declared.digest {
        row.answer(StatusCode::OK)
    } else {
        problem(
            StatusCode::CONFLICT,
            "The job already has an upload with another declaration",
        )
    }
}

/// `POST /api/v1/agent/uploads`.
pub async fn begin(State(u): State<Uploads>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match bounded_body_within(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let Some(declared) = shape(&parts.headers, &bytes) else {
        return bad_request("The body is not the expected JSON object");
    };
    if declared.over {
        return problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The object is over the size limit",
        );
    }
    let caller = match authenticate(&u, &parts.headers).await {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    match lookup_job(&u.pool, &caller.tenant, &caller.agent.0, &declared).await {
        Err(error) => return db_failure(&u, &error),
        Ok(JobLookup::NoJob | JobLookup::OtherGeneration) => {
            return problem(StatusCode::NOT_FOUND, "No such job");
        }
        Ok(JobLookup::Undecodable) => {
            u.warn("a stored job instruction does not decode", "");
            return problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed");
        }
        Ok(JobLookup::Found(Some(row))) => return existing(&row, &caller.agent, &declared),
        Ok(JobLookup::Found(None)) => {}
    }
    let Some(store) = u.backend.store.as_deref() else {
        return no_store();
    };
    let upload = match store.begin().await {
        Ok(upload) => upload,
        Err(error) => return store_failure(&u, &error),
    };
    let part_bytes = u.backend.part_bytes;
    match insert_row(
        &u.pool,
        &caller.tenant,
        &caller.agent.0,
        &declared,
        &upload,
        part_bytes,
    )
    .await
    {
        Ok(Some(id)) => json_answer(
            StatusCode::CREATED,
            json!({
                "uploadId": id,
                "partBytes": part_bytes,
                "partCount": part_count(declared.size, part_bytes),
                "state": "open",
            }),
        ),
        Ok(None) => {
            // Another request inserted the declaration first: this upload is not used.
            abort(store, &upload).await;
            match reread(&u.pool, &caller.tenant, &declared).await {
                Ok(Some(row)) => existing(&row, &caller.agent, &declared),
                Ok(None) => problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The upload changed while it was begun; repeat the request",
                ),
                Err(error) => db_failure(&u, &error),
            }
        }
        Err(InsertFailure::Statement(error)) => {
            // The statement did not insert: nothing names the upload.
            abort(store, &upload).await;
            db_failure(&u, &error)
        }
        Err(InsertFailure::Commit(error)) => {
            // The commit's answer was lost, so the row may exist: aborting the upload now could
            // leave a row whose upload is gone. Look for a row that names the key, and abort only
            // when none does. When that read fails too, the upload is left for the cleanup of
            // unfinished uploads (a logged warning), never aborted on a guess.
            match row_of_key(&u.pool, &caller.tenant, upload.key.as_str()).await {
                Ok(Some(id)) => json_answer(
                    StatusCode::CREATED,
                    json!({
                        "uploadId": id,
                        "partBytes": part_bytes,
                        "partCount": part_count(declared.size, part_bytes),
                        "state": "open",
                    }),
                ),
                Ok(None) => {
                    abort(store, &upload).await;
                    db_failure(&u, &error)
                }
                Err(_) => {
                    u.warn(
                        "an upload may be left without a row after a failed commit",
                        "the commit and the read after it failed",
                    );
                    db_failure(&u, &error)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The part URL and the status

/// A part number as the route accepts it: canonical decimal digits (no sign, no leading zero) from
/// 1 to the store's limit.
fn parse_part(text: &str) -> Option<u16> {
    if text.is_empty()
        || text.len() > 5
        || !text.bytes().all(|b| b.is_ascii_digit())
        || text.starts_with('0')
    {
        return None;
    }
    let part: u16 = text.parse().ok()?;
    (1..=MAX_PARTS).contains(&part).then_some(part)
}

/// The upload ID and the part of the part URL's path, as sent; an empty text for a segment the
/// path does not have (the router matched the path, so this does not happen).
fn path_segments(path: &str) -> (&str, &str) {
    let rest = path
        .strip_prefix("/api/v1/agent/uploads/")
        .unwrap_or_default();
    let mut segments = rest.split('/');
    let id = segments.next().unwrap_or_default();
    let part = segments.nth(1).unwrap_or_default();
    (id, part)
}

/// The row an upload ID names for the caller, or the answer. An ID that is not a canonical UUID
/// names nothing.
async fn find(u: &Uploads, caller: &Caller, id: &str) -> Result<Row, Box<Response>> {
    if !is_canonical_uuid(id) {
        return Err(Box::new(not_found()));
    }
    match upload_row(&u.pool, &caller.tenant, &caller.agent.0, id).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(Box::new(not_found())),
        Err(error) => Err(Box::new(db_failure(u, &error))),
    }
}

/// `POST /api/v1/agent/uploads/{uploadId}/parts/{part}`.
pub async fn part_url(State(u): State<Uploads>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    if let Err(response) = empty_body(body).await {
        return *response;
    }
    // The segments are taken as sent, not decoded: a percent-encoded byte that is not UTF-8 in
    // the upload ID is then a name that is no UUID (404 after the token), as in the status route,
    // and one in the part is not a canonical number (400 before it).
    let (id, part) = path_segments(parts.uri.path());
    let Some(part) = parse_part(part) else {
        return bad_request("The part must be a number from 1 to 10000");
    };
    let caller = match authenticate(&u, &parts.headers).await {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let row = match find(&u, &caller, id).await {
        Ok(row) => row,
        Err(response) => return *response,
    };
    if u64::from(part) > row.count() {
        return bad_request("The part is beyond the upload's part count");
    }
    if row.state != "open" {
        return Problem::with_state(
            StatusCode::CONFLICT,
            "The upload is not open",
            row.state.clone(),
        )
        .into_response();
    }
    let Some(store) = u.backend.store.as_deref() else {
        return no_store();
    };
    let Some(upload) = row.upload() else {
        u.warn("a stored upload does not parse", "");
        return problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed");
    };
    match store.presign_part(&upload, part) {
        Ok(url) => json_answer(
            StatusCode::OK,
            json!({"url": url.expose(), "expiresInSecs": PART_URL_EXPIRY.as_secs()}),
        ),
        Err(error) => store_failure(&u, &error),
    }
}

/// The count of leading parts that exist with the size the row expects.
fn parts_held(parts: &[Part], row: &Row) -> u64 {
    let count = row.count();
    let mut held = 0;
    for number in 1..=count {
        let want = if number == count {
            last_part_size(row.size_bytes, row.part_bytes)
        } else {
            row.part_bytes
        };
        // The listing is in part order and a number occurs once.
        match parts.iter().find(|p| u64::from(p.number) == number) {
            Some(part) if part.size == want => held += 1,
            _ => break,
        }
    }
    held
}

/// `GET /api/v1/agent/uploads/{uploadId}`.
pub async fn status(
    State(u): State<Uploads>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if let Err(response) = empty_body(body).await {
        return *response;
    }
    let caller = match authenticate(&u, &parts.headers).await {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let Ok(Path(id)) = path else {
        return not_found();
    };
    let row = match find(&u, &caller, &id).await {
        Ok(row) => row,
        Err(response) => return *response,
    };
    let mut answer = json!({
        "state": row.state,
        "partBytes": row.part_bytes,
        "partCount": row.count(),
    });
    if row.state == "open" {
        let Some(store) = u.backend.store.as_deref() else {
            return no_store();
        };
        let Some(upload) = row.upload() else {
            u.warn("a stored upload does not parse", "");
            return problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed");
        };
        // An upload the store no longer has is not a state of this row to write: the answer is a
        // 503 and the row stays as it is (ADR-493 item 17).
        match store.list_parts(&upload).await {
            Ok(listed) => answer["partsHeld"] = parts_held(&listed, &row).into(),
            Err(error) => return store_failure(&u, &error),
        }
    }
    json_answer(StatusCode::OK, answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_part_number_is_canonical_decimal_from_1_to_10000() {
        for good in ["1", "9", "10", "820", "9999", "10000"] {
            assert!(parse_part(good).is_some(), "{good}");
        }
        for bad in [
            "", "0", "00", "01", "+1", "-1", " 1", "1 ", "1.0", "1e2", "10001", "65536", "100000",
            "٣", "0x10",
        ] {
            assert_eq!(parse_part(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_part_count_and_the_last_part_follow_one_formula() {
        let p = 10;
        assert_eq!((part_count(0, p), last_part_size(0, p)), (1, 0));
        assert_eq!((part_count(1, p), last_part_size(1, p)), (1, 1));
        assert_eq!((part_count(10, p), last_part_size(10, p)), (1, 10));
        assert_eq!((part_count(11, p), last_part_size(11, p)), (2, 1));
        assert_eq!((part_count(20, p), last_part_size(20, p)), (2, 10));
        assert_eq!(part_count(MAX_SIZE, PART_BYTES), 820);
        // Every size: the full parts and the last add up to the size.
        for size in 0..=65 {
            let count = part_count(size, p);
            assert_eq!((count - 1) * p + last_part_size(size, p), size);
            assert!((1..=p).contains(&last_part_size(size, p)) || size == 0);
        }
    }

    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h
    }

    const JOB: &str = "0192f3a4-5b6c-7d8e-8f90-123456789abc";

    fn body(size: &str) -> String {
        format!(
            r#"{{"jobId":"{JOB}","generation":1,"kind":"result","sizeBytes":{size},"sha256":"{}"}}"#,
            "ab".repeat(32)
        )
    }

    #[test]
    fn a_number_past_u64_is_over_the_limit_and_not_malformed() {
        let shaped = |size: &str| shape(&headers(), body(size).as_bytes());
        assert!(!shaped("4294967296").unwrap().over);
        assert!(shaped("4294967297").unwrap().over);
        assert!(shaped("18446744073709551615").unwrap().over);
        assert!(shaped("18446744073709551616").unwrap().over);
        assert!(shaped("99999999999999999999999999").unwrap().over);
        assert!(shaped("1e30").unwrap().over);
        // Beyond a double the reader refuses the number whole: a 400, not a length over the limit.
        let huge_integer = "9".repeat(400);
        for bad in [
            "-1",
            "1.5",
            "1.0",
            "1e3",
            "5e9",
            "1e400",
            huge_integer.as_str(),
            "\"5\"",
            "null",
            "true",
            "[1]",
        ] {
            assert!(shaped(bad).is_none(), "{bad}");
        }
    }
}

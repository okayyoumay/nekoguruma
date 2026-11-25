//! The Web API that requests a job's communication log (design 8.11, 16.2, ADR-455, ADR-508
//! items 1 and 2): `GET /api/v1/comm-log-uploads/scope` shows the scope of a request, and
//! `POST /api/v1/comm-log-uploads` makes the request for a scope that is still the one shown.
//!
//! Both take 1 to [`MAX_JOBS`] distinct job IDs, which are judged before the database is asked
//! (400). Then, in one transaction: the caller's permission (the role `worker` with an empty
//! scope, 403, the same answer whatever the IDs), then one statement that reads the caller's
//! jobs, their agents (and whether the caller is still on each agent's allow-list), their
//! `commLog` objects and their request rows, so that the answer is one snapshot ([`load`]). A job that is not found for the tenant and the caller (unknown, another
//! tenant's, or another operator's) is 404, one answer for the three; a stored instruction that
//! does not decode is 500. Both routes then judge the job the same way ([`refusal`]).
//!
//! The scope's digest is a SHA-256 over a tagged, length-prefixed encoding of what the operator
//! is shown that the job's later life can change or that says whose data it is: per job, in
//! ascending job-ID order, the ID, the ownership generation of the stored instruction, the
//! agent, the kind, the VIN, the creation time and the end time ([`scope_digest`]). The log's
//! state and the request's state are not in it: the request itself moves them within seconds, so
//! a repeat of a request whose answer was lost would otherwise be refused. It is not a
//! credential, and `POST` judges every permission again. It derives from the VIN, so it is never
//! logged, never in an audit row and never in a problem.
//!
//! `POST` answers in this order, the first that applies: 403 without the permission; 404; 500;
//! 403 (the same document) for a job whose agent is revoked or whose agent's allow-list no longer
//! names the caller, judged with the agents' rows held `FOR SHARE` until the commit, so that a
//! revocation cannot commit mid-request; 422 `JOB_NOT_DELIVERED`, then 422 `JOB_NOT_ENDED`; 409 `SCOPE_CHANGED`. A job is
//! delivered when the agent acknowledged its delivery and ended when it has an end time, never by
//! its state's name. The answer is the worst class over all the jobs, so it does not depend on
//! the order of the IDs. Then the rows are written in job-ID order, so that two requests with
//! overlapping jobs cannot wait on each other: a new request is inserted, a `refused` or `expired`
//! one renewed ([`comm_log_requests::request`]), one `requested` or `accepted` left as it is, and
//! a job whose log is stored (a `finished` `commLog` object, checked again by the statements
//! themselves) is left alone whatever its row says. Each job inserted or renewed has one audit row in the same transaction. All of it
//! commits together or not at all, and the answer is 202 with each job's request state after the
//! write.
//!
//! What the 403 refused goes to the log only, with the operator and no job.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shared_proto::is_canonical_uuid;
use sqlx::{PgConnection, PgPool};

use crate::audit;
use crate::comm_log_requests::{self, RequestState, Written};
use crate::db;
use crate::health::may_log;
use crate::jobs::{JobError, hex, parse_digest, put_once, put_text};
use crate::operator_session::SessionOperator;
use crate::problem::Problem;
use crate::register::{bounded_body_within, is_json};
use crate::uploads::decode_instruction;

/// The path of the scope read.
pub const SCOPE_PATH: &str = "/api/v1/comm-log-uploads/scope";
/// The path of the request.
pub const PATH: &str = "/api/v1/comm-log-uploads";
/// The most jobs one call names.
pub const MAX_JOBS: usize = 32;
/// The largest body accepted, in bytes: far above what 32 job IDs and a digest need.
pub const MAX_BODY: usize = 4 * 1024;
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// The domain tag at the start of a scope's digest. A change of the fields or the encoding gives
/// a new tag, so a digest made before it is a scope that changed.
const DIGEST_TAG: &[u8] = b"ngr-comm-log-scope-v1";

// ---------------------------------------------------------------------------------------------
// The shape of a request

/// Which rule of the request failed before the database was asked. Each is a 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeError {
    ContentType,
    Json,
    JobIds,
    ScopeDigest,
    Query,
}

impl ShapeError {
    fn title(self) -> &'static str {
        match self {
            ShapeError::ContentType => "Content-Type must be application/json",
            ShapeError::Json => "The body is not the expected JSON object",
            ShapeError::JobIds => "jobIds must be 1 to 32 distinct lowercase UUIDs",
            ShapeError::ScopeDigest => "scopeDigest must be 64 lowercase hexadecimal digits",
            ShapeError::Query => "The query must be 1 to 32 distinct jobId parameters and no other",
        }
    }
}

/// The job IDs of a request, sorted: 1 to [`MAX_JOBS`], each a canonical UUID, none twice. One
/// UUID has one spelling, so comparing the texts finds every repeat.
fn job_ids(mut ids: Vec<String>) -> Option<Vec<String>> {
    if !(1..=MAX_JOBS).contains(&ids.len()) || !ids.iter().all(|id| is_canonical_uuid(id)) {
        return None;
    }
    ids.sort_unstable();
    ids.windows(2).all(|pair| pair[0] != pair[1]).then_some(ids)
}

/// The job IDs of the scope read: the query is `jobId=<uuid>` once per job and nothing else.
/// Nothing is percent-decoded: a canonical UUID needs no escape.
pub(crate) fn shape_query(query: Option<&str>) -> Result<Vec<String>, ShapeError> {
    let ids = query
        .unwrap_or_default()
        .split('&')
        .map(|part| part.strip_prefix("jobId=").map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or(ShapeError::Query)?;
    job_ids(ids).ok_or(ShapeError::Query)
}

/// The two properties of the request body, as typed; nothing is judged yet.
#[derive(Default)]
struct Raw {
    job_ids: Option<Vec<String>>,
    scope_digest: Option<String>,
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Raw, D::Error> {
        struct RawVisitor;

        impl<'de> Visitor<'de> for RawVisitor {
            type Value = Raw;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a log request object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
                let mut raw = Raw::default();
                while let Some(name) = map.next_key::<String>()? {
                    match name.as_str() {
                        "jobIds" => put_once(&mut raw.job_ids, map.next_value()?)?,
                        "scopeDigest" => put_once(&mut raw.scope_digest, map.next_value()?)?,
                        _ => return Err(de::Error::custom("unknown property")),
                    }
                }
                Ok(raw)
            }
        }

        deserializer.deserialize_map(RawVisitor)
    }
}

/// Reads the body as one object of the two known properties, each once and none null, with
/// nothing after it.
fn parse(body: &[u8]) -> Option<Raw> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let raw = Raw::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(raw)
}

/// The Content-Type and the body of a request: the sorted job IDs and the digest it names.
pub(crate) fn shape(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(Vec<String>, [u8; 32]), ShapeError> {
    if !is_json(headers) {
        return Err(ShapeError::ContentType);
    }
    let raw = parse(body).ok_or(ShapeError::Json)?;
    let ids = raw.job_ids.and_then(job_ids).ok_or(ShapeError::JobIds)?;
    let digest = raw
        .scope_digest
        .as_deref()
        .and_then(parse_digest)
        .ok_or(ShapeError::ScopeDigest)?;
    Ok((ids, digest))
}

// ---------------------------------------------------------------------------------------------
// The scope

/// A time as the database holds it: microseconds since the epoch, for the digest, and the date-time
/// text the answer shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    micros: i64,
    text: String,
}

/// One job of a scope, as the one read returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeJob {
    id: String,
    /// The ownership generation of the stored instruction.
    generation: i64,
    agent_id: String,
    kind: String,
    vin: Option<String>,
    created: Stamp,
    ended: Option<Stamp>,
    /// The agent acknowledged the job's delivery.
    delivered: bool,
    /// The job's agent is revoked, or the caller is no longer on the agent's allow-list.
    barred: bool,
    /// The state of the job's `commLog` object at this generation: the live object's state, else
    /// `failed` when only failed attempts exist, else `none`.
    log: String,
    /// The request row of this generation, if there is one.
    request: Option<RequestState>,
}

/// One statement reads every job of the caller among `$3` (`$1` tenant, `$2` operator) with what
/// the scope shows: the job's agent, its `commLog` objects and its request rows at every
/// generation (the generation is in the stored instruction, which only this code decodes).
const SCOPE_SQL: &str = "SELECT j.id::text, j.agent_id::text, j.kind, j.vin, \
         (extract(epoch FROM j.created_at) * 1000000)::bigint, \
         to_char(j.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
         (extract(epoch FROM j.finished_at) * 1000000)::bigint, \
         to_char(j.finished_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
         j.delivered_at IS NOT NULL, (a.revoked_at IS NOT NULL OR NOT EXISTS (SELECT 1 FROM agent_allowed_operators l \
             WHERE l.tenant_id = j.tenant_id AND l.agent_id = j.agent_id AND l.operator_id = $2::uuid)), \
         j.signed_job, \
         coalesce(o.generations, '{}'), coalesce(o.states, '{}'), \
         coalesce(r.generations, '{}'), coalesce(r.states, '{}'), coalesce(r.refusals, '{}') \
     FROM jobs j JOIN agents a ON a.id = j.agent_id \
     LEFT JOIN LATERAL ( \
         SELECT array_agg(o.generation ORDER BY o.id) AS generations, \
                array_agg(o.state ORDER BY o.id) AS states \
         FROM job_objects o WHERE o.tenant_id = j.tenant_id AND o.job_id = j.id \
           AND o.kind = 'commLog') o ON true \
     LEFT JOIN LATERAL ( \
         SELECT array_agg(q.generation ORDER BY q.generation) AS generations, \
                array_agg(q.state ORDER BY q.generation) AS states, \
                array_agg(coalesce(q.refusal, '') ORDER BY q.generation) AS refusals \
         FROM comm_log_requests q WHERE q.tenant_id = j.tenant_id AND q.job_id = j.id) r ON true \
     WHERE j.tenant_id = $1::uuid AND j.operator_id = $2::uuid AND j.id = ANY($3::uuid[])";

type ScopeRow = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    Option<i64>,
    Option<String>,
    bool,
    bool,
    Vec<u8>,
    Vec<i64>,
    Vec<String>,
    Vec<i64>,
    Vec<String>,
    Vec<String>,
);

/// The state of the job's `commLog` at `generation` from the generation and state of each of its
/// objects (see [`ScopeJob::log`]).
fn log_state(generations: &[i64], states: &[String], generation: i64) -> String {
    let mut seen = "none";
    for (_, state) in generations
        .iter()
        .zip(states)
        .filter(|(g, _)| **g == generation)
    {
        if state != "failed" {
            return state.clone();
        }
        seen = "failed";
    }
    seen.to_owned()
}

impl ScopeJob {
    /// The job of a row, or `None` when its stored instruction does not decode (or names a
    /// generation the table cannot hold).
    fn from_row(row: ScopeRow) -> Option<ScopeJob> {
        let (id, agent_id, kind, vin, created_us, created, ended_us, ended) =
            (row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7);
        let generation = i64::try_from(decode_instruction(&row.10)?.ownership_generation).ok()?;
        let request = row
            .13
            .into_iter()
            .zip(row.14)
            .zip(row.15)
            .find(|((g, _), _)| *g == generation)
            .map(|((_, state), refusal)| RequestState {
                state,
                refusal: Some(refusal).filter(|r| !r.is_empty()),
            });
        Some(ScopeJob {
            log: log_state(&row.11, &row.12, generation),
            id,
            generation,
            agent_id,
            kind,
            vin,
            created: Stamp {
                micros: created_us,
                text: created,
            },
            ended: ended_us
                .zip(ended)
                .map(|(micros, text)| Stamp { micros, text }),
            delivered: row.8,
            barred: row.9,
            request,
        })
    }
}

/// Why a job cannot be requested now, from the first rule it fails, in the order the answers
/// are given when several jobs differ: a revoked agent or an allow-list without the caller, a job never delivered, a job not ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Refusal {
    Barred,
    NotDelivered,
    NotEnded,
}

/// The rule both routes judge a job by: the job's agent is not revoked and still lists the caller, the agent acknowledged its
/// delivery (a job the server ended before then, or never sent, holds no log) and the job has
/// ended (a running job's log is not final).
fn refusal(job: &ScopeJob) -> Option<Refusal> {
    if job.barred {
        Some(Refusal::Barred)
    } else if !job.delivered {
        Some(Refusal::NotDelivered)
    } else if job.ended.is_none() {
        Some(Refusal::NotEnded)
    } else {
        None
    }
}

/// The class `POST` answers for a scope: the first of [`Refusal`]'s order that any job has.
fn worst(jobs: &[ScopeJob]) -> Option<Refusal> {
    jobs.iter().filter_map(refusal).min()
}

/// The SHA-256 of a scope over the tag, the number of jobs and each job in ascending job-ID order:
/// the ID, the generation, the agent, the kind, the VIN (presence, then value), the creation time
/// and the end time (presence, then value). A text takes a 4-byte big-endian length before it,
/// as a job request's hash does; an integer is 8 bytes big-endian, the times being microseconds
/// since the epoch.
fn scope_digest(jobs: &[ScopeJob]) -> [u8; 32] {
    let mut ordered: Vec<&ScopeJob> = jobs.iter().collect();
    ordered.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    let mut hasher = Sha256::new();
    put_text(&mut hasher, DIGEST_TAG);
    hasher.update((ordered.len() as u32).to_be_bytes());
    for job in ordered {
        put_text(&mut hasher, job.id.as_bytes());
        hasher.update(job.generation.to_be_bytes());
        put_text(&mut hasher, job.agent_id.as_bytes());
        put_text(&mut hasher, job.kind.as_bytes());
        match &job.vin {
            Some(vin) => {
                hasher.update([1]);
                put_text(&mut hasher, vin.as_bytes());
            }
            None => hasher.update([0]),
        }
        hasher.update(job.created.micros.to_be_bytes());
        match &job.ended {
            Some(ended) => {
                hasher.update([1]);
                hasher.update(ended.micros.to_be_bytes());
            }
            None => hasher.update([0]),
        }
    }
    hasher.finalize().into()
}

/// Why a call ended before it had an answer for the scope.
enum Stop {
    /// The caller does not hold the role `worker` with an empty scope.
    NotWorker,
    /// A named job is not the caller's, or does not exist.
    NoSuchJob,
    /// A stored instruction does not decode.
    Undecodable,
    Failed(JobError),
}

impl From<JobError> for Stop {
    fn from(error: JobError) -> Stop {
        Stop::Failed(error)
    }
}

impl From<sqlx::Error> for Stop {
    fn from(error: sqlx::Error) -> Stop {
        Stop::Failed(error.into())
    }
}

/// The caller's permission, then the scope of `ids` in ascending job-ID order, in the caller's
/// transaction: the one read both routes share.
async fn load(
    tx: &mut PgConnection,
    operator: &SessionOperator,
    ids: &[String],
) -> Result<Vec<ScopeJob>, Stop> {
    let worker: bool = sqlx::query_scalar(concat!("SELECT ", db::worker_unscoped!()))
        .bind(&operator.tenant_id)
        .bind(&operator.operator_id)
        .fetch_one(&mut *tx)
        .await?;
    if !worker {
        return Err(Stop::NotWorker);
    }
    let rows: Vec<ScopeRow> = sqlx::query_as(SCOPE_SQL)
        .bind(&operator.tenant_id)
        .bind(&operator.operator_id)
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
    if rows.len() != ids.len() {
        return Err(Stop::NoSuchJob);
    }
    let mut jobs = rows
        .into_iter()
        .map(ScopeJob::from_row)
        .collect::<Option<Vec<_>>>()
        .ok_or(Stop::Undecodable)?;
    jobs.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    Ok(jobs)
}

// ---------------------------------------------------------------------------------------------
// The answers

fn request_document(request: Option<&RequestState>) -> Value {
    match request {
        None => Value::Null,
        Some(RequestState { state, refusal }) => match refusal {
            Some(refusal) => json!({ "state": state, "refusal": refusal }),
            None => json!({ "state": state }),
        },
    }
}

/// The `period` of a scope: the earliest creation to the latest end, the end being null while a
/// job of the scope has not ended.
fn period(jobs: &[ScopeJob]) -> Value {
    let from = jobs.iter().map(|job| &job.created).min_by_key(|s| s.micros);
    let to = jobs
        .iter()
        .map(|job| job.ended.as_ref())
        .collect::<Option<Vec<_>>>()
        .and_then(|ended| ended.into_iter().max_by_key(|s| s.micros));
    json!({ "from": from.map(|s| &s.text), "to": to.map(|s| &s.text) })
}

fn scope_document(jobs: &[ScopeJob]) -> Value {
    let listed: Vec<Value> = jobs
        .iter()
        .map(|job| {
            json!({
                "jobId": job.id,
                "generation": job.generation,
                "agentId": job.agent_id,
                "kind": job.kind,
                "vin": job.vin,
                "created": job.created.text,
                "ended": job.ended.as_ref().map(|s| &s.text),
                "log": { "state": job.log },
                "request": request_document(job.request.as_ref()),
                "requestable": refusal(job).is_none(),
            })
        })
        .collect();
    json!({
        "jobs": listed,
        "period": period(jobs),
        "scopeDigest": hex(&scope_digest(jobs)),
    })
}

/// An answer that is not cached: it holds VINs or the state of requests.
fn no_store(status: StatusCode, body: &Value) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn bad_request(error: ShapeError) -> Response {
    Problem::new(StatusCode::BAD_REQUEST, error.title()).into_response()
}

// ---------------------------------------------------------------------------------------------
// The routes

/// The route's state: the pool and the log gates of this router.
#[derive(Clone)]
pub struct CommLogUploads(Arc<Inner>);

impl std::fmt::Debug for CommLogUploads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommLogUploads").finish_non_exhaustive()
    }
}

struct Inner {
    pool: PgPool,
    gates: [Mutex<Option<Instant>>; 4],
}

/// Why the routes warn; each has its own log gate, so a flood of one cannot silence another.
#[derive(Clone, Copy)]
enum Warning {
    /// The caller lacks the role (the reason of a 403).
    NotWorker = 0,
    /// A job's agent is revoked or does not list the caller (a 403).
    NotPermitted = 1,
    /// The database is unavailable (503).
    Unavailable = 2,
    /// A statement or a stored instruction failed in this code (500).
    Internal = 3,
}

impl CommLogUploads {
    pub fn new(pool: PgPool) -> CommLogUploads {
        CommLogUploads(Arc::new(Inner {
            pool,
            gates: std::array::from_fn(|_| Mutex::new(None)),
        }))
    }
}

/// What the transaction of a request decided.
enum Outcome {
    /// Written; each job's request state after the write, in job-ID order.
    Written(Vec<(String, i64, Option<RequestState>)>),
    Refused(Refusal),
    /// The digest is not the scope's.
    Changed,
}

impl Inner {
    fn warn(&self, why: Warning, operator: &SessionOperator, what: &str) {
        if may_log(&self.gates[why as usize], Instant::now(), LOG_EVERY) {
            match why {
                Warning::Internal => {
                    tracing::error!(operator_id = %operator.operator_id, "{what}");
                }
                _ => tracing::warn!(operator_id = %operator.operator_id, "{what}"),
            }
        }
    }

    /// The answer for a call that stopped before its scope was known.
    fn stopped(&self, operator: &SessionOperator, stop: Stop) -> Response {
        match stop {
            Stop::NotWorker => {
                self.warn(
                    Warning::NotWorker,
                    operator,
                    "log request refused: the caller is not a worker with an empty scope",
                );
                Problem::new(StatusCode::FORBIDDEN, "Not permitted to request a log")
                    .into_response()
            }
            Stop::NoSuchJob => Problem::new(StatusCode::NOT_FOUND, "No such job").into_response(),
            Stop::Undecodable => {
                self.warn(
                    Warning::Internal,
                    operator,
                    "a stored job instruction does not decode",
                );
                internal()
            }
            Stop::Failed(JobError::Unavailable(what)) => {
                self.warn(
                    Warning::Unavailable,
                    operator,
                    &format!("log request unavailable: {what}"),
                );
                Problem::new(StatusCode::SERVICE_UNAVAILABLE, "Log requests unavailable")
                    .into_response()
            }
            Stop::Failed(JobError::Internal(what)) => {
                self.warn(
                    Warning::Internal,
                    operator,
                    &format!("log request failed: {what}"),
                );
                internal()
            }
        }
    }

    async fn scope(
        &self,
        operator: &SessionOperator,
        ids: &[String],
    ) -> Result<Vec<ScopeJob>, Stop> {
        let mut tx = self.pool.begin().await?;
        db::statement_timeouts(&mut tx).await?;
        // The transaction is dropped, which rolls it back: nothing is written.
        load(&mut tx, operator, ids).await
    }

    /// The transaction of `POST`. Read committed is stated, not assumed. Any early return drops
    /// the transaction, which rolls back.
    async fn request(
        &self,
        operator: &SessionOperator,
        ids: &[String],
        digest: &[u8; 32],
    ) -> Result<Outcome, Stop> {
        let mut tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await?;
        db::statement_timeouts(&mut tx).await?;
        let jobs = load(&mut tx, operator, ids).await?;
        // Eligibility is judged before the digest, so a scope that cannot be requested is not
        // told it changed.
        // The agents' rows are held shared until the commit, in ID order, so that a revocation
        // waits for this request or has committed before this read (as job creation does).
        let mut agents: Vec<&str> = jobs.iter().map(|job| job.agent_id.as_str()).collect();
        agents.sort_unstable();
        agents.dedup();
        let held: Vec<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM agents WHERE id = ANY($1::uuid[]) AND tenant_id = $2::uuid \
               AND revoked_at IS NULL ORDER BY id FOR SHARE",
        )
        .bind(&agents)
        .bind(&operator.tenant_id)
        .fetch_all(&mut *tx)
        .await?;
        if held.len() != agents.len() {
            return Ok(Outcome::Refused(Refusal::Barred));
        }
        if let Some(refusal) = worst(&jobs) {
            return Ok(Outcome::Refused(refusal));
        }
        if scope_digest(&jobs) != *digest {
            return Ok(Outcome::Changed);
        }
        let tenant = operator.tenant_id.as_str();
        let who = operator.operator_id.as_str();
        let mut done = Vec::with_capacity(jobs.len());
        for job in jobs {
            // A stored log is not replaced (ADR-508 item 6): the statements leave the row of a
            // job whose log is stored as it is, also when it was stored since the scope read.
            let written =
                comm_log_requests::request(&mut tx, tenant, &job.id, job.generation, who).await?;
            let state = match written {
                Written::Inserted | Written::Renewed => {
                    audit::comm_log_upload_requested(&mut tx, tenant, who, &job.id, job.generation)
                        .await?;
                    Some(RequestState {
                        state: "requested".to_owned(),
                        refusal: None,
                    })
                }
                Written::Left => {
                    comm_log_requests::state(&mut tx, tenant, &job.id, job.generation).await?
                }
            };
            done.push((job.id, job.generation, state));
        }
        tx.commit().await?;
        Ok(Outcome::Written(done))
    }
}

fn internal() -> Response {
    Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "The request failed").into_response()
}

/// `GET /api/v1/comm-log-uploads/scope?jobId=...`: 200 with the jobs, the period and the digest
/// of the scope. It writes nothing.
pub async fn scope(
    State(uploads): State<CommLogUploads>,
    Extension(operator): Extension<SessionOperator>,
    request: Request,
) -> Response {
    let inner = &*uploads.0;
    let ids = match shape_query(request.uri().query()) {
        Ok(ids) => ids,
        Err(error) => return bad_request(error),
    };
    match inner.scope(&operator, &ids).await {
        Ok(jobs) => no_store(StatusCode::OK, &scope_document(&jobs)),
        Err(stop) => inner.stopped(&operator, stop),
    }
}

/// `POST /api/v1/comm-log-uploads`: 202 once the request is written for every job of the scope.
pub async fn create(
    State(uploads): State<CommLogUploads>,
    Extension(operator): Extension<SessionOperator>,
    request: Request,
) -> Response {
    let inner = &*uploads.0;
    let (parts, body) = request.into_parts();
    let bytes = match bounded_body_within(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let (ids, digest) = match shape(&parts.headers, &bytes) {
        Ok(shaped) => shaped,
        Err(error) => return bad_request(error),
    };
    match inner.request(&operator, &ids, &digest).await {
        Ok(Outcome::Written(done)) => {
            let jobs: Vec<Value> = done
                .iter()
                .map(|(id, generation, state)| {
                    json!({
                        "jobId": id,
                        "generation": generation,
                        "request": request_document(state.as_ref()),
                    })
                })
                .collect();
            let job_ids: Vec<&str> = done.iter().map(|(id, ..)| id.as_str()).collect();
            tracing::info!(
                operator_id = %operator.operator_id,
                job_ids = %job_ids.join(","),
                "log requested"
            );
            no_store(StatusCode::ACCEPTED, &json!({ "jobs": jobs }))
        }
        Ok(Outcome::Refused(Refusal::Barred)) => {
            inner.warn(
                Warning::NotPermitted,
                &operator,
                "log request refused: the agent of a job is revoked or does not list the caller",
            );
            Problem::new(StatusCode::FORBIDDEN, "Not permitted to request a log").into_response()
        }
        Ok(Outcome::Refused(Refusal::NotDelivered)) => Problem::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "A job was never delivered to its agent",
            "JOB_NOT_DELIVERED",
        )
        .into_response(),
        Ok(Outcome::Refused(Refusal::NotEnded)) => Problem::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "A job has not ended",
            "JOB_NOT_ENDED",
        )
        .into_response(),
        Ok(Outcome::Changed) => Problem::with_code(
            StatusCode::CONFLICT,
            "The scope is not the one shown",
            "SCOPE_CHANGED",
        )
        .into_response(),
        Err(stop) => inner.stopped(&operator, stop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01";
    const B: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d02";
    const AGENT: &str = "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab";
    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers
    }

    fn post(body: Value) -> Result<(Vec<String>, [u8; 32]), ShapeError> {
        shape(&json_headers(), body.to_string().as_bytes())
    }

    fn post_raw(body: &str) -> Result<(Vec<String>, [u8; 32]), ShapeError> {
        shape(&json_headers(), body.as_bytes())
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("0194f0a2-7c3e-7d11-8a52-3f6f9b1c{i:04x}"))
            .collect()
    }

    fn stamp(micros: i64) -> Stamp {
        Stamp {
            micros,
            text: format!("t{micros}"),
        }
    }

    fn job(id: &str) -> ScopeJob {
        ScopeJob {
            id: id.to_owned(),
            generation: 1,
            agent_id: AGENT.to_owned(),
            kind: "acquire".to_owned(),
            vin: Some("1HGCM82633A004352".to_owned()),
            created: stamp(1_000_000),
            ended: Some(stamp(2_000_000)),
            delivered: true,
            barred: false,
            log: "none".to_owned(),
            request: None,
        }
    }

    // S1, S3, S4, S5
    #[test]
    fn a_query_names_one_to_32_distinct_canonical_job_ids_and_nothing_else() {
        let query = |ids: &[String]| {
            let text: Vec<String> = ids.iter().map(|id| format!("jobId={id}")).collect();
            shape_query(Some(&text.join("&")))
        };
        assert_eq!(query(&ids(1)).map(|v| v.len()), Ok(1));
        assert_eq!(query(&ids(32)).map(|v| v.len()), Ok(32));
        assert_eq!(query(&ids(33)), Err(ShapeError::Query));
        assert_eq!(shape_query(None), Err(ShapeError::Query));
        assert_eq!(shape_query(Some("")), Err(ShapeError::Query));
        let mut twice = ids(2);
        twice.push(ids(2)[0].clone());
        assert_eq!(query(&twice), Err(ShapeError::Query));
        for bad in [
            "jobId=",
            "jobId=0194F0A2-7C3E-7D11-8A52-3F6F9B1C2D01",
            "jobId={0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01}",
            "jobId=0194f0a27c3e7d118a523f6f9b1c2d01",
            "jobId=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d0",
            "jobid=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01",
            "jobId=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01&other=1",
            "jobId=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01&",
            "jobId=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01&&jobId=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d02",
            "other=0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d01",
        ] {
            assert_eq!(shape_query(Some(bad)), Err(ShapeError::Query), "{bad}");
        }
    }

    // G1: the IDs are sorted, so the order a client names them in does not matter.
    #[test]
    fn the_ids_of_a_request_come_back_sorted() {
        let (got, _) = post(json!({ "jobIds": [B, A], "scopeDigest": DIGEST })).unwrap();
        assert_eq!(got, [A, B]);
    }

    // S1, S3, S4, S5, S6, S7
    #[test]
    fn the_body_is_one_object_of_two_properties_judged_in_order() {
        let ok = |v: Value| post(v).map(|(ids, _)| ids.len());
        assert_eq!(
            ok(json!({ "jobIds": ids(1), "scopeDigest": DIGEST })),
            Ok(1)
        );
        assert_eq!(
            ok(json!({ "jobIds": ids(32), "scopeDigest": DIGEST })),
            Ok(32)
        );
        assert_eq!(
            ok(json!({ "jobIds": ids(33), "scopeDigest": DIGEST })),
            Err(ShapeError::JobIds)
        );
        for (bad, error) in [
            (
                json!({ "jobIds": [], "scopeDigest": DIGEST }),
                ShapeError::JobIds,
            ),
            (json!({ "scopeDigest": DIGEST }), ShapeError::JobIds),
            (
                json!({ "jobIds": [A, A], "scopeDigest": DIGEST }),
                ShapeError::JobIds,
            ),
            (
                json!({ "jobIds": [A.to_uppercase()], "scopeDigest": DIGEST }),
                ShapeError::JobIds,
            ),
            (
                json!({ "jobIds": [""], "scopeDigest": DIGEST }),
                ShapeError::JobIds,
            ),
            // A value of the wrong type is a body that is not the expected object.
            (
                json!({ "jobIds": null, "scopeDigest": DIGEST }),
                ShapeError::Json,
            ),
            (
                json!({ "jobIds": A, "scopeDigest": DIGEST }),
                ShapeError::Json,
            ),
        ] {
            assert_eq!(ok(bad.clone()), Err(error), "{bad}");
        }
    }

    // S6
    #[test]
    fn the_digest_is_64_lowercase_hex_digits() {
        let with = |digest: Value| post(json!({ "jobIds": [A], "scopeDigest": digest }));
        assert!(with(json!(DIGEST)).is_ok());
        for bad in [
            json!(&DIGEST[..63]),
            json!(format!("{DIGEST}a")),
            json!(DIGEST.to_uppercase()),
            json!(format!("sha256:{DIGEST}")),
            json!(""),
        ] {
            assert_eq!(with(bad.clone()), Err(ShapeError::ScopeDigest), "{bad}");
        }
        for bad in [json!(null), json!(7), json!([DIGEST])] {
            assert_eq!(with(bad.clone()), Err(ShapeError::Json), "{bad}");
        }
        assert_eq!(
            post(json!({ "jobIds": [A] })),
            Err(ShapeError::ScopeDigest),
            "missing"
        );
    }

    // S7
    #[test]
    fn an_unknown_or_repeated_property_trailing_bytes_and_a_wrong_type_are_refused() {
        let good = format!(r#"{{"jobIds":["{A}"],"scopeDigest":"{DIGEST}"}}"#);
        assert!(post_raw(&good).is_ok());
        for bad in [
            format!(r#"{{"jobIds":["{A}"],"scopeDigest":"{DIGEST}","extra":1}}"#),
            format!(r#"{{"jobIds":["{A}"],"jobIds":["{B}"],"scopeDigest":"{DIGEST}"}}"#),
            format!(r#"{{"jobIds":["{A}"],"scopeDigest":"{DIGEST}","scopeDigest":"{DIGEST}"}}"#),
            format!("{good} x"),
            format!("{good}{good}"),
            format!(r#"{{"jobIds":[1],"scopeDigest":"{DIGEST}"}}"#),
            format!(r#"["{A}","{DIGEST}"]"#),
            "not json".to_owned(),
            String::new(),
        ] {
            assert_eq!(post_raw(&bad), Err(ShapeError::Json), "{bad}");
        }
        let mut other = json_headers();
        other.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert_eq!(shape(&other, good.as_bytes()), Err(ShapeError::ContentType));
        assert_eq!(
            shape(&HeaderMap::new(), good.as_bytes()),
            Err(ShapeError::ContentType)
        );
    }

    // G11: the encoding, against a preimage written out byte by byte.
    #[test]
    fn the_digest_is_the_sha256_of_the_tagged_length_prefixed_encoding() {
        let mut jobs = vec![job(A)];
        jobs[0].ended = None;
        let mut preimage = Vec::new();
        let text = |out: &mut Vec<u8>, t: &str| {
            out.extend((t.len() as u32).to_be_bytes());
            out.extend(t.as_bytes());
        };
        text(&mut preimage, "ngr-comm-log-scope-v1");
        preimage.extend(1u32.to_be_bytes());
        text(&mut preimage, A);
        preimage.extend(1i64.to_be_bytes());
        text(&mut preimage, AGENT);
        text(&mut preimage, "acquire");
        preimage.push(1);
        text(&mut preimage, "1HGCM82633A004352");
        preimage.extend(1_000_000i64.to_be_bytes());
        preimage.push(0);
        let expected: [u8; 32] = Sha256::digest(&preimage).into();
        assert_eq!(scope_digest(&jobs), expected);
        // The same scope, pinned: a change of the encoding changes this and needs a new tag.
        assert_eq!(
            hex(&scope_digest(&jobs)),
            "8cad6e6847f07c5fd8f305d149eb0f4d6d57b419bbcf49c727e61f01c4a5f5a7"
        );
    }

    // G1, G9, G11
    type Change = Box<dyn Fn(&mut ScopeJob)>;

    #[test]
    fn the_digest_ignores_the_order_and_changes_with_each_field_alone() {
        let (a, b) = (job(A), job(B));
        let base = scope_digest(&[a.clone(), b.clone()]);
        assert_eq!(scope_digest(&[b.clone(), a.clone()]), base, "order");
        let changed: Vec<(&str, Change)> = vec![
            ("generation", Box::new(|j| j.generation = 2)),
            ("agent", Box::new(|j| j.agent_id = B.to_owned())),
            ("kind", Box::new(|j| j.kind = "writeSettings".to_owned())),
            (
                "vin",
                Box::new(|j| j.vin = Some("WVWZZZ1JZXW000001".to_owned())),
            ),
            ("no vin", Box::new(|j| j.vin = None)),
            ("empty vin", Box::new(|j| j.vin = Some(String::new()))),
            ("created", Box::new(|j| j.created = stamp(1_000_001))),
            ("ended", Box::new(|j| j.ended = Some(stamp(2_000_001)))),
            ("not ended", Box::new(|j| j.ended = None)),
        ];
        for (what, change) in &changed {
            let mut other = a.clone();
            change(&mut other);
            assert_ne!(scope_digest(&[other, b.clone()]), base, "{what}");
        }
        // The state of the log and of the request, the delivery and the agent's revocation are
        // not part of what the digest says.
        let mut same = a.clone();
        same.log = "finished".to_owned();
        same.request = Some(RequestState {
            state: "accepted".to_owned(),
            refusal: None,
        });
        same.delivered = false;
        same.barred = true;
        assert_eq!(scope_digest(&[same, b.clone()]), base);
        // A byte that moves from one field to the next, and a job taken or added, change it.
        let (mut x, mut y) = (a.clone(), a.clone());
        x.agent_id = format!("{AGENT}x");
        x.kind = "cquire".to_owned();
        y.agent_id = AGENT.to_owned();
        y.kind = "xcquire".to_owned();
        assert_ne!(scope_digest(&[x]), scope_digest(&[y]));
        assert_ne!(scope_digest(std::slice::from_ref(&a)), base);
        assert_ne!(scope_digest(&[a, b, job(&ids(1)[0])]), base);
    }

    // J12
    #[test]
    fn the_class_of_a_refusal_does_not_depend_on_the_order_of_the_jobs() {
        let ok = job(A);
        let mut revoked = job(B);
        revoked.barred = true;
        revoked.delivered = false;
        let mut undelivered = job(&ids(1)[0]);
        undelivered.delivered = false;
        undelivered.ended = None;
        let mut running = job(&ids(2)[1]);
        running.ended = None;
        assert_eq!(worst(std::slice::from_ref(&ok)), None);
        assert_eq!(refusal(&revoked), Some(Refusal::Barred));
        assert_eq!(refusal(&undelivered), Some(Refusal::NotDelivered));
        assert_eq!(refusal(&running), Some(Refusal::NotEnded));
        // The class is the first of the fixed order that any job has, in any position.
        let all = [ok, revoked, undelivered, running];
        let mut backwards = all.clone();
        backwards.reverse();
        assert_eq!(worst(&backwards), Some(Refusal::Barred));
        assert_eq!(worst(&all), Some(Refusal::Barred));
        assert_eq!(worst(&all[2..]), Some(Refusal::NotDelivered));
        assert_eq!(worst(&all[3..]), Some(Refusal::NotEnded));
    }

    // L1, L2, L3
    #[test]
    fn the_log_state_is_the_live_object_of_the_generation_else_failed_else_none() {
        let state = |g: &[i64], s: &[&str]| {
            let s: Vec<String> = s.iter().map(|s| (*s).to_owned()).collect();
            log_state(g, &s, 1)
        };
        assert_eq!(state(&[], &[]), "none");
        assert_eq!(state(&[1, 1], &["failed", "failed"]), "failed");
        for live in ["open", "completing", "finished"] {
            assert_eq!(state(&[1], &[live]), live);
            assert_eq!(state(&[1, 1], &["failed", live]), live);
            assert_eq!(state(&[1, 1], &[live, "failed"]), live);
        }
        // Another generation's object is ignored.
        assert_eq!(state(&[2], &["finished"]), "none");
        assert_eq!(state(&[2, 1], &["finished", "failed"]), "failed");
    }

    // L6
    #[test]
    fn the_period_runs_from_the_first_creation_to_the_last_end_and_is_open_while_a_job_runs() {
        let (mut a, mut b) = (job(A), job(B));
        a.created = stamp(5);
        a.ended = Some(stamp(10));
        b.created = stamp(3);
        b.ended = Some(stamp(20));
        assert_eq!(
            period(&[a.clone(), b.clone()]),
            json!({ "from": "t3", "to": "t20" })
        );
        b.ended = None;
        assert_eq!(period(&[a, b]), json!({ "from": "t3", "to": null }));
    }
}

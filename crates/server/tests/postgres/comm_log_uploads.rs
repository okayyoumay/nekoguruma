//! `GET /api/v1/comm-log-uploads/scope` and `POST /api/v1/comm-log-uploads` over real TCP against a
//! throwaway PostgreSQL (design 8.11, 16.2, ADR-455, ADR-508 items 1 and 2). The rules that need
//! no database are unit tests in `src/comm_log_uploads.rs`; the delivery of a written request to
//! a connected agent is `comm_log_requests.rs`.
//!
//! The case names in the comments are the rows of the case table of the step: S for the shape of
//! a request, C for the caller, J for the job, M for what the POST writes, L for what the GET
//! shows, G for the digest, X for concurrency and A for the audit rows. The tests run on the
//! current-thread runtime. Job states, request rows and objects are written by SQL. A negative (a
//! refusal that must leave no row) is asserted after the answer that came first; a wait is
//! polling for a lock the database reports, never a sleep.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use server::local_guard::pinned_origin;
use server::local_user::Hasher;
use tokio::sync::OnceCell;

use crate::control::second_agent;
use crate::jobs::{password, session_cookie};
use crate::redeem::{Agent, json};
use crate::serving::{Reply, exchange, host_header};
use crate::session::Up;
use crate::uploads::signed_job;

const VIN: &str = "1HGCM82633A004352";
const VIN_2: &str = "WVWZZZ1JZXW000001";
/// The states a job has ended in, whoever ended it.
const ENDED: [&str; 6] = [
    "completed",
    "failed",
    "cancelled",
    "expired",
    "preconditionFailed",
    "needsOnSiteRecovery",
];
/// The job states a delivered job is in while it has not ended.
const RUNNING: [&str; 7] = [
    "received",
    "precheck",
    "ready",
    "running",
    "verifying",
    "awaitingHmi",
    "cancelRequested",
];

// ---------------------------------------------------------------------------------------------
// Callers and requests

/// An operator of the tests' tenant with a login session.
pub(crate) struct Who {
    pub(crate) id: String,
    pub(crate) cookie: String,
}

/// What the Web API helpers need of a server under test: where it listens, the database its
/// operators are in, the agent whose tenant they belong to and its operator sessions.
pub(crate) trait Site {
    fn addr(&self) -> SocketAddr;
    fn pool(&self) -> &sqlx::PgPool;
    fn agent_id(&self) -> &str;
    fn operator_sessions(&self) -> &server::OperatorSessions;
}

impl Site for Up {
    fn addr(&self) -> SocketAddr {
        self.s.addr
    }

    fn pool(&self) -> &sqlx::PgPool {
        &self.w.pool
    }

    fn agent_id(&self) -> &str {
        &self.id
    }

    fn operator_sessions(&self) -> &server::OperatorSessions {
        &self.operator_sessions
    }
}

/// The password hash the sessions of these tests are bound to, made once.
async fn hash() -> &'static str {
    static HASH: OnceCell<String> = OnceCell::const_new();
    HASH.get_or_init(|| async { Hasher::new().hash(password().to_owned()).await.unwrap() })
        .await
}

/// A session for the operator `subject` of the agent's tenant, who holds `roles`; the operator
/// is made when there is none. `alice` is the operator `Up` made, the creator of every job
/// these tests make.
pub(crate) async fn who(u: &impl Site, subject: &str, roles: &[&str]) -> Who {
    let (tenant, id): (String, String) = sqlx::query_as(
        "WITH t AS (SELECT tenant_id FROM agents WHERE id = $1::uuid), \
              made AS (INSERT INTO operators (id, tenant_id, subject, display_name) \
                  SELECT gen_random_uuid(), tenant_id, $2, $2 FROM t \
                  WHERE NOT EXISTS (SELECT 1 FROM operators o, t WHERE o.tenant_id = t.tenant_id \
                      AND o.subject = $2) \
                  RETURNING id, tenant_id) \
         SELECT tenant_id::text, id::text FROM made \
         UNION ALL SELECT o.tenant_id::text, o.id::text FROM operators o, t \
             WHERE o.tenant_id = t.tenant_id AND o.subject = $2",
    )
    .bind(u.agent_id())
    .bind(subject)
    .fetch_one(u.pool())
    .await
    .unwrap();
    for role in roles {
        sqlx::query("INSERT INTO operator_roles (tenant_id, operator_id, role) VALUES ($1::uuid, $2::uuid, $3)")
            .bind(&tenant)
            .bind(&id)
            .bind(role)
            .execute(u.pool())
            .await
            .unwrap();
    }
    let hash = hash().await;
    sqlx::query(
        "INSERT INTO operator_credentials (operator_id, password_hash) VALUES ($1::uuid, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&id)
    .bind(hash)
    .execute(u.pool())
    .await
    .unwrap();
    let cookie = session_cookie(u.operator_sessions(), &tenant, &id, hash);
    Who { id, cookie }
}

/// `alice` with the role `worker`.
pub(crate) async fn alice(u: &impl Site) -> Who {
    who(u, "alice", &["worker"]).await
}

fn scope_target(ids: &[&str]) -> String {
    let query: Vec<String> = ids.iter().map(|id| format!("jobId={id}")).collect();
    format!("/api/v1/comm-log-uploads/scope?{}", query.join("&"))
}

fn raw_get(addr: SocketAddr, target: &str, cookie: Option<&str>) -> String {
    let mut raw = format!(
        "GET {target} HTTP/1.1\r\n{}\r\nConnection: close\r\n",
        host_header(addr)
    );
    if let Some(cookie) = cookie {
        raw.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    raw.push_str("\r\n");
    raw
}

/// A POST with the pinned Origin, the session and a JSON body, unless the arguments say less.
fn raw_post(
    addr: SocketAddr,
    cookie: Option<&str>,
    origin: Option<&str>,
    content_type: Option<&str>,
    body: &str,
) -> String {
    let mut raw = format!(
        "POST /api/v1/comm-log-uploads HTTP/1.1\r\n{}\r\nConnection: close\r\n",
        host_header(addr)
    );
    if let Some(origin) = origin {
        raw.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(content_type) = content_type {
        raw.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    if let Some(cookie) = cookie {
        raw.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw
}

/// The request body for `ids` and `digest`.
pub(crate) fn body(ids: &[&str], digest: &str) -> String {
    json!({ "jobIds": ids, "scopeDigest": digest }).to_string()
}

fn raw_request(u: &impl Site, who: &Who, body: &str) -> String {
    raw_post(
        u.addr(),
        Some(&who.cookie),
        Some(&pinned_origin(u.addr().port())),
        Some("application/json"),
        body,
    )
}

pub(crate) async fn scope(u: &impl Site, who: &Who, ids: &[&str]) -> Reply {
    exchange(
        u.addr(),
        &raw_get(u.addr(), &scope_target(ids), Some(&who.cookie)),
    )
    .await
}

pub(crate) async fn post(u: &impl Site, who: &Who, body: &str) -> Reply {
    exchange(u.addr(), &raw_request(u, who, body)).await
}

/// The digest an answer of the scope read carries.
pub(crate) fn digest_of(scope: &Reply) -> String {
    assert_eq!(scope.status, 200, "{} {}", scope.head, scope.body);
    json(scope)["scopeDigest"].as_str().unwrap().to_owned()
}

/// What an operator does: reads the scope of `ids`, then asks for it with the digest it was shown.
pub(crate) async fn request(u: &impl Site, who: &Who, ids: &[&str]) -> Reply {
    let shown = scope(u, who, ids).await;
    post(u, who, &body(ids, &digest_of(&shown))).await
}

/// The answer is a problem document of `status`, with `code` when it has one.
fn problem(r: &Reply, status: u16, code: Option<&str>, what: &str) {
    assert_eq!(r.status, status, "{what}: {} {}", r.head, r.body);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
    let body = json(r);
    assert_eq!(body["code"].as_str(), code, "{what}: {}", r.body);
}

/// The whole answer but its Date, for comparing two answers byte for byte.
fn normalized(r: &Reply) -> String {
    let head: Vec<&str> = r
        .head
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
        .collect();
    format!("{}\n\n{}", head.join("\n"), r.body)
}

// ---------------------------------------------------------------------------------------------
// Jobs, rows and objects

pub(crate) async fn exec(u: &Up, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(&u.w.pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Changes the job's columns: `assignments` is the `SET` list.
pub(crate) async fn set(u: &Up, job: &str, assignments: &str) {
    exec(
        u,
        &format!("UPDATE jobs SET {assignments} WHERE id = '{job}'"),
    )
    .await;
}

/// A job of `agent` that can be requested: delivered and ended, with a stored instruction of the
/// generation 1, created by `alice`.
pub(crate) async fn job_of(u: &Up, agent: &str) -> String {
    signed_job(&u.w, agent, 1, Vec::new(), None, None, true).await
}

/// Makes a job delivered and ended.
pub(crate) async fn end(u: &Up, job: &str) {
    set(
        u,
        job,
        "state = 'completed', progress_permille = 1000, delivered_at = now(), \
         delivery_attempts = 1, finished_at = now()",
    )
    .await;
}

/// A job of the agent `Up` registered that can be requested.
pub(crate) async fn job(u: &Up) -> String {
    job_of(u, &u.id).await
}

/// `n` such jobs, in ascending ID order.
async fn jobs(u: &Up, n: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for _ in 0..n {
        ids.push(job(u).await);
    }
    ids.sort();
    ids
}

fn refs(ids: &[String]) -> Vec<&str> {
    ids.iter().map(String::as_str).collect()
}

/// A request row to write by SQL, for the job's generation.
#[derive(Clone)]
pub(crate) struct Seed<'a> {
    pub(crate) generation: i64,
    pub(crate) state: &'a str,
    /// The refusal of a `refused` row; `noLog` when it has none.
    pub(crate) refusal: Option<&'a str>,
    pub(crate) sent: bool,
    pub(crate) age_secs: i64,
    /// The operator who asked; the job's creator when it has none.
    pub(crate) by: Option<&'a str>,
}

impl<'a> Seed<'a> {
    pub(crate) fn new(state: &'a str) -> Seed<'a> {
        Seed {
            generation: 1,
            state,
            refusal: None,
            sent: false,
            age_secs: 0,
            by: None,
        }
    }

    /// The row, written for the job.
    pub(crate) async fn put(&self, u: &Up, job: &str) {
        sqlx::query(
            "INSERT INTO comm_log_requests (tenant_id, job_id, generation, agent_id, \
                 requested_by, requested_at, state, refusal, sent_at, answered_at) \
             SELECT tenant_id, id, $2, agent_id, coalesce($3::uuid, operator_id), \
                 now() - $4 * interval '1 second', $5, \
                 CASE WHEN $5 = 'refused' THEN coalesce($6, 'noLog') END, \
                 CASE WHEN $7 THEN now() END, \
                 CASE WHEN $5 IN ('accepted', 'refused') THEN now() END \
             FROM jobs WHERE id = $1::uuid",
        )
        .bind(job)
        .bind(self.generation)
        .bind(self.by)
        .bind(self.age_secs)
        .bind(self.state)
        .bind(self.refusal)
        .bind(self.sent)
        .execute(&u.w.pool)
        .await
        .unwrap();
    }
}

/// A `kind` object of the job at `generation` in `state`.
pub(crate) async fn object(u: &Up, job: &str, generation: i64, kind: &str, state: &str) {
    sqlx::query(
        "INSERT INTO job_objects (tenant_id, id, job_id, generation, kind, agent_id, \
             storage_key, upload_id, part_bytes, size_bytes, digest, state, finished_at) \
         SELECT tenant_id, gen_random_uuid(), id, $2, $3, agent_id, \
             gen_random_uuid()::text, 'u', 1048576, 3, repeat('ab', 32), $4, \
             CASE WHEN $4 = 'finished' THEN now() END \
         FROM jobs WHERE id = $1::uuid",
    )
    .bind(job)
    .bind(generation)
    .bind(kind)
    .bind(state)
    .execute(&u.w.pool)
    .await
    .unwrap();
}

/// The job's request row at generation 1 as one line: state, refusal, whether sent, whether
/// answered. Empty when there is none.
pub(crate) async fn row(u: &Up, job: &str) -> String {
    sqlx::query_scalar(
        "SELECT concat_ws('|', state, coalesce(refusal, '-'), (sent_at IS NOT NULL)::text, \
             (answered_at IS NOT NULL)::text) FROM comm_log_requests \
         WHERE job_id = $1::uuid AND generation = 1",
    )
    .bind(job)
    .fetch_optional(&u.w.pool)
    .await
    .unwrap()
    .unwrap_or_default()
}

/// Every column of the job's rows, for showing that a request left a row as it was.
async fn whole(u: &Up, job: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT r::text FROM comm_log_requests r WHERE job_id = $1::uuid ORDER BY generation",
    )
    .bind(job)
    .fetch_all(&u.w.pool)
    .await
    .unwrap()
}

async fn count(u: &Up, table: &'static str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(&u.w.pool)
        .await
        .unwrap()
}

/// Nothing was written: no request row and no audit row of the request.
async fn assert_untouched(u: &Up, what: &str) {
    assert_eq!(count(u, "comm_log_requests").await, 0, "{what}");
    assert_eq!(audits(u).await, 0, "{what}");
}

async fn audits(u: &Up) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'commLogUploadRequested'")
        .fetch_one(&u.w.pool)
        .await
        .unwrap()
}

/// Waits until a statement that begins with `prefix` waits for a lock, as the database reports it.
async fn wait_for_lock(u: &Up, prefix: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while u
        .w
        .sql(&format!(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE '{prefix}%'"
        ))
        .await
        == "0"
    {
        assert!(Instant::now() < deadline, "no `{prefix}` waits for a lock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn spawn_post(u: &Up, who: &Who, body: &str) -> tokio::task::JoinHandle<Reply> {
    let (addr, raw) = (u.s.addr, raw_request(u, who, body));
    tokio::spawn(async move { exchange(addr, &raw).await })
}

fn job_in<'a>(scope: &'a Value, job: &str) -> &'a Value {
    scope["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["jobId"] == job)
        .unwrap_or_else(|| panic!("{job} is not in {scope}"))
}

// ---------------------------------------------------------------------------------------------
// The shape of a request, and the caller

#[tokio::test]
async fn a_request_that_is_not_well_formed_is_a_400_before_the_permission_is_judged() {
    // S1, S4, S5, S6, S7, S9: the caller holds no role, so an answer of 403 would mean the shape
    // was not judged first.
    let u = Up::new().await;
    let viewer = who(&u, "viewer", &["viewer"]).await;
    let id = job(&u).await;
    let digest = "ab".repeat(32);
    let upper = id.to_uppercase();
    for target in [
        "/api/v1/comm-log-uploads/scope".to_owned(),
        "/api/v1/comm-log-uploads/scope?jobId=".to_owned(),
        scope_target(&[&id, &id]),
        scope_target(&[&upper]),
        format!("{}&other=1", scope_target(&[&id])),
        format!("{}&", scope_target(&[&id])),
        scope_target(&["0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d0"]),
    ] {
        let r = exchange(u.s.addr, &raw_get(u.s.addr, &target, Some(&viewer.cookie))).await;
        problem(&r, 400, None, &target);
    }
    let many: Vec<String> = (0..33)
        .map(|i| format!("0194f0a2-7c3e-7d11-8a52-3f6f9b1c{i:04x}"))
        .collect();
    let r = scope(&u, &viewer, &refs(&many)).await;
    problem(&r, 400, None, "33 jobs on GET");
    for (what, text) in [
        ("no ids", body(&[], &digest)),
        ("a repeat", body(&[&id, &id], &digest)),
        ("uppercase", body(&[&upper], &digest)),
        ("33 ids", body(&refs(&many), &digest)),
        ("a short digest", body(&[&id], &digest[..63])),
        ("an uppercase digest", body(&[&id], &digest.to_uppercase())),
        ("a tagged digest", body(&[&id], &format!("sha256:{digest}"))),
        (
            "a null digest",
            format!(r#"{{"jobIds":["{id}"],"scopeDigest":null}}"#),
        ),
        ("no digest", format!(r#"{{"jobIds":["{id}"]}}"#)),
        (
            "an unknown property",
            format!(r#"{{"jobIds":["{id}"],"scopeDigest":"{digest}","x":1}}"#),
        ),
        (
            "a repeated property",
            format!(r#"{{"jobIds":["{id}"],"jobIds":["{id}"],"scopeDigest":"{digest}"}}"#),
        ),
        (
            "trailing bytes",
            format!(r#"{{"jobIds":["{id}"],"scopeDigest":"{digest}"}} x"#),
        ),
        ("not json", "nonsense".to_owned()),
    ] {
        problem(&post(&u, &viewer, &text).await, 400, None, what);
    }
    let origin = pinned_origin(u.s.addr.port());
    let wrong_type = raw_post(
        u.s.addr,
        Some(&viewer.cookie),
        Some(&origin),
        Some("text/plain"),
        &body(&[&id], &digest),
    );
    problem(
        &exchange(u.s.addr, &wrong_type).await,
        400,
        None,
        "Content-Type",
    );
    // A body declared over the bound is a 413 before any byte is read: none is sent.
    let declared = format!(
        "POST /api/v1/comm-log-uploads HTTP/1.1\r\n{}\r\nConnection: close\r\n\
         Origin: {origin}\r\nContent-Type: application/json\r\nCookie: {}\r\n\
         Content-Length: 4097\r\n\r\n",
        host_header(u.s.addr),
        viewer.cookie
    );
    assert_eq!(exchange(u.s.addr, &declared).await.status, 413);
    assert_untouched(&u, "malformed requests").await;
    u.finish().await;
}

#[tokio::test]
async fn a_request_needs_a_session_and_a_post_the_pinned_origin() {
    // C1
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    let r = exchange(u.s.addr, &raw_get(u.s.addr, &scope_target(&[&id]), None)).await;
    problem(&r, 401, None, "GET without a session");
    let text = body(&[&id], &"ab".repeat(32));
    let origin = pinned_origin(u.s.addr.port());
    let no_session = raw_post(
        u.s.addr,
        None,
        Some(&origin),
        Some("application/json"),
        &text,
    );
    problem(
        &exchange(u.s.addr, &no_session).await,
        401,
        None,
        "POST without a session",
    );
    for foreign in [None, Some("http://example.com")] {
        let raw = raw_post(
            u.s.addr,
            Some(&alice.cookie),
            foreign,
            Some("application/json"),
            &text,
        );
        problem(
            &exchange(u.s.addr, &raw).await,
            403,
            None,
            "POST without the pinned Origin",
        );
    }
    assert_untouched(&u, "C1").await;
    u.finish().await;
}

#[tokio::test]
async fn a_caller_without_the_worker_role_is_refused_with_one_403_whatever_the_ids() {
    // C2: the same answer for a job of its own, an unknown job and a malformed-looking-valid one;
    // and for an operator who held the role when the job was made and lost it since.
    let u = Up::new().await;
    let viewer = who(&u, "viewer", &["viewer"]).await;
    let id = job(&u).await;
    let unknown = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";
    let digest = "ab".repeat(32);
    let mut answers = Vec::new();
    for ids in [vec![id.as_str()], vec![unknown]] {
        answers.push(scope(&u, &viewer, &ids).await);
        answers.push(post(&u, &viewer, &body(&ids, &digest)).await);
    }
    for r in &answers {
        problem(r, 403, None, "viewer");
    }
    assert_eq!(normalized(&answers[0]), normalized(&answers[2]), "GET");
    assert_eq!(normalized(&answers[1]), normalized(&answers[3]), "POST");
    let alice = alice(&u).await;
    let shown = scope(&u, &alice, &[&id]).await;
    let ok = digest_of(&shown);
    exec(&u, "DELETE FROM operator_roles WHERE role = 'worker'").await;
    problem(
        &scope(&u, &alice, &[&id]).await,
        403,
        None,
        "role lost, GET",
    );
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        403,
        None,
        "role lost, POST",
    );
    // A worker whose role has a scope is not the unscoped worker the request needs.
    let scoped = who(&u, "scoped", &["worker"]).await;
    exec(
        &u,
        &format!(
            "UPDATE operator_roles SET scope = '{{\"vin\": \"x\"}}'::jsonb WHERE operator_id = '{}'",
            scoped.id
        ),
    )
    .await;
    problem(
        &scope(&u, &scoped, &[&id]).await,
        403,
        None,
        "scoped worker",
    );
    assert_untouched(&u, "C2").await;
    u.finish().await;
}

#[tokio::test]
async fn a_job_that_is_unknown_another_tenants_or_another_operators_is_one_404_and_hides_the_rest()
{
    // C3, J1, J2
    let u = Up::new().await;
    let alice = alice(&u).await;
    let bob = who(&u, "bob", &["worker"]).await;
    let own = job(&u).await;
    // Another tenant's job, made for the operator of that tenant.
    u.w.add_operator("carol").await;
    let agent = Agent::new();
    let (_, redeemed) = u.w.redeem("carol", &agent).await;
    assert_eq!(redeemed.status, 201, "{}", redeemed.head);
    let foreign_agent = json(&redeemed)["agentId"].as_str().unwrap().to_owned();
    let foreign = job_of(&u, &foreign_agent).await;
    let unknown = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";
    let digest = "ab".repeat(32);
    let mut gets = Vec::new();
    let mut posts = Vec::new();
    // Another operator of the tenant asks for alice's job; alice for an unknown and a foreign one.
    for (who, id) in [
        (&bob, &own),
        (&alice, &unknown.to_owned()),
        (&alice, &foreign),
    ] {
        gets.push(scope(&u, who, &[id]).await);
        posts.push(post(&u, who, &body(&[id], &digest)).await);
    }
    for r in gets.iter().chain(&posts) {
        problem(r, 404, None, "not the caller's job");
    }
    assert!(
        gets.iter().all(|r| normalized(r) == normalized(&gets[0])),
        "GET"
    );
    assert!(
        posts.iter().all(|r| normalized(r) == normalized(&posts[0])),
        "POST"
    );
    // J2: one job that could be requested and one that is not the caller's: nothing is shown or
    // written.
    let shown = scope(&u, &alice, &[&own]).await;
    let ok = digest_of(&shown);
    let mixed = {
        let mut ids = [own.as_str(), unknown];
        ids.sort_unstable();
        ids
    };
    problem(&scope(&u, &alice, &mixed).await, 404, None, "mixed GET");
    problem(
        &post(&u, &alice, &body(&mixed, &ok)).await,
        404,
        None,
        "mixed POST",
    );
    assert_untouched(&u, "C3, J1, J2").await;
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The job

/// The row `assignments` leave for a job, and what the POST and the GET say of it.
async fn judged(u: &Up, who: &Who, assignments: &str) -> (Reply, Reply) {
    let id = job(u).await;
    set(u, &id, assignments).await;
    let shown = scope(u, who, &[&id]).await;
    assert_eq!(shown.status, 200, "{assignments}: {}", shown.body);
    let posted = post(u, who, &body(&[&id], &digest_of(&shown))).await;
    (shown, posted)
}

#[tokio::test]
async fn a_job_the_agent_never_acknowledged_is_422_job_not_delivered_whoever_ended_it() {
    // J3, J4: one writer of each end that happens before a delivery was acknowledged.
    let u = Up::new().await;
    let alice = alice(&u).await;
    for (what, assignments) in [
        (
            "received, no ack",
            "state = 'received', delivered_at = NULL, finished_at = NULL",
        ),
        (
            "expired by the server",
            "state = 'expired', delivered_at = NULL, finished_at = now()",
        ),
        (
            "key retired",
            "state = 'failed', failure_code = 'instructionKeyRetired', delivered_at = NULL, finished_at = now()",
        ),
        (
            "internal",
            "state = 'failed', failure_code = 'internal', delivered_at = NULL, finished_at = now()",
        ),
        (
            "rejected",
            "state = 'failed', failure_code = 'instructionRejected', delivered_at = NULL, finished_at = now()",
        ),
    ] {
        let (shown, posted) = judged(&u, &alice, assignments).await;
        problem(&posted, 422, Some("JOB_NOT_DELIVERED"), what);
        let listed = &json(&shown)["jobs"][0];
        assert_eq!(listed["requestable"], false, "{what}");
    }
    // The job that was not ended shows no end.
    let (shown, _) = judged(
        &u,
        &alice,
        "state = 'received', delivered_at = NULL, finished_at = NULL",
    )
    .await;
    assert_eq!(json(&shown)["jobs"][0]["ended"], Value::Null);
    assert_untouched(&u, "J3, J4").await;
    u.finish().await;
}

#[tokio::test]
async fn a_delivered_job_that_has_not_ended_is_422_job_not_ended() {
    // J5, J8: every state of a running job, and a job reopened for on-site recovery.
    let u = Up::new().await;
    let alice = alice(&u).await;
    for state in RUNNING.iter().chain(&["needsOnSiteRecovery"]) {
        let (shown, posted) = judged(
            &u,
            &alice,
            &format!("state = '{state}', delivered_at = now(), finished_at = NULL"),
        )
        .await;
        problem(&posted, 422, Some("JOB_NOT_ENDED"), state);
        assert_eq!(json(&shown)["jobs"][0]["requestable"], false, "{state}");
    }
    // A stored log does not excuse the job: it is judged first.
    let stored = job(&u).await;
    set(&u, &stored, "state = 'running', finished_at = NULL").await;
    object(&u, &stored, 1, "commLog", "finished").await;
    let shown = scope(&u, &alice, &[&stored]).await;
    problem(
        &post(&u, &alice, &body(&[&stored], &digest_of(&shown))).await,
        422,
        Some("JOB_NOT_ENDED"),
        "stored log, not ended",
    );
    assert_untouched(&u, "J5, J8").await;
    u.finish().await;
}

#[tokio::test]
async fn a_delivered_and_ended_job_is_requested_in_each_end_state() {
    // J6, J7: the agent's six ends, `expired` being also the server's end of a delivered job that
    // the agent may still be running.
    let u = Up::new().await;
    let alice = alice(&u).await;
    for state in ENDED {
        let (shown, posted) = judged(
            &u,
            &alice,
            &format!("state = '{state}', delivered_at = now(), finished_at = now()"),
        )
        .await;
        assert_eq!(json(&shown)["jobs"][0]["requestable"], true, "{state}");
        assert_eq!(posted.status, 202, "{state}: {}", posted.body);
    }
    assert_eq!(count(&u, "comm_log_requests").await, ENDED.len() as i64);
    u.finish().await;
}

#[tokio::test]
async fn a_job_whose_agent_is_revoked_is_listed_not_requestable_and_a_post_is_403() {
    // J10, G10: the revocation comes between the two calls.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    // A stored log does not excuse a revoked agent.
    object(&u, &id, 1, "commLog", "finished").await;
    let shown = scope(&u, &alice, &[&id]).await;
    let ok = digest_of(&shown);
    exec(
        &u,
        &format!("UPDATE agents SET revoked_at = now() WHERE id = '{}'", u.id),
    )
    .await;
    let after = scope(&u, &alice, &[&id]).await;
    assert_eq!(after.status, 200, "{}", after.body);
    assert_eq!(json(&after)["jobs"][0]["requestable"], false);
    // The digest is the same: the agent's standing is not in it.
    assert_eq!(digest_of(&after), ok);
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        403,
        None,
        "revoked",
    );
    assert_untouched(&u, "J10").await;
    u.finish().await;
}

#[tokio::test]
async fn a_stored_instruction_that_does_not_decode_is_a_500_on_both_routes() {
    // J11
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    let shown = scope(&u, &alice, &[&id]).await;
    let ok = digest_of(&shown);
    set(&u, &id, "signed_job = '\\x01'::bytea").await;
    problem(&scope(&u, &alice, &[&id]).await, 500, None, "GET");
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        500,
        None,
        "POST",
    );
    assert_untouched(&u, "J11").await;
    u.finish().await;
}

#[tokio::test]
async fn several_jobs_that_fail_differently_are_answered_by_class_whatever_the_order_of_the_ids() {
    // J12: 404, then 500, then 403, then not delivered, then not ended, then 409.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let (revoked_agent, _) = second_agent(&u).await;
    let broken = job(&u).await;
    set(&u, &broken, "signed_job = '\\x01'::bytea").await;
    let revoked = job_of(&u, &revoked_agent).await;
    exec(
        &u,
        &format!("UPDATE agents SET revoked_at = now() WHERE id = '{revoked_agent}'"),
    )
    .await;
    let undelivered = job(&u).await;
    set(&u, &undelivered, "delivered_at = NULL").await;
    let running = job(&u).await;
    set(&u, &running, "state = 'running', finished_at = NULL").await;
    let unknown = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f".to_owned();
    let digest = "ab".repeat(32);
    let cases: Vec<(Vec<&str>, u16, Option<&str>)> = vec![
        (vec![&unknown, &broken, &revoked], 404, None),
        (vec![&broken, &revoked, &undelivered], 500, None),
        (vec![&revoked, &undelivered, &running], 403, None),
        (vec![&undelivered, &running], 422, Some("JOB_NOT_DELIVERED")),
        (vec![&running], 422, Some("JOB_NOT_ENDED")),
        (vec![&running, &undelivered], 422, Some("JOB_NOT_DELIVERED")),
    ];
    for (mut ids, status, code) in cases {
        for _ in 0..2 {
            problem(
                &post(&u, &alice, &body(&ids, &digest)).await,
                status,
                code,
                &format!("{ids:?}"),
            );
            ids.reverse();
        }
    }
    assert_untouched(&u, "J12").await;
    u.finish().await;
}

#[tokio::test]
async fn the_generation_is_the_stored_instructions_and_another_generations_rows_are_ignored() {
    // J13, J14
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = signed_job(&u.w, &u.id, 3, Vec::new(), None, None, true).await;
    // A refused request and a stored log at another generation say nothing of generation 3.
    Seed {
        generation: 2,
        ..Seed::new("refused")
    }
    .put(&u, &id)
    .await;
    object(&u, &id, 2, "commLog", "finished").await;
    let shown = scope(&u, &alice, &[&id]).await;
    let listed = json(&shown);
    let job = job_in(&listed, &id);
    assert_eq!(job["generation"], 3);
    assert_eq!(job["log"]["state"], "none");
    assert_eq!(job["request"], Value::Null);
    let posted = post(&u, &alice, &body(&[&id], &digest_of(&shown))).await;
    assert_eq!(posted.status, 202, "{}", posted.body);
    assert_eq!(json(&posted)["jobs"][0]["generation"], 3);
    let generations: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, state FROM comm_log_requests WHERE job_id = $1::uuid ORDER BY generation",
    )
    .bind(&id)
    .fetch_all(&u.w.pool)
    .await
    .unwrap();
    assert_eq!(
        generations,
        [(2, "refused".to_owned()), (3, "requested".to_owned())]
    );
    let detail: String = sqlx::query_scalar(
        "SELECT detail::text FROM audit_log WHERE action = 'commLogUploadRequested'",
    )
    .fetch_one(&u.w.pool)
    .await
    .unwrap();
    assert_eq!(detail, r#"{"generation": 3}"#);
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// What POST writes

/// The row and the audit rows of `job` as they are, for comparing before and after.
async fn state_of(u: &Up, job: &str) -> (Vec<String>, i64) {
    (whole(u, job).await, audits(u).await)
}

#[tokio::test]
async fn a_new_request_is_inserted_with_the_jobs_own_agent_and_one_audit_row_that_holds_no_vin() {
    // M1, A1, P4: each log state that does not store a log, no row before.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let mut ids = Vec::new();
    for log in [None, Some("failed"), Some("open"), Some("completing")] {
        let id = job(&u).await;
        set(&u, &id, &format!("vin = '{VIN}', kind = 'writeSettings'")).await;
        if let Some(log) = log {
            object(&u, &id, 1, "commLog", log).await;
        }
        ids.push(id);
    }
    ids.sort();
    let shown = scope(&u, &alice, &refs(&ids)).await;
    let ok = digest_of(&shown);
    let posted = post(&u, &alice, &body(&refs(&ids), &ok)).await;
    assert_eq!(posted.status, 202, "{}", posted.body);
    assert_eq!(posted.header("cache-control"), Some("no-store"));
    for id in &ids {
        assert_eq!(row(&u, id).await, "requested|-|false|false");
        let (by, agent, generation): (String, String, i64) = sqlx::query_as(
            "SELECT requested_by::text, agent_id::text, generation FROM comm_log_requests \
             WHERE job_id = $1::uuid",
        )
        .bind(id)
        .fetch_one(&u.w.pool)
        .await
        .unwrap();
        assert_eq!(
            (by.as_str(), agent.as_str(), generation),
            (alice.id.as_str(), u.id.as_str(), 1)
        );
    }
    assert_eq!(audits(&u).await, 4);
    // The audit row is the job's: tenant, the caller, the job's agent, job and VIN, with the
    // generation alone in its detail.
    let rows: Vec<(String, String, String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT operator_id::text, agent_id::text, job_id::text, vin, detail::text, \
             confirmation_level FROM audit_log WHERE action = 'commLogUploadRequested' \
         ORDER BY job_id",
    )
    .fetch_all(&u.w.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 4);
    for ((operator, agent, job, vin, detail, level), id) in rows.iter().zip(&ids) {
        assert_eq!(
            (operator.as_str(), agent.as_str(), job),
            (alice.id.as_str(), u.id.as_str(), id)
        );
        assert_eq!(vin.as_deref(), Some(VIN));
        assert_eq!(level, "notifyOnly");
        assert_eq!(detail, r#"{"generation": 1}"#);
        assert!(
            !detail.contains(&ok),
            "the digest is never in the audit row"
        );
    }
    u.finish().await;
}

#[tokio::test]
async fn a_request_that_is_requested_or_accepted_is_left_as_it_is_with_no_audit_row() {
    // M2, A3
    let u = Up::new().await;
    let alice = alice(&u).await;
    let bob = who(&u, "bob", &["worker"]).await;
    for state in ["requested", "accepted"] {
        for log in [None, Some("open"), Some("finished")] {
            let id = job(&u).await;
            if let Some(log) = log {
                object(&u, &id, 1, "commLog", log).await;
            }
            Seed {
                sent: true,
                age_secs: 90 * 60,
                by: Some(&bob.id),
                ..Seed::new(state)
            }
            .put(&u, &id)
            .await;
            let before = state_of(&u, &id).await;
            let r = request(&u, &alice, &[&id]).await;
            assert_eq!(r.status, 202, "{state} {log:?}: {}", r.body);
            assert_eq!(state_of(&u, &id).await, before, "{state} {log:?}");
            assert_eq!(json(&r)["jobs"][0]["request"]["state"], state);
        }
    }
    assert_eq!(audits(&u).await, 0);
    u.finish().await;
}

#[tokio::test]
async fn a_refused_or_expired_request_is_renewed_in_place_with_one_audit_row() {
    // M3, M4, A2: with the old request by another operator and two hours old.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let bob = who(&u, "bob", &["worker"]).await;
    let old: Vec<(&str, Seed)> = vec![
        (
            "unknownJob",
            Seed {
                refusal: Some("unknownJob"),
                sent: true,
                ..Seed::new("refused")
            },
        ),
        (
            "noLog",
            Seed {
                refusal: Some("noLog"),
                sent: true,
                ..Seed::new("refused")
            },
        ),
        (
            "busy",
            Seed {
                refusal: Some("busy"),
                sent: true,
                ..Seed::new("refused")
            },
        ),
        (
            "tooLarge",
            Seed {
                refusal: Some("tooLarge"),
                sent: true,
                ..Seed::new("refused")
            },
        ),
        (
            "expired, sent",
            Seed {
                sent: true,
                ..Seed::new("expired")
            },
        ),
        ("expired, never sent", Seed::new("expired")),
    ];
    for (what, seed) in old {
        for log in [None, Some("failed"), Some("open")] {
            let id = job(&u).await;
            if let Some(log) = log {
                object(&u, &id, 1, "commLog", log).await;
            }
            Seed {
                age_secs: 120 * 60,
                by: Some(&bob.id),
                ..seed.clone()
            }
            .put(&u, &id)
            .await;
            let audits_before = audits(&u).await;
            let r = request(&u, &alice, &[&id]).await;
            assert_eq!(r.status, 202, "{what} {log:?}: {}", r.body);
            assert_eq!(
                row(&u, &id).await,
                "requested|-|false|false",
                "{what} {log:?}"
            );
            let (by, fresh): (String, bool) = sqlx::query_as(
                "SELECT requested_by::text, requested_at > now() - interval '1 hour' \
                 FROM comm_log_requests WHERE job_id = $1::uuid",
            )
            .bind(&id)
            .fetch_one(&u.w.pool)
            .await
            .unwrap();
            assert_eq!(by, alice.id, "{what}: the new request's operator");
            assert!(fresh, "{what}: the new request's time");
            assert_eq!(audits(&u).await, audits_before + 1, "{what} {log:?}");
        }
    }
    u.finish().await;
}

#[tokio::test]
async fn a_job_whose_log_is_stored_is_left_alone_whatever_its_row_says() {
    // M5, M6, A3: nothing is inserted or renewed, and the scope shows the log beside the row.
    let u = Up::new().await;
    let alice = alice(&u).await;
    for old in [
        None,
        Some(Seed::new("refused")),
        Some(Seed::new("expired")),
        Some(Seed::new("requested")),
        Some(Seed::new("accepted")),
    ] {
        let id = job(&u).await;
        object(&u, &id, 1, "commLog", "finished").await;
        // An older failed attempt does not change that.
        object(&u, &id, 1, "commLog", "failed").await;
        if let Some(seed) = &old {
            seed.put(&u, &id).await;
        }
        let before = state_of(&u, &id).await;
        let shown = scope(&u, &alice, &[&id]).await;
        let listed = json(&shown);
        assert_eq!(job_in(&listed, &id)["log"]["state"], "finished");
        assert_eq!(
            job_in(&listed, &id)["request"]["state"].as_str(),
            old.as_ref().map(|s| s.state)
        );
        let r = post(&u, &alice, &body(&[&id], &digest_of(&shown))).await;
        assert_eq!(r.status, 202, "{}", r.body);
        assert_eq!(state_of(&u, &id).await, before);
    }
    assert_eq!(audits(&u).await, 0);
    u.finish().await;
}

#[tokio::test]
async fn a_request_nearly_a_day_old_is_left_and_keeps_its_first_time() {
    // M8: the expiry counts from the first request.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    Seed {
        age_secs: 24 * 3600 - 6,
        sent: true,
        ..Seed::new("requested")
    }
    .put(&u, &id)
    .await;
    let before = state_of(&u, &id).await;
    assert_eq!(request(&u, &alice, &[&id]).await.status, 202);
    assert_eq!(state_of(&u, &id).await, before);
    u.finish().await;
}

#[tokio::test]
async fn a_mixed_batch_writes_each_job_by_its_own_row_and_audits_what_it_wrote() {
    // M9, A2, A3: new, refused, expired, requested and stored.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 5).await;
    Seed::new("refused").put(&u, &ids[1]).await;
    Seed::new("expired").put(&u, &ids[2]).await;
    Seed::new("requested").put(&u, &ids[3]).await;
    object(&u, &ids[4], 1, "commLog", "finished").await;
    let r = request(&u, &alice, &refs(&ids)).await;
    assert_eq!(r.status, 202, "{}", r.body);
    let answered = json(&r);
    let listed: Vec<(&str, Option<&str>)> = answered["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| (j["jobId"].as_str().unwrap(), j["request"]["state"].as_str()))
        .collect();
    let expected: Vec<(&str, Option<&str>)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), (i != 4).then_some("requested")))
        .collect();
    assert_eq!(listed, expected, "in job-ID order");
    assert_eq!(audits(&u).await, 3);
    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT job_id::text FROM audit_log WHERE action = 'commLogUploadRequested' ORDER BY job_id",
    )
    .fetch_all(&u.w.pool)
    .await
    .unwrap();
    assert_eq!(audited, ids[..3]);
    assert_eq!(count(&u, "comm_log_requests").await, 4);
    u.finish().await;
}

#[tokio::test]
async fn thirty_two_jobs_are_requested_in_one_transaction_with_thirty_two_audit_rows() {
    // S2: 1 and 32 jobs on both routes.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 32).await;
    let one = scope(&u, &alice, &[&ids[0]]).await;
    assert_eq!(one.status, 200);
    let all = scope(&u, &alice, &refs(&ids)).await;
    assert_eq!(all.status, 200, "{}", all.body);
    assert_eq!(json(&all)["jobs"].as_array().unwrap().len(), 32);
    let r = post(&u, &alice, &body(&refs(&ids), &digest_of(&all))).await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(json(&r)["jobs"].as_array().unwrap().len(), 32);
    assert_eq!(count(&u, "comm_log_requests").await, 32);
    assert_eq!(audits(&u).await, 32);
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// What GET shows

#[tokio::test]
async fn the_scope_shows_the_log_from_the_jobs_objects_and_the_request_beside_it_and_writes_nothing()
 {
    // L1 to L5
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 7).await;
    // 0: no object. 1: failed attempts only. 2: open, after a failed one. 3: completing.
    // 4: finished. 5: only a result object, and a log at another generation. 6: refused row.
    object(&u, &ids[1], 1, "commLog", "failed").await;
    object(&u, &ids[1], 1, "commLog", "failed").await;
    object(&u, &ids[2], 1, "commLog", "failed").await;
    object(&u, &ids[2], 1, "commLog", "open").await;
    object(&u, &ids[3], 1, "commLog", "completing").await;
    object(&u, &ids[4], 1, "commLog", "finished").await;
    object(&u, &ids[5], 1, "result", "finished").await;
    object(&u, &ids[5], 2, "commLog", "finished").await;
    Seed {
        refusal: Some("busy"),
        ..Seed::new("refused")
    }
    .put(&u, &ids[6])
    .await;
    Seed::new("accepted").put(&u, &ids[3]).await;
    let r = scope(&u, &alice, &refs(&ids)).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let shown = json(&r);
    let logs: Vec<&str> = ids
        .iter()
        .map(|id| job_in(&shown, id)["log"]["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        logs,
        [
            "none",
            "failed",
            "open",
            "completing",
            "finished",
            "none",
            "none"
        ]
    );
    assert_eq!(job_in(&shown, &ids[0])["request"], Value::Null);
    assert_eq!(
        job_in(&shown, &ids[3])["request"],
        json!({ "state": "accepted" })
    );
    assert_eq!(
        job_in(&shown, &ids[6])["request"],
        json!({ "state": "refused", "refusal": "busy" })
    );
    // The jobs are listed in job-ID order whatever the order asked.
    let mut reversed = refs(&ids);
    reversed.reverse();
    assert_eq!(scope(&u, &alice, &reversed).await.body, r.body);
    assert_eq!(count(&u, "comm_log_requests").await, 2);
    assert_eq!(audits(&u).await, 0);
    u.finish().await;
}

#[tokio::test]
async fn the_scope_shows_each_job_and_the_period_in_utc_with_microseconds() {
    // L6
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 2).await;
    set(
        &u,
        &ids[0],
        &format!("vin = '{VIN}', created_at = '2026-01-02 03:04:05.678901+00', finished_at = '2026-01-02 04:00:00.5+00'"),
    )
    .await;
    set(
        &u,
        &ids[1],
        "created_at = '2026-01-01 23:00:00+01', finished_at = '2026-01-03 00:00:00+00', kind = 'writeSettings'",
    )
    .await;
    let r = scope(&u, &alice, &refs(&ids)).await;
    let shown = json(&r);
    let first = job_in(&shown, &ids[0]);
    assert_eq!(first["created"], "2026-01-02T03:04:05.678901Z");
    assert_eq!(first["ended"], "2026-01-02T04:00:00.500000Z");
    assert_eq!(first["vin"], VIN);
    assert_eq!(first["agentId"], u.id);
    assert_eq!(first["kind"], "acquire");
    assert_eq!(job_in(&shown, &ids[1])["vin"], Value::Null);
    assert_eq!(job_in(&shown, &ids[1])["kind"], "writeSettings");
    assert_eq!(
        shown["period"],
        json!({ "from": "2026-01-01T22:00:00.000000Z", "to": "2026-01-03T00:00:00.000000Z" })
    );
    // A job that has not ended leaves the period open.
    set(&u, &ids[1], "state = 'running', finished_at = NULL").await;
    let open = json(&scope(&u, &alice, &refs(&ids)).await);
    assert_eq!(
        open["period"],
        json!({ "from": "2026-01-01T22:00:00.000000Z", "to": null })
    );
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The digest

#[tokio::test]
async fn a_digest_is_this_scopes_whatever_the_order_and_another_scopes_is_a_409_that_names_nothing()
{
    // G1, G2, S8
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 3).await;
    let shown = scope(&u, &alice, &refs(&ids[..2])).await;
    let ok = digest_of(&shown);
    assert_eq!(ok.len(), 64);
    // The same digest from the GET in another order.
    let mut reversed = refs(&ids[..2]);
    reversed.reverse();
    assert_eq!(digest_of(&scope(&u, &alice, &reversed).await), ok);
    let other = job(&u).await;
    for (what, named) in [
        ("a subset", refs(&ids[..1])),
        ("a superset", refs(&ids)),
        ("other jobs", vec![ids[2].as_str(), other.as_str()]),
    ] {
        let r = post(&u, &alice, &body(&named, &ok)).await;
        problem(&r, 409, Some("SCOPE_CHANGED"), what);
        let answered = json(&r);
        let mut keys: Vec<&str> = answered
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["code", "status", "title", "type"], "{what}");
        assert!(!r.body.contains(&ok) && !r.body.contains(&other), "{what}");
    }
    problem(
        &post(&u, &alice, &body(&refs(&ids[..2]), &"0".repeat(64))).await,
        409,
        Some("SCOPE_CHANGED"),
        "a well-formed digest that is not the scope's",
    );
    assert_untouched(&u, "G2").await;
    // The scope that was shown, named in the other order, is requested.
    let r = post(&u, &alice, &body(&reversed, &ok)).await;
    assert_eq!(r.status, 202, "{}", r.body);
    u.finish().await;
}

#[tokio::test]
async fn a_job_that_ended_or_changed_since_it_was_shown_is_a_409_and_one_that_did_not_end_is_a_422()
{
    // G3, G4, G5
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    set(&u, &id, "state = 'running', finished_at = NULL").await;
    let shown = scope(&u, &alice, &[&id]).await;
    let ok = digest_of(&shown);
    // G4: still not ended: the job's own refusal, not the digest's.
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        422,
        Some("JOB_NOT_ENDED"),
        "G4",
    );
    // G3: ended since.
    set(&u, &id, "state = 'completed', finished_at = now()").await;
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        409,
        Some("SCOPE_CHANGED"),
        "G3",
    );
    // G5: reopened and ended again.
    let ended = digest_of(&scope(&u, &alice, &[&id]).await);
    set(&u, &id, "state = 'needsOnSiteRecovery', finished_at = NULL").await;
    problem(
        &post(&u, &alice, &body(&[&id], &ended)).await,
        422,
        Some("JOB_NOT_ENDED"),
        "reopened",
    );
    set(
        &u,
        &id,
        "state = 'completed', finished_at = now() + interval '1 second'",
    )
    .await;
    problem(
        &post(&u, &alice, &body(&[&id], &ended)).await,
        409,
        Some("SCOPE_CHANGED"),
        "G5",
    );
    assert_untouched(&u, "G3, G4, G5").await;
    // The VIN is part of the scope too.
    let again = digest_of(&scope(&u, &alice, &[&id]).await);
    set(&u, &id, &format!("vin = '{VIN_2}'")).await;
    problem(
        &post(&u, &alice, &body(&[&id], &again)).await,
        409,
        Some("SCOPE_CHANGED"),
        "VIN",
    );
    u.finish().await;
}

#[tokio::test]
async fn the_state_of_the_log_and_of_the_request_is_not_in_the_digest() {
    // G6, G7, G8
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 3).await;
    let shown = scope(&u, &alice, &refs(&ids)).await;
    let ok = digest_of(&shown);
    // The log goes from none to open, and a refused request is by now answered otherwise.
    object(&u, &ids[0], 1, "commLog", "open").await;
    Seed::new("refused").put(&u, &ids[1]).await;
    let r = post(&u, &alice, &body(&refs(&ids), &ok)).await;
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(
        row(&u, &ids[0]).await,
        "requested|-|false|false",
        "inserted whatever the log"
    );
    assert_eq!(
        row(&u, &ids[1]).await,
        "requested|-|false|false",
        "renewed by the row's state now"
    );
    assert_eq!(row(&u, &ids[2]).await, "requested|-|false|false");
    assert_eq!(audits(&u).await, 3);
    // The same request again, after the log of the first job was stored and the second's request
    // was answered: the 202 of a lost answer is repeated and nothing is written.
    exec(
        &u,
        "UPDATE job_objects SET state = 'finished', finished_at = now()",
    )
    .await;
    exec(
        &u,
        "UPDATE comm_log_requests SET state = 'accepted', answered_at = now()",
    )
    .await;
    let before = (
        whole(&u, &ids[0]).await,
        whole(&u, &ids[1]).await,
        audits(&u).await,
    );
    let again = post(&u, &alice, &body(&refs(&ids), &ok)).await;
    assert_eq!(again.status, 202, "{}", again.body);
    assert_eq!(
        (
            whole(&u, &ids[0]).await,
            whole(&u, &ids[1]).await,
            audits(&u).await
        ),
        before
    );
    // A failed attempt that a later open one follows is no change either.
    let other = job(&u).await;
    object(&u, &other, 1, "commLog", "failed").await;
    let shown = scope(&u, &alice, &[&other]).await;
    exec(
        &u,
        &format!("UPDATE job_objects SET state = 'open' WHERE job_id = '{other}'"),
    )
    .await;
    assert_eq!(
        post(&u, &alice, &body(&[&other], &digest_of(&shown)))
            .await
            .status,
        202
    );
    u.finish().await;
}

#[tokio::test]
async fn a_role_lost_between_the_two_calls_is_a_403_not_a_409() {
    // G10
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    let ok = digest_of(&scope(&u, &alice, &[&id]).await);
    exec(&u, "DELETE FROM operator_roles WHERE role = 'worker'").await;
    problem(
        &post(&u, &alice, &body(&[&id], &ok)).await,
        403,
        None,
        "G10",
    );
    assert_untouched(&u, "G10").await;
    u.finish().await;
}

#[tokio::test]
async fn a_body_of_exactly_the_bound_is_read_and_judged() {
    // S7: 4096 bytes pass the size rule; the request is then judged as any other (here a job
    // that is not the caller's).
    let u = Up::new().await;
    let alice = alice(&u).await;
    let text = body(&["0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f"], &"ab".repeat(32));
    let padded = format!("{text}{}", " ".repeat(4096 - text.len()));
    assert_eq!(padded.len(), 4096);
    problem(&post(&u, &alice, &padded).await, 404, None, "4096 bytes");
    u.finish().await;
}

#[tokio::test]
async fn an_operator_off_the_agents_allow_list_gets_the_403_of_a_revoked_agent() {
    // The allow-list rule of ADR-508 item 2: judged in the class of the revoked agent, with one
    // document for every reason.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let viewer = who(&u, "viewer", &["viewer"]).await;
    let listed = job(&u).await;
    let stored = job(&u).await;
    object(&u, &stored, 1, "commLog", "finished").await;
    let ok = digest_of(&scope(&u, &alice, &[&listed]).await);
    let missing_role = post(&u, &viewer, &body(&[&listed], &ok)).await;
    problem(&missing_role, 403, None, "missing role");
    exec(
        &u,
        &format!(
            "DELETE FROM agent_allowed_operators WHERE agent_id = '{}' AND operator_id = '{}'",
            u.id, alice.id
        ),
    )
    .await;
    let after = scope(&u, &alice, &[&listed]).await;
    assert_eq!(after.status, 200, "{}", after.body);
    assert_eq!(json(&after)["jobs"][0]["requestable"], false);
    let off_list = post(&u, &alice, &body(&[&listed], &ok)).await;
    problem(&off_list, 403, None, "off the allow-list");
    // A stored log does not make the job requestable.
    let with_log = post(&u, &alice, &body(&[&stored], &ok)).await;
    problem(&with_log, 403, None, "stored log, off the allow-list");
    // The same document as the missing role's and the revoked agent's.
    exec(
        &u,
        &format!(
            "INSERT INTO agent_allowed_operators (tenant_id, agent_id, operator_id) \
             SELECT tenant_id, '{}', '{}' FROM agents WHERE id = '{}'; \
             UPDATE agents SET revoked_at = now() WHERE id = '{}'",
            u.id, alice.id, u.id, u.id
        ),
    )
    .await;
    let revoked = post(&u, &alice, &body(&[&listed], &ok)).await;
    problem(&revoked, 403, None, "revoked");
    assert_eq!(normalized(&off_list), normalized(&missing_role));
    assert_eq!(normalized(&revoked), normalized(&missing_role));
    assert_untouched(&u, "allow-list").await;
    u.finish().await;
}

#[tokio::test]
async fn a_log_stored_between_the_scope_read_and_the_write_is_left_alone() {
    // The statements check the stored log themselves: the request holds the agent's row
    // `FOR SHARE` after the scope read, so a held update of that row stops it between the two.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let (new, refused) = {
        let ids = jobs(&u, 2).await;
        (ids[0].clone(), ids[1].clone())
    };
    Seed::new("refused").put(&u, &refused).await;
    let before = state_of(&u, &refused).await;
    let text = body(
        &[&new, &refused].map(String::as_str),
        &digest_of(&scope(&u, &alice, &[&new, &refused]).await),
    );
    let mut hold = u.w.pg.pool().begin().await.unwrap();
    sqlx::query("UPDATE agents SET last_seen_at = now() WHERE id = $1::uuid")
        .bind(&u.id)
        .execute(&mut *hold)
        .await
        .unwrap();
    let waiting = spawn_post(&u, &alice, &text);
    wait_for_lock(&u, "SELECT 1 FROM agents").await;
    object(&u, &new, 1, "commLog", "finished").await;
    object(&u, &refused, 1, "commLog", "finished").await;
    hold.commit().await.unwrap();
    let r = waiting.await.unwrap();
    assert_eq!(r.status, 202, "{}", r.body);
    assert_eq!(row(&u, &new).await, "", "no row inserted");
    assert_eq!(state_of(&u, &refused).await, before, "no row renewed");
    assert_eq!(audits(&u).await, 0);
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Concurrency

/// Holds `audit_log` until the returned transaction ends, so that a request waits at its audit
/// row, after the request row it wrote.
async fn hold_audit_log(u: &Up) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut lock = u.w.pg.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE audit_log IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    lock
}

#[tokio::test]
async fn two_requests_at_once_for_a_job_with_no_row_make_one_row_and_one_audit_row() {
    // X1, A5: the first waits at its audit row holding its uncommitted request row; the second
    // waits at the key.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    let text = body(&[&id], &digest_of(&scope(&u, &alice, &[&id]).await));
    let lock = hold_audit_log(&u).await;
    let first = spawn_post(&u, &alice, &text);
    wait_for_lock(&u, "INSERT INTO audit_log").await;
    let second = spawn_post(&u, &alice, &text);
    wait_for_lock(&u, "INSERT INTO comm_log_requests").await;
    lock.rollback().await.unwrap();
    for r in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(r.status, 202, "{}", r.body);
        assert_eq!(json(&r)["jobs"][0]["request"]["state"], "requested");
    }
    assert_eq!(count(&u, "comm_log_requests").await, 1);
    assert_eq!(audits(&u).await, 1);
    u.finish().await;
}

#[tokio::test]
async fn two_requests_at_once_for_a_refused_row_renew_it_once_with_one_audit_row() {
    // X2, A5
    let u = Up::new().await;
    let alice = alice(&u).await;
    let id = job(&u).await;
    Seed::new("refused").put(&u, &id).await;
    let text = body(&[&id], &digest_of(&scope(&u, &alice, &[&id]).await));
    let lock = hold_audit_log(&u).await;
    let first = spawn_post(&u, &alice, &text);
    wait_for_lock(&u, "INSERT INTO audit_log").await;
    let second = spawn_post(&u, &alice, &text);
    wait_for_lock(&u, "INSERT INTO comm_log_requests").await;
    lock.rollback().await.unwrap();
    for r in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(r.status, 202, "{}", r.body);
    }
    assert_eq!(row(&u, &id).await, "requested|-|false|false");
    assert_eq!(audits(&u).await, 1);
    u.finish().await;
}

#[tokio::test]
async fn two_requests_for_overlapping_jobs_in_opposite_order_write_in_job_id_order_and_do_not_deadlock()
 {
    // X3: the first holds the lower job's row at its audit row. A second request that wrote the
    // higher job first would take that row and then wait for the first's, which waits for its
    // own: it would wait for the audit log, not for a request row.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 2).await;
    let ok = digest_of(&scope(&u, &alice, &refs(&ids)).await);
    let lock = hold_audit_log(&u).await;
    let first = spawn_post(&u, &alice, &body(&[&ids[0], &ids[1]], &ok));
    wait_for_lock(&u, "INSERT INTO audit_log").await;
    let second = spawn_post(&u, &alice, &body(&[&ids[1], &ids[0]], &ok));
    wait_for_lock(&u, "INSERT INTO comm_log_requests").await;
    lock.rollback().await.unwrap();
    for r in [first.await.unwrap(), second.await.unwrap()] {
        assert_eq!(r.status, 202, "{}", r.body);
    }
    assert_eq!(count(&u, "comm_log_requests").await, 2);
    assert_eq!(audits(&u).await, 2);
    u.finish().await;
}

#[tokio::test]
async fn an_audit_log_held_for_five_seconds_is_a_503_that_writes_no_request_and_a_retry_succeeds() {
    // X15, A4, M10, G8: the request row of the first job is written before its audit row waits,
    // so the rollback is what leaves no row.
    let u = Up::new().await;
    let alice = alice(&u).await;
    let ids = jobs(&u, 2).await;
    let text = body(
        &refs(&ids),
        &digest_of(&scope(&u, &alice, &refs(&ids)).await),
    );
    let lock = hold_audit_log(&u).await;
    let started = Instant::now();
    let r = post(&u, &alice, &text).await;
    problem(&r, 503, None, "X15");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
    assert_untouched(&u, "X15").await;
    let retried = post(&u, &alice, &text).await;
    assert_eq!(retried.status, 202, "{}", retried.body);
    assert_eq!(count(&u, "comm_log_requests").await, 2);
    assert_eq!(audits(&u).await, 2);
    u.finish().await;
}

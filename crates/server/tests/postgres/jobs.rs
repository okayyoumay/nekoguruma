//! `POST /api/v1/jobs` over real TCP against a throwaway PostgreSQL (design 4.4, 5.1, 12,
//! ADR-477). The rules that need no database are unit tests in `src/jobs.rs`.
//!
//! The case names in the comments (L for the session layer, R for the request, C for creation,
//! K for the idempotency key, A for permission, S for the server's own failures, E for the way to
//! the agent) are the rows of ADR-477's table. The tests run on the current-thread runtime and
//! wait in real time; a negative (a refusal that must leave no row) is asserted after the answer
//! that came first. Every answer but a 202 is followed by a check that the count of `jobs` rows
//! is what it was.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use server::config::Http;
use server::jobs::body_hash;
use server::local_guard::pinned_origin;
use server::local_user::{self, Hasher, NewUser};
use server::{InstructionKey, Server};
use sha2::Digest;
use shared_crypto::instruction::{Envelope, verify};
use shared_crypto::{SigningKey, random_32};
use shared_proto::{INSTRUCTION_VERSION, JobInstruction, JobKind};
use sqlx::Postgres;

use crate::agent_events::say;
use crate::control::{open, quiet_for};
use crate::job_queue::{DELIVERED, LONG, SENT, ack, command, reject, row, settings, until_row};
use crate::redeem::{Agent, Flaky, World, capture, instruction_key, json};
use crate::serving::{Reply, Running, exchange};
use crate::session::{audience_of, get_challenge, proof, send_session, session_body};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
/// A digest no tenant of these tests holds unless a test gives it one.
const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const VIN: &str = "1HGCM82633A004352";
const VIN_2: &str = "WVWZZZ1JZXW000001";
const CLEARED: &str = "ngr_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict";

/// The password of every user of these tests, made at run time.
pub(crate) fn password() -> &'static str {
    static PASSWORD: LazyLock<String> =
        LazyLock::new(|| URL_SAFE_NO_PAD.encode(random_32().expect("random bytes")));
    &PASSWORD
}

// ---------------------------------------------------------------------------------------------
// Requests

fn origin(addr: SocketAddr) -> String {
    pinned_origin(addr.port())
}

fn raw(addr: SocketAddr, method: &str, path: &str, headers: &[String], body: &str) -> String {
    let mut raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for header in headers {
        raw.push_str(header);
        raw.push_str("\r\n");
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw
}

/// The headers of a job request that is complete: the pinned Origin, JSON, the key, the session.
fn job_headers(addr: SocketAddr, cookie: &str, key: &str) -> Vec<String> {
    vec![
        format!("Origin: {}", origin(addr)),
        "Content-Type: application/json".to_owned(),
        format!("Idempotency-Key: {key}"),
        format!("Cookie: {cookie}"),
    ]
}

fn raw_job(addr: SocketAddr, cookie: &str, key: &str, body: &str) -> String {
    raw(
        addr,
        "POST",
        "/api/v1/jobs",
        &job_headers(addr, cookie, key),
        body,
    )
}

pub(crate) async fn post_to(addr: SocketAddr, cookie: &str, key: &str, body: &str) -> Reply {
    exchange(addr, &raw_job(addr, cookie, key, body)).await
}

pub(crate) async fn login(addr: SocketAddr, name: &str) -> String {
    let body = json!({ "username": name, "password": password() }).to_string();
    let headers = [
        format!("Origin: {}", origin(addr)),
        "Content-Type: application/json".to_owned(),
    ];
    let r = exchange(addr, &raw(addr, "POST", "/api/v1/login", &headers, &body)).await;
    assert_eq!(r.status, 200, "{}", r.head);
    r.header("set-cookie")
        .expect("Set-Cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn assert_problem(r: &Reply, status: u16, what: &str) {
    assert_eq!(r.status, status, "{what}: {} {}", r.head, r.body);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
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

fn job_id(r: &Reply) -> String {
    assert_eq!(r.status, 202, "{} {}", r.head, r.body);
    json(r)["jobId"].as_str().unwrap().to_owned()
}

// ---------------------------------------------------------------------------------------------
// The world

/// A migrated database with the local users `alice` (role worker, the agent's owner) and a
/// registered agent, and a server on it. The agent's tenant holds the IR documents `DIGEST_A` and
/// `DIGEST_B` and the artifact `DIGEST_B` (ADR-501), given their rows by SQL: those digests name
/// no real document, and the route that stores one (ADR-500) writes an audit row these tests
/// would count. The server has the upload tests' fake store, so a test can store a real document
/// through that route.
struct Lab {
    w: World,
    agent: Agent,
    agent_id: String,
    s: Running,
    hash: String,
    more: Vec<Running>,
    /// The first server's login sessions, for a session the login route cannot make.
    operator_sessions: server::OperatorSessions,
}

/// A login session of `operator` in the session table, which the login route cannot make for a
/// tenant other than the local one; the session's cookie. `hash` is the operator's stored
/// password hash.
pub(crate) fn session_cookie(
    sessions: &server::OperatorSessions,
    tenant: &str,
    operator: &str,
    hash: &str,
) -> String {
    let token = random_32().unwrap();
    sessions.establish(
        &[],
        sha2::Sha256::digest(token).into(),
        server::operator_session::Holder {
            operator: operator.to_owned(),
            tenant: tenant.to_owned(),
            fingerprint: server::operator_session::fingerprint(hash),
        },
        Instant::now(),
        std::time::SystemTime::now(),
    );
    format!(
        "{}={}",
        server::operator_session::COOKIE_NAME,
        URL_SAFE_NO_PAD.encode(token)
    )
}

pub(crate) async fn add_user(
    pool: &sqlx::PgPool,
    name: &str,
    roles: &[&str],
    hash: &str,
) -> String {
    let roles: Vec<String> = roles.iter().map(|r| (*r).to_owned()).collect();
    let user = NewUser::new(name, name, &roles).unwrap();
    local_user::add(pool, &user, hash)
        .await
        .unwrap()
        .operator_id
}

/// The upload tests' store with no real store behind it: a document stored through the route is
/// read back as matching and keeps no bytes.
fn fake_store() -> server::uploads::Backend {
    let store: Arc<dyn server::MultipartStore> = Arc::new(crate::uploads::Wrap::default());
    server::uploads::Backend::new(Some(store))
}

/// Gives the agent's tenant the rows that make the IR documents `DIGEST_A` and `DIGEST_B` and the
/// artifact `DIGEST_B` stored.
pub(crate) async fn hold_named(pool: &sqlx::PgPool, agent_id: &str) {
    let tenant: String =
        sqlx::query_scalar("SELECT tenant_id::text FROM agents WHERE id = $1::uuid")
            .bind(agent_id)
            .fetch_one(pool)
            .await
            .unwrap();
    hold(pool, &tenant, &[DIGEST_A, DIGEST_B], &[DIGEST_B]).await;
}

/// Gives `tenant` a row for each of `documents` in `ir_documents` and each of `artifacts` in
/// `artifacts`, each with a storage key of its own, so that a job may name them (ADR-501).
async fn hold(pool: &sqlx::PgPool, tenant: &str, documents: &[&str], artifacts: &[&str]) {
    sqlx::query(
        "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
             variant_name, source_format, storage_key) \
         SELECT $1::uuid, d, 'ext', 1, 'variant', 'odx', gen_random_uuid()::text \
         FROM unnest($2::text[]) AS d",
    )
    .bind(tenant)
    .bind(documents)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artifacts (tenant_id, digest, size_bytes, storage_key) \
         SELECT $1::uuid, d, 1, gen_random_uuid()::text FROM unnest($2::text[]) AS d",
    )
    .bind(tenant)
    .bind(artifacts)
    .execute(pool)
    .await
    .unwrap();
}

fn http() -> Http {
    Http {
        port: 0,
        shutdown_grace_secs: 2,
    }
}

impl Lab {
    async fn new() -> Lab {
        Lab::build(instruction_key(), None).await
    }

    async fn build(key: Arc<InstructionKey>, queue: Option<server::job_queue::Settings>) -> Lab {
        let w = World::with_key(key).await;
        let hash = Hasher::new().hash(password().to_owned()).await.unwrap();
        add_user(&w.pool, "alice", &["worker"], &hash).await;
        let agent = Agent::new();
        let agent_id = w.register(&agent).await;
        hold_named(&w.pool, &agent_id).await;
        let server = Server::bind_with_store(&http(), w.pg.pool(), w.key.clone(), fake_store())
            .await
            .unwrap();
        let server = match queue {
            Some(settings) => server.with_job_queue(settings),
            None => server,
        };
        let operator_sessions = server.operator_sessions();
        Lab {
            operator_sessions,
            s: Running::spawn(server),
            w,
            agent,
            agent_id,
            hash,
            more: Vec::new(),
        }
    }

    /// Another server on the same database, with its own pool and session table.
    async fn another_server(&mut self) -> SocketAddr {
        let server =
            Server::bind_with_store(&http(), self.w.pg.pool(), self.w.key.clone(), fake_store())
                .await
                .unwrap();
        let running = Running::spawn(server);
        let addr = running.addr;
        self.more.push(running);
        addr
    }

    fn addr(&self) -> SocketAddr {
        self.s.addr
    }

    /// A worker of a second tenant with an agent that lists the worker, and a session of the
    /// worker; the session's cookie, the agent's ID and the tenant's ID. The tenant holds the
    /// same named objects as the first. The login route serves
    /// the local tenant only, so the session is made in the table, which a login would fill the
    /// same way.
    async fn second_tenant(&self) -> (String, String, String) {
        let (tenant, operator) = self.w.add_operator("bob").await;
        self.exec(&format!(
            "INSERT INTO operator_roles (tenant_id, operator_id, role) \
             VALUES ('{tenant}', '{operator}', 'worker'); \
             INSERT INTO operator_credentials (operator_id, password_hash) \
             VALUES ('{operator}', '{}')",
            self.hash
        ))
        .await;
        let agent = Agent::new();
        let (_, r) = self.w.redeem("bob", &agent).await;
        assert_eq!(r.status, 201, "{}", r.head);
        let agent_id = json(&r)["agentId"].as_str().unwrap().to_owned();
        hold_named(&self.w.pool, &agent_id).await;
        let cookie = session_cookie(&self.operator_sessions, &tenant, &operator, &self.hash);
        (cookie, agent_id, tenant)
    }

    /// A user with these roles; the operator ID.
    async fn user(&self, name: &str, roles: &[&str]) -> String {
        add_user(&self.w.pool, name, roles, &self.hash).await
    }

    async fn login(&self, name: &str) -> String {
        login(self.addr(), name).await
    }

    /// The request body of an acquire job for the agent, with `extra` merged over it; a null
    /// value removes the property.
    fn body(&self, extra: Value) -> String {
        let mut body = json!({
            "kind": "acquire",
            "agentId": self.agent_id,
            "vciId": "vci-1",
            "irDigests": [DIGEST_A],
        });
        for (name, value) in extra.as_object().unwrap() {
            if value.is_null() {
                body.as_object_mut().unwrap().remove(name);
            } else {
                body[name] = value.clone();
            }
        }
        body.to_string()
    }

    async fn post(&self, cookie: &str, key: &str, body: &str) -> Reply {
        post_to(self.addr(), cookie, key, body).await
    }

    /// Runs a statement that changes rows, as the superuser.
    async fn exec(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&self.w.pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    async fn jobs(&self) -> String {
        self.w.sql("SELECT count(*) FROM jobs").await
    }

    /// The job rows are `expected` many.
    async fn assert_jobs(&self, expected: u32, what: &str) {
        assert_eq!(self.jobs().await, expected.to_string(), "{what}");
    }

    /// The `jobCreated` rows of `audit_log` are `expected` many (ADR-484).
    async fn assert_audits(&self, expected: u32, what: &str) {
        assert_eq!(
            self.w
                .sql("SELECT count(*) FROM audit_log WHERE action = 'jobCreated'")
                .await,
            expected.to_string(),
            "{what}"
        );
    }

    /// Every job has exactly one `jobCreated` row and there is no other row in `audit_log`.
    async fn assert_one_audit_per_job(&self, what: &str) {
        assert_eq!(
            self.w
                .sql(
                    "SELECT count(*) FROM jobs j WHERE (SELECT count(*) FROM audit_log a \
                     WHERE a.job_id = j.id AND a.action = 'jobCreated') <> 1"
                )
                .await,
            "0",
            "{what}"
        );
        assert_eq!(
            self.w.sql("SELECT count(*) FROM audit_log").await,
            self.jobs().await,
            "{what}"
        );
    }

    async fn field(&self, job: &str, column: &str) -> String {
        self.w
            .sql(&format!("SELECT {column} FROM jobs WHERE id = '{job}'"))
            .await
    }

    /// The instruction of the stored job, after its envelope verifies under the server's key.
    async fn instruction(&self, job: &str) -> JobInstruction {
        let blob: Vec<u8> = sqlx::query_scalar("SELECT signed_job FROM jobs WHERE id = $1::uuid")
            .bind(job)
            .fetch_one(&self.w.pool)
            .await
            .unwrap();
        let envelope = Envelope::split(&blob).unwrap();
        assert_eq!(
            envelope.fingerprint(),
            &self.w.key.public_key().fingerprint()
        );
        verify(
            &self.w.key.public_key(),
            envelope.payload(),
            &envelope.signature().unwrap(),
        )
        .unwrap();
        JobInstruction::decode(envelope.payload()).unwrap()
    }

    /// The agent's session token, by the proof exchange of design 6.4.
    async fn agent_token(&self) -> String {
        let challenge = get_challenge(&self.s).await;
        let nonce = random_32().unwrap();
        let signed = proof(
            &self.agent.key,
            &audience_of(self.addr().port()),
            &self.agent_id,
            challenge,
            nonce,
        );
        let r = send_session(
            &self.s,
            &session_body(&self.agent_id, &challenge, &nonce, &signed),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.head);
        json(&r)["token"].as_str().unwrap().to_owned()
    }

    async fn finish(self) {
        for running in self.more {
            running.finish().await;
        }
        self.s.finish().await;
        self.w.finish().await;
    }
}

/// An uncommitted insert of a job row that holds `key` for alice, as a stand-in for another
/// request that is part way through; the transaction and the row's ID.
async fn hold_key(
    pool: &sqlx::PgPool,
    agent_id: &str,
    key: &str,
    hash: Option<[u8; 32]>,
) -> (sqlx::Transaction<'static, Postgres>, String) {
    let mut tx = pool.begin().await.unwrap();
    let id: String = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, id, kind, agent_id, operator_id, vci_id, \
             confirmation_level, start_deadline, signed_job, signing_key_id, idempotency_key, \
             request_hash) \
         SELECT a.tenant_id, gen_random_uuid(), 'acquire', a.id, o.id, 'vci-1', 'notifyOnly', \
             now() + interval '15 minutes', '\\x01'::bytea, 'k', $2, $3 \
         FROM agents a JOIN operators o ON o.tenant_id = a.tenant_id AND o.subject = 'alice' \
         WHERE a.id = $1::uuid RETURNING id::text",
    )
    .bind(agent_id)
    .bind(key)
    .bind(hash.map(|h| h.to_vec()))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    (tx, id)
}

/// Waits until a statement whose text starts with `prefix` waits for a lock, as the database
/// reports it.
async fn wait_for_lock(lab: &Lab, prefix: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while lab
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

/// Waits until a statement that inserts a job waits for a lock, as the database reports it.
async fn wait_for_insert_lock(lab: &Lab) {
    wait_for_lock(lab, "INSERT INTO jobs").await;
}

fn spawn_post(
    addr: SocketAddr,
    cookie: &str,
    key: &str,
    body: &str,
) -> tokio::task::JoinHandle<Reply> {
    let raw = raw_job(addr, cookie, key, body);
    tokio::spawn(async move { exchange(addr, &raw).await })
}

// ---------------------------------------------------------------------------------------------
// The session layer: L1 to L5

#[tokio::test]
async fn the_layer_answers_before_the_body_and_a_wrong_method_needs_no_session() {
    let lab = Lab::new().await;
    let addr = lab.addr();
    let body = lab.body(json!({}));
    let cookie = lab.login("alice").await;
    let post = |headers: Vec<String>, body: &str| {
        let raw = raw(addr, "POST", "/api/v1/jobs", &headers, body);
        async move { exchange(addr, &raw).await }
    };
    let json_type = "Content-Type: application/json".to_owned();
    let with_origin = format!("Origin: {}", origin(addr));
    let key = "Idempotency-Key: k".to_owned();

    // L1: a foreign Origin is the guard's 403, a missing one the layer's, before the cookie.
    let foreign = post(
        vec![
            "Origin: http://evil.example".to_owned(),
            json_type.clone(),
            key.clone(),
            format!("Cookie: {cookie}"),
        ],
        &body,
    )
    .await;
    assert_eq!(foreign.status, 403, "{}", foreign.head);
    let no_origin = post(
        vec![json_type.clone(), key.clone(), format!("Cookie: {cookie}")],
        &body,
    )
    .await;
    assert_problem(&no_origin, 403, "no Origin, a live session");
    assert!(no_origin.body.contains("Origin required"));
    let no_origin_no_cookie = post(vec![json_type.clone(), key.clone()], &body).await;
    assert_eq!(normalized(&no_origin_no_cookie), normalized(&no_origin));

    // L2: no cookie, a malformed token, a token nobody holds.
    let none = post(
        vec![with_origin.clone(), json_type.clone(), key.clone()],
        &body,
    )
    .await;
    assert_problem(&none, 401, "no cookie");
    assert_eq!(none.header("www-authenticate"), Some("Ngr-Session"));
    let unknown_token = URL_SAFE_NO_PAD.encode(random_32().unwrap());
    for (what, value) in [
        ("malformed", "ngr_session=%%%".to_owned()),
        ("unknown", format!("ngr_session={unknown_token}")),
    ] {
        let r = post(
            vec![
                with_origin.clone(),
                json_type.clone(),
                key.clone(),
                format!("Cookie: {value}"),
            ],
            &body,
        )
        .await;
        assert_problem(&r, 401, what);
        assert_eq!(r.header("set-cookie"), Some(CLEARED), "{what}");
    }

    // L3: a disabled operator and a replaced password end the session.
    lab.user("bob", &["worker"]).await;
    lab.user("carol", &["worker"]).await;
    let (bob, carol) = (lab.login("bob").await, lab.login("carol").await);
    lab.exec("UPDATE operators SET disabled_at = now() WHERE subject = 'bob'")
        .await;
    let replaced = Hasher::new()
        .hash(URL_SAFE_NO_PAD.encode(random_32().unwrap()))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE operator_credentials SET password_hash = $1 \
         WHERE operator_id = (SELECT id FROM operators WHERE subject = 'carol')",
    )
    .bind(replaced)
    .execute(&lab.w.pool)
    .await
    .unwrap();
    for (what, cookie) in [("disabled", &bob), ("replaced", &carol)] {
        let r = lab.post(cookie, "k", &body).await;
        assert_problem(&r, 401, what);
        assert_eq!(r.header("set-cookie"), Some(CLEARED), "{what}");
    }

    // L4: a wrong method is a 405 that names POST, with no session.
    for method in ["GET", "PUT"] {
        let r = exchange(addr, &raw(addr, method, "/api/v1/jobs", &[], "")).await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
        assert_eq!(r.header("allow"), Some("POST"), "{method}");
    }

    // L5: a body over the limit without a session is a 401: the layer answers before the body, so
    // only the headers are sent (unread input at close would reset the connection on Windows).
    let mut headers = vec![with_origin.clone(), json_type.clone(), key.clone()];
    headers.push("Content-Length: 20000".to_owned());
    let big_raw = format!(
        "POST /api/v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{}\r\n\r\n",
        addr.port(),
        headers.join("\r\n")
    );
    let big = exchange(addr, &big_raw).await;
    assert_problem(&big, 401, "oversized, no session");

    // Nothing above made a job, and the session that was not touched still creates one.
    lab.assert_jobs(0, "refusals").await;
    assert_eq!(lab.post(&cookie, "k", &body).await.status, 202);

    // L2, expired: a session past its absolute lifetime is a 401 with the cookie cleared.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(8 * 60 * 60 + 1)).await;
    tokio::time::resume();
    let expired = lab.post(&cookie, "k2", &body).await;
    assert_problem(&expired, 401, "expired");
    assert_eq!(expired.header("set-cookie"), Some(CLEARED));
    lab.assert_jobs(1, "expired").await;
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The body: R1

#[tokio::test]
async fn the_body_is_bounded_in_size_and_time_and_each_refusal_closes_the_connection() {
    let lab = Lab::new().await;
    let addr = lab.addr();
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let closes = |r: &Reply, what: &str| {
        assert_problem(r, r.status, what);
        assert_eq!(r.header("connection"), Some("close"), "{what}");
    };

    // Exactly 16384 bytes pass.
    let padded = format!("{body}{}", " ".repeat(16384 - body.len()));
    assert_eq!(padded.len(), 16384);
    let exact = lab.post(&cookie, "exact", &padded).await;
    let first = job_id(&exact);

    // One more, declared, is a 413 before any byte is read: the body is never sent.
    let started = Instant::now();
    let headers = job_headers(addr, &cookie, "declared");
    let stalled = format!(
        "POST /api/v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{}\r\n\
         Content-Length: 16385\r\n\r\n",
        addr.port(),
        headers.join("\r\n")
    );
    let declared = exchange(addr, &stalled).await;
    assert_eq!(declared.status, 413, "{}", declared.head);
    closes(&declared, "declared");
    assert!(started.elapsed() < Duration::from_secs(4), "answered early");

    // A body that grows past the limit without declaring a length is a 413 when it does: one
    // chunk just over the limit, with no terminating chunk, so nothing is left unread.
    let chunk = "x".repeat(16385);
    let chunked = format!(
        "POST /api/v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{}\r\n\
         Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}",
        addr.port(),
        job_headers(addr, &cookie, "chunked").join("\r\n"),
        chunk.len(),
    );
    let grown = exchange(addr, &chunked).await;
    assert_eq!(grown.status, 413, "{}", grown.head);
    closes(&grown, "grown");

    // A body that does not arrive within 5 seconds is a 408.
    let started = Instant::now();
    let slow = format!(
        "POST /api/v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{}\r\n\
         Content-Length: 10\r\n\r\n",
        addr.port(),
        job_headers(addr, &cookie, "slow").join("\r\n")
    );
    let late = exchange(addr, &slow).await;
    assert_eq!(late.status, 408, "{}", late.head);
    closes(&late, "late");
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // Only the first request made a job.
    lab.assert_jobs(1, "refusals").await;
    assert_eq!(lab.field(&first, "idempotency_key").await, "exact");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Creation: C1 to C4

#[tokio::test]
async fn an_acquire_job_is_stored_signed_and_answered_with_its_state() {
    // C1
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let r = lab.post(&cookie, "key-c1", &body).await;
    assert_eq!(r.status, 202, "{} {}", r.head, r.body);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert!(r.header("location").is_none());
    let id = job_id(&r);
    assert_eq!(
        json(&r),
        json!({
            "jobId": id, "kind": "acquire", "state": "received", "progressPermille": 0,
            "confirmation": { "level": "notifyOnly" }
        })
    );
    // A version 7 UUID with the variant bits of RFC 9562.
    assert_eq!(&id[14..15], "7");
    assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
    // Its time is the database's millisecond, rounded down: never ahead of the clock.
    let embedded = i64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).unwrap();
    let clock = lab
        .w
        .sql("SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint::text")
        .await
        .parse::<i64>()
        .unwrap();
    assert!(
        embedded <= clock && clock - embedded < 60_000,
        "{embedded} {clock}"
    );
    let row = lab
        .field(
            &id,
            "concat_ws('|', kind, state, progress_permille, confirmation_level, \
                 signing_key_id, (signed_job IS NOT NULL)::text, \
                 encode(request_hash, 'hex'), delivery_attempts, \
                 (delivery_sent_at IS NULL)::text, (delivered_at IS NULL)::text, \
                 remote::text, parameters::text, ir_digests::text, (vin IS NULL)::text, \
                 (artifact_digest IS NULL)::text, (finished_at IS NULL)::text)",
        )
        .await;
    let hash = body_hash(body.as_bytes()).unwrap();
    let hash_hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        row,
        format!(
            "acquire|received|0|notifyOnly|{}|true|{hash_hex}|0|true|true|false|{{}}|{{{DIGEST_A}}}|true|true|true",
            lab.w.key.key_id()
        )
    );
    assert_eq!(lab.w.sql("SELECT count(*) FROM vehicle_locks").await, "0");
    // A1: one `jobCreated` row, copied from the job; a job with no VIN has a NULL `vin`.
    let columns = lab
        .w
        .sql(&format!(
            "SELECT concat_ws('|', (a.tenant_id = j.tenant_id)::text, \
                 (a.operator_id = j.operator_id)::text, (a.agent_id = j.agent_id)::text, \
                 (a.job_id = j.id)::text, (a.vin IS NULL)::text, a.action, \
                 a.confirmation_level, (a.on_site_approved IS NULL)::text, \
                 (a.agent_reported_at IS NULL)::text, a.offline_executed::text, \
                 (a.recorded_at = j.created_at)::text, \
                 (SELECT string_agg(k, ',' ORDER BY k) FROM jsonb_object_keys(a.detail) k), \
                 a.detail->>'kind', a.detail->>'irDigests', a.detail->>'signingKeyId') \
             FROM audit_log a JOIN jobs j ON j.id = a.job_id WHERE a.job_id = '{id}'"
        ))
        .await;
    assert_eq!(
        columns,
        format!(
            "true|true|true|true|true|jobCreated|notifyOnly|true|true|false|true|\
             irDigests,kind,signingKeyId|acquire|[\"{DIGEST_A}\"]|{}",
            lab.w.key.key_id()
        )
    );
    lab.assert_audits(1, "A1").await;
    lab.assert_one_audit_per_job("A1").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_write_settings_job_has_the_grace_period_level() {
    // C2
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let r = lab
        .post(
            &cookie,
            "key-c2",
            &lab.body(json!({ "kind": "writeSettings" })),
        )
        .await;
    let id = job_id(&r);
    assert_eq!(json(&r)["kind"], "writeSettings");
    assert_eq!(json(&r)["confirmation"]["level"], "gracePeriod");
    assert_eq!(
        lab.field(&id, "kind || '|' || confirmation_level").await,
        "writeSettings|gracePeriod"
    );
    // A2: the row carries the kind and the level.
    assert_eq!(
        lab.w
            .sql(&format!(
                "SELECT confirmation_level || '|' || (detail->>'kind') FROM audit_log \
                 WHERE job_id = '{id}'"
            ))
            .await,
        "gracePeriod|writeSettings"
    );
    assert_eq!(lab.instruction(&id).await.kind, JobKind::WriteSettings);
    lab.finish().await;
}

// ADR-488: a job's level is read from `confirmation_defaults` when the job is created.
#[tokio::test]
async fn a_job_takes_its_level_from_the_defaults_table_as_it_stands_at_its_creation() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let acquire = lab.post(&cookie, "d1", &lab.body(json!({}))).await;
    let write = lab
        .post(&cookie, "d2", &lab.body(json!({ "kind": "writeSettings" })))
        .await;
    assert_eq!(json(&acquire)["confirmation"]["level"], "notifyOnly");
    assert_eq!(json(&write)["confirmation"]["level"], "gracePeriod");
    // A changed row reaches the next job and leaves the earlier one alone.
    lab.exec("UPDATE confirmation_defaults SET level = 'onSiteApproval' WHERE kind = 'acquire'")
        .await;
    let next = lab.post(&cookie, "d3", &lab.body(json!({}))).await;
    assert_eq!(json(&next)["confirmation"]["level"], "onSiteApproval");
    assert_eq!(
        lab.field(&job_id(&next), "confirmation_level").await,
        "onSiteApproval"
    );
    assert_eq!(
        lab.field(&job_id(&acquire), "confirmation_level").await,
        "notifyOnly"
    );
    lab.finish().await;
}

#[tokio::test]
async fn a_kind_without_a_default_row_is_a_500_and_leaves_nothing_behind() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    lab.exec("DELETE FROM confirmation_defaults WHERE kind = 'acquire'")
        .await;
    let body = lab.body(json!({ "vin": "WVWZZZ1JZXW000001" }));
    let r = lab.post(&cookie, "d4", &body).await;
    assert_problem(&r, 500, "no default row");
    lab.assert_jobs(0, "no job").await;
    lab.assert_audits(0, "no audit row").await;
    assert_eq!(
        lab.w.sql("SELECT count(*) FROM vehicle_locks").await,
        "0",
        "no lock"
    );
    // The key is unbound: with the row back, the same key and body create the job.
    lab.exec("INSERT INTO confirmation_defaults VALUES ('acquire', 'notifyOnly')")
        .await;
    assert_eq!(lab.post(&cookie, "d4", &body).await.status, 202);
    lab.assert_jobs(1, "created after the row is back").await;
    lab.finish().await;
}

#[tokio::test]
async fn the_vin_the_artifact_and_the_sorted_unique_digests_reach_the_instruction_and_the_row() {
    // C3 and C4
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let operator = lab.w.sql("SELECT id FROM operators").await;
    let body = lab.body(json!({
        "vin": VIN,
        "artifactDigest": DIGEST_B,
        "irDigests": [DIGEST_B, DIGEST_A, DIGEST_B],
    }));
    let id = job_id(&lab.post(&cookie, "key-c3", &body).await);
    let instruction = lab.instruction(&id).await;
    assert_eq!(instruction.version, INSTRUCTION_VERSION);
    assert_eq!(instruction.job_id.0, id);
    assert_eq!(instruction.ownership_generation, 1);
    assert_eq!(instruction.agent_id.0, lab.agent_id);
    assert_eq!(instruction.operator_id, operator);
    assert_eq!(instruction.kind, JobKind::Acquire);
    assert_eq!(instruction.vci.0, "vci-1");
    assert_eq!(instruction.vin.as_ref().map(|v| v.0.as_str()), Some(VIN));
    assert_eq!(instruction.document_hashes, vec![[0xaa; 32], [0xbb; 32]]);
    assert_eq!(instruction.artifact_hash, Some([0xbb; 32]));
    assert_eq!(instruction.plan_hash, None);
    assert_eq!(instruction.start_deadline - instruction.issued_at, 900);
    assert_eq!(
        lab.field(
            &id,
            "concat_ws('|', vin, artifact_digest, ir_digests::text, \
                 extract(epoch FROM start_deadline)::bigint, \
                 floor(extract(epoch FROM created_at))::bigint)"
        )
        .await,
        format!(
            "{VIN}|{DIGEST_B}|{{{DIGEST_A},{DIGEST_B}}}|{}|{}",
            instruction.start_deadline, instruction.issued_at
        )
    );
    // A2: the VIN is in the column and in no part of the detail, which holds the sorted unique
    // digests, the artifact and the key, and neither the VCI nor the request's hash.
    let audit = lab
        .w
        .sql(&format!(
            "SELECT concat_ws('|', vin, confirmation_level, detail::text) FROM audit_log \
             WHERE job_id = '{id}' AND action = 'jobCreated'"
        ))
        .await;
    let (head, detail) = audit.split_at(audit.find('{').unwrap());
    assert_eq!(head, format!("{VIN}|notifyOnly|"));
    assert_eq!(
        serde_json::from_str::<Value>(detail).unwrap(),
        json!({
            "kind": "acquire",
            "irDigests": [DIGEST_A, DIGEST_B],
            "artifactDigest": DIGEST_B,
            "signingKeyId": lab.w.key.key_id(),
        })
    );
    assert!(!detail.contains(VIN) && !detail.contains("vci-1") && !detail.contains("key-c3"));
    let hash_hex = lab.field(&id, "encode(request_hash, 'hex')").await;
    assert!(!detail.contains(&hash_hex));
    lab.assert_audits(1, "A2").await;
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The idempotency key: K1 to K8

#[tokio::test]
async fn the_same_operator_and_request_replay_the_stored_job_and_a_different_one_is_a_conflict() {
    // K1, K2, K3, K4, K5
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let first = lab.post(&cookie, "k", &body).await;
    let id = job_id(&first);
    let signed = lab.field(&id, "encode(signed_job, 'hex')").await;

    // K1
    let again = lab.post(&cookie, "k", &body).await;
    assert_eq!(job_id(&again), id);
    assert_eq!(normalized(&again), normalized(&first));
    lab.assert_jobs(1, "K1").await;
    lab.assert_audits(1, "A3: a replay writes no row").await;
    assert_eq!(lab.field(&id, "encode(signed_job, 'hex')").await, signed);

    // K2: another body under the key is a 422 with no data of the job.
    let other = lab
        .post(&cookie, "k", &lab.body(json!({"vciId": "vci-2"})))
        .await;
    assert_problem(&other, 422, "K2");
    assert!(!other.body.contains(&id));
    lab.assert_jobs(1, "K2").await;
    lab.assert_audits(1, "A4: a conflict writes no row").await;

    // K3: another operator of the tenant with the same body gets the same document.
    lab.user("bob", &["worker"]).await;
    let bob = lab.login("bob").await;
    let theirs = lab.post(&bob, "k", &body).await;
    assert_problem(&theirs, 422, "K3");
    assert_eq!(normalized(&theirs), normalized(&other));
    lab.assert_jobs(1, "K3").await;

    // K4: a row that holds no request hash is never replayed.
    lab.exec("UPDATE jobs SET request_hash = NULL").await;
    assert_problem(&lab.post(&cookie, "k", &body).await, 422, "K4");
    lab.assert_jobs(1, "K4").await;

    // K5: the key is the tenant's: another tenant holding it does not block a new job.
    lab.exec(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') \
                        RETURNING id) \
             UPDATE jobs SET tenant_id = (SELECT id FROM t)",
    )
    .await;
    let fresh = job_id(&lab.post(&cookie, "k", &body).await);
    assert_ne!(fresh, id);
    lab.assert_jobs(2, "K5").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_replay_shows_the_current_state_and_writes_nothing() {
    // K6
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    let snapshot = || {
        lab.w.sql(
            "SELECT (SELECT string_agg(row_to_json(j)::text, ',') FROM jobs j) || '#' || \
                    (SELECT string_agg(row_to_json(a)::text, ',') FROM audit_log a)",
        )
    };
    let before = snapshot().await;

    lab.exec("UPDATE jobs SET delivered_at = now(), delivery_attempts = 1")
        .await;
    let delivered = lab.post(&cookie, "k", &body).await;
    assert_eq!(job_id(&delivered), id);
    assert_eq!(json(&delivered)["state"], "received");

    lab.exec(
        "UPDATE jobs SET state = 'completed', progress_permille = 1000, \
                 finished_at = now()",
    )
    .await;
    let finished_snapshot = snapshot().await;
    assert_ne!(finished_snapshot, before);
    let finished = lab.post(&cookie, "k", &body).await;
    assert_eq!(job_id(&finished), id);
    assert_eq!(json(&finished)["state"], "completed");
    assert_eq!(json(&finished)["progressPermille"], 1000);
    assert_eq!(snapshot().await, finished_snapshot);
    lab.assert_jobs(1, "K6").await;
    // A3: the replays, after the delivery and after the end, left the one audit row as it was.
    lab.assert_audits(1, "A3 after delivery and end").await;
    assert!(before.contains("jobCreated"), "{before}");
    lab.finish().await;
}

#[tokio::test]
async fn a_replay_does_not_need_the_permission_the_creation_did() {
    // K7
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    let replay = |what: &'static str| {
        let (lab, cookie, body, id) = (&lab, &cookie, &body, &id);
        async move {
            assert_eq!(job_id(&lab.post(cookie, "k", body).await), *id, "{what}");
            lab.assert_jobs(1, what).await;
        }
    };
    lab.exec("DELETE FROM operator_roles").await;
    replay("the creator lost the role").await;
    lab.exec("DELETE FROM agent_allowed_operators").await;
    replay("the creator left the allow-list").await;
    lab.exec("UPDATE agents SET revoked_at = now()").await;
    replay("the agent was revoked").await;
    lab.exec("UPDATE instruction_keys SET retired_at = now()")
        .await;
    replay("the key was retired").await;
    lab.finish().await;
}

#[tokio::test]
async fn the_key_is_judged_before_the_permission() {
    // K8
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    lab.user("viewer", &["viewer"]).await;
    let viewer = lab.login("viewer").await;
    // Someone else's key: the conflict, which does not say that the job exists.
    let taken = lab.post(&viewer, "k", &body).await;
    assert_problem(&taken, 422, "someone else's key");
    assert!(!taken.body.contains(&id));
    // A key nobody used: the permission is judged, and refuses.
    let unused = lab.post(&viewer, "unused", &body).await;
    assert_problem(&unused, 403, "an unused key");
    lab.assert_jobs(1, "K8").await;
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Concurrency: K9 to K13

#[tokio::test]
async fn two_identical_requests_at_once_make_one_job_and_both_are_202() {
    // K9
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let (a, b) = tokio::join!(lab.post(&cookie, "k", &body), lab.post(&cookie, "k", &body));
    assert_eq!(job_id(&a), job_id(&b));
    lab.assert_jobs(1, "K9").await;
    lab.assert_one_audit_per_job("A8 K9").await;
    lab.finish().await;
}

#[tokio::test]
async fn two_bodies_for_one_key_at_once_make_one_job_for_the_first_and_a_conflict_for_the_other() {
    // K10
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let (one, two) = (
        lab.body(json!({"vciId": "vci-1"})),
        lab.body(json!({"vciId": "vci-2"})),
    );
    let (a, b) = tokio::join!(lab.post(&cookie, "k", &one), lab.post(&cookie, "k", &two));
    let (winner_body, winner, loser) = match (a.status, b.status) {
        (202, 422) => (&one, a, b),
        (422, 202) => (&two, b, a),
        other => panic!("one 202 and one 422 were due, not {other:?}"),
    };
    assert_problem(&loser, 422, "the loser");
    let id = job_id(&winner);
    let hash: String = body_hash(winner_body.as_bytes())
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(lab.field(&id, "encode(request_hash, 'hex')").await, hash);
    lab.assert_jobs(1, "K10").await;
    lab.assert_one_audit_per_job("A8 K10").await;
    lab.finish().await;
}

#[tokio::test]
async fn two_servers_on_one_database_make_one_job_for_one_key() {
    // K11
    let mut lab = Lab::new().await;
    let second = lab.another_server().await;
    let (first_cookie, second_cookie) = (lab.login("alice").await, login(second, "alice").await);
    let body = lab.body(json!({}));
    let (a, b) = tokio::join!(
        lab.post(&first_cookie, "k", &body),
        post_to(second, &second_cookie, "k", &body)
    );
    assert_eq!(job_id(&a), job_id(&b));
    lab.assert_jobs(1, "K11").await;
    lab.assert_one_audit_per_job("A8 K11").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_key_held_by_an_uncommitted_insert_is_decided_when_the_holder_ends() {
    // K12
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let hash = body_hash(body.as_bytes()).unwrap();
    let pool = lab.w.pg.pool();

    // The holder commits a row of the same request: the waiter replays it.
    let (tx, held) = hold_key(&pool, &lab.agent_id, "same", Some(hash)).await;
    let waiting = spawn_post(lab.addr(), &cookie, "same", &body);
    wait_for_insert_lock(&lab).await;
    tx.commit().await.unwrap();
    let r = waiting.await.unwrap();
    assert_eq!(job_id(&r), held);
    lab.assert_jobs(1, "replay").await;

    // The holder commits a row of another request: the waiter is a conflict.
    let (tx, held) = hold_key(&pool, &lab.agent_id, "other", Some([0u8; 32])).await;
    let waiting = spawn_post(lab.addr(), &cookie, "other", &body);
    wait_for_insert_lock(&lab).await;
    tx.commit().await.unwrap();
    let r = waiting.await.unwrap();
    assert_problem(&r, 422, "conflict");
    assert!(!r.body.contains(&held));
    lab.assert_jobs(2, "conflict").await;

    // The holder rolls back: the waiter creates the job.
    let (tx, held) = hold_key(&pool, &lab.agent_id, "gone", Some(hash)).await;
    let waiting = spawn_post(lab.addr(), &cookie, "gone", &body);
    wait_for_insert_lock(&lab).await;
    tx.rollback().await.unwrap();
    let created = job_id(&waiting.await.unwrap());
    assert_ne!(created, held);
    lab.assert_jobs(3, "rollback").await;
    assert_eq!(lab.field(&created, "idempotency_key").await, "gone");
    // A8: only the creation that inserted a row wrote one; the held rows came from the test.
    lab.assert_audits(1, "A8 K12").await;
    assert_eq!(
        lab.w.sql("SELECT job_id::text FROM audit_log").await,
        created
    );
    lab.finish().await;
}

#[tokio::test]
async fn the_replay_after_a_lost_insert_works_whatever_the_database_default_isolation() {
    // K12 again, with the database defaulting to repeatable read: the transaction states read
    // committed, so the re-read after the lost insert sees the holder's row.
    let mut lab = Lab::new().await;
    lab.w
        .pg
        .execute("ALTER DATABASE postgres SET default_transaction_isolation = 'repeatable read'")
        .await;
    // Pools are lazy: a server and a holder made now open sessions with the new default.
    let addr = lab.another_server().await;
    let cookie = login(addr, "alice").await;
    let body = lab.body(json!({}));
    let hash = body_hash(body.as_bytes()).unwrap();
    let pool = lab.w.pg.pool();
    let (tx, held) = hold_key(&pool, &lab.agent_id, "same", Some(hash)).await;
    let waiting = spawn_post(addr, &cookie, "same", &body);
    wait_for_insert_lock(&lab).await;
    tx.commit().await.unwrap();
    let r = waiting.await.unwrap();
    assert_eq!(r.status, 202, "{} {}", r.head, r.body);
    assert_eq!(job_id(&r), held);
    lab.assert_jobs(1, "replay").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_creation_waiting_on_a_held_key_does_not_block_the_agent_stamp() {
    // The agent's row is locked only after the insert, so the wait on the key holds nothing.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let pool = lab.w.pg.pool();
    let (tx, _held) = hold_key(&pool, &lab.agent_id, "k", None).await;
    let waiting = spawn_post(lab.addr(), &cookie, "k", &body);
    wait_for_insert_lock(&lab).await;
    let stamp = format!(
        "UPDATE agents SET last_seen_at = now() WHERE id = '{}'::uuid",
        lab.agent_id
    );
    tokio::time::timeout(Duration::from_secs(2), lab.exec(&stamp))
        .await
        .expect("the stamp waits behind the creation");
    assert!(!waiting.is_finished(), "the creation ended early");
    tx.rollback().await.unwrap();
    assert_eq!(waiting.await.unwrap().status, 202);
    lab.finish().await;
}

#[tokio::test]
async fn a_revocation_held_uncommitted_is_seen_by_the_agent_lock_and_the_job_is_refused() {
    // Also V13: the lock taken before the agent's row is read goes with the rolled-back job.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let pool = lab.w.pg.pool();
    let mut revoke = pool.begin().await.unwrap();
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&lab.agent_id)
        .execute(&mut *revoke)
        .await
        .unwrap();
    // The permission read does not see the uncommitted revocation; the lock waits for it.
    let waiting = spawn_post(lab.addr(), &cookie, "k", &body);
    wait_for_lock(&lab, "SELECT 1 FROM agents").await;
    revoke.commit().await.unwrap();
    let r = waiting.await.unwrap();
    assert_problem(&r, 403, "revoked while creating");
    lab.assert_jobs(0, "revoked while creating").await;
    lab.assert_audits(0, "A6: a revocation after the insert rolls the row back")
        .await;
    assert_eq!(lab.locks().await, "", "V13");
    lab.finish().await;
}

#[tokio::test]
async fn a_key_held_for_more_than_five_seconds_is_a_503() {
    // K13
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let pool = lab.w.pg.pool();
    let (tx, held) = hold_key(&pool, &lab.agent_id, "k", None).await;
    let started = Instant::now();
    let r = lab.post(&cookie, "k", &body).await;
    assert_problem(&r, 503, "K13");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    // The holder's row is the only one, and the waiter wrote nothing.
    tx.commit().await.unwrap();
    lab.assert_jobs(1, "K13").await;
    lab.assert_audits(0, "A7 K13").await;
    assert_eq!(lab.field(&held, "idempotency_key").await, "k");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Permission: A1 to A5

#[tokio::test]
async fn permission_needs_the_role_the_agent_and_its_allow_list_and_each_refusal_is_one_403() {
    let (logs, _guard) = capture();
    let lab = Lab::new().await;
    let body = lab.body(json!({}));
    let mut refusals: Vec<(&str, Reply)> = Vec::new();

    // A1: a viewer and an administrator are no workers.
    lab.user("viewer", &["viewer"]).await;
    lab.user("admin", &["admin"]).await;
    for name in ["viewer", "admin"] {
        let cookie = lab.login(name).await;
        refusals.push((name, lab.post(&cookie, &format!("k-{name}"), &body).await));
    }
    // A2: a worker whose scope is narrowed.
    lab.user("scoped", &["worker"]).await;
    lab.exec(
        "UPDATE operator_roles SET scope = '{\"oem\":[\"X\"]}'::jsonb \
             WHERE operator_id = (SELECT id FROM operators WHERE subject = 'scoped')",
    )
    .await;
    let scoped = lab.login("scoped").await;
    refusals.push(("scoped", lab.post(&scoped, "k-scoped", &body).await));

    let cookie = lab.login("alice").await;
    // A3: an agent nobody knows, and one of another tenant.
    let unknown = lab.body(json!({"agentId": "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab"}));
    refusals.push(("unknown", lab.post(&cookie, "k-unknown", &unknown).await));
    lab.exec(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'other') \
                        RETURNING id) \
             INSERT INTO agents (id, tenant_id, display_name, mode, public_key) \
             SELECT '1a1b2c3d-4e5f-4a6b-8c7d-0123456789ab', id, 'foreign', 'user', '\\x07' \
             FROM t",
    )
    .await;
    let foreign = lab.body(json!({"agentId": "1a1b2c3d-4e5f-4a6b-8c7d-0123456789ab"}));
    refusals.push(("foreign", lab.post(&cookie, "k-foreign", &foreign).await));
    // A5: the agent does not list the operator.
    lab.exec("DELETE FROM agent_allowed_operators").await;
    refusals.push(("allow-list", lab.post(&cookie, "k-allow", &body).await));
    // A4: a revoked agent.
    lab.exec("UPDATE agents SET revoked_at = now() WHERE id = (SELECT id FROM agents ORDER BY registered_at LIMIT 1)")
        .await;
    refusals.push(("revoked", lab.post(&cookie, "k-revoked", &body).await));

    for (what, reply) in &refusals {
        assert_problem(reply, 403, what);
        assert_eq!(normalized(reply), normalized(&refusals[0].1), "{what}");
    }
    lab.assert_jobs(0, "refusals").await;
    lab.assert_audits(0, "A6: a refusal before the insert writes no row")
        .await;
    let text = logs.text();
    for reason in [
        "not_worker",
        "agent_unknown",
        "agent_revoked",
        "not_allowed",
    ] {
        assert!(
            text.contains(&format!("reason=\"{reason}\"")),
            "{reason}: {text}"
        );
    }
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The server's own failures: S1 to S3

#[tokio::test]
async fn a_key_that_is_not_the_active_one_is_a_503_and_is_logged() {
    // S1
    let (logs, _guard) = capture();
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    for (what, sql) in [
        ("retired", "UPDATE instruction_keys SET retired_at = now()"),
        (
            "none",
            "DELETE FROM agent_registration_codes; DELETE FROM instruction_keys",
        ),
    ] {
        lab.exec(sql).await;
        let r = lab.post(&cookie, what, &body).await;
        assert_problem(&r, 503, what);
    }
    lab.assert_jobs(0, "S1").await;
    lab.assert_audits(0, "A7 S1").await;
    assert!(
        logs.text()
            .contains("the instruction key is not the active key"),
        "{}",
        logs.text()
    );
    lab.finish().await;
}

#[tokio::test]
async fn a_signer_that_fails_is_a_500_and_leaves_no_row() {
    // S2
    let (logs, _guard) = capture();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let key = Arc::new(InstructionKey::from_signer(Arc::new(Flaky {
        key: SigningKey::generate().unwrap(),
        fail: fail.clone(),
    })));
    let lab = Lab::build(key, None).await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    // The registration needed the signer, so it fails only from here on.
    fail.store(true, Ordering::SeqCst);
    let r = lab.post(&cookie, "k", &body).await;
    assert_problem(&r, 500, "S2");
    lab.assert_jobs(0, "S2").await;
    lab.assert_audits(0, "A7 S2").await;
    assert!(logs.text().contains("job creation failed"));
    fail.store(false, Ordering::SeqCst);
    assert_eq!(lab.post(&cookie, "k", &body).await.status, 202);
    lab.finish().await;
}

#[tokio::test]
async fn a_statement_that_waits_for_a_lock_on_jobs_for_five_seconds_is_a_503() {
    // S3
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let pool = lab.w.pg.pool();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let started = Instant::now();
    let r = lab.post(&cookie, "k", &body).await;
    assert_problem(&r, 503, "S3");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
    lab.assert_jobs(0, "S3").await;
    lab.assert_audits(0, "A7 S3").await;
    assert_eq!(lab.post(&cookie, "k", &body).await.status, 202);
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// To the agent: E1

#[tokio::test]
async fn a_created_job_is_delivered_once_with_its_stored_bytes_and_a_replay_sends_nothing() {
    // E1
    let lab = Lab::build(instruction_key(), Some(settings(LONG))).await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({}));
    let first = job_id(&lab.post(&cookie, "k1", &body).await);

    let mut client = open(lab.addr(), &lab.agent_token().await).await;
    let (command_id, bytes) = command(&mut client).await;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT signed_job FROM jobs WHERE id = $1::uuid")
        .bind(&first)
        .fetch_one(&lab.w.pool)
        .await
        .unwrap();
    assert_eq!(bytes, stored);
    ack(&mut client, &command_id).await;
    until_row(&lab.w, &first, DELIVERED).await;

    // The replay is the stored job again, and the next frame is the next job's, not this one's.
    let replay = lab.post(&cookie, "k1", &body).await;
    assert_eq!(job_id(&replay), first);
    let second_body = lab.body(json!({"vciId": "vci-2"}));
    let second = job_id(&lab.post(&cookie, "k2", &second_body).await);
    let (next_id, next_bytes) = command(&mut client).await;
    assert_eq!(next_bytes, lab.instruction_bytes(&second).await);
    ack(&mut client, &next_id).await;
    until_row(&lab.w, &second, DELIVERED).await;
    assert_eq!(row(&lab.w, &first).await, DELIVERED);
    quiet_for(&mut client, 300).await;
    lab.finish().await;
}

impl Lab {
    /// The stored envelope of the job.
    async fn instruction_bytes(&self, job: &str) -> Vec<u8> {
        sqlx::query_scalar("SELECT signed_job FROM jobs WHERE id = $1::uuid")
            .bind(job)
            .fetch_one(&self.w.pool)
            .await
            .unwrap()
    }
}

// ---------------------------------------------------------------------------------------------
// The vehicle lock (design 8.8): V2 to V14

impl Lab {
    /// The locks as `vin>job` pairs, in VIN order.
    async fn locks(&self) -> String {
        self.w
            .sql(
                "SELECT coalesce(string_agg(vin || '>' || job_id::text, ',' ORDER BY vin), '') \
                 FROM vehicle_locks",
            )
            .await
    }

    async fn tenant(&self) -> String {
        self.w.sql("SELECT id::text FROM tenants").await
    }

    /// Moves the clock of the lock of `vin` forward by the interval, by moving its times back.
    async fn age_lock(&self, vin: &str, interval: &str) {
        self.exec(&format!(
            "UPDATE vehicle_locks SET acquired_at = acquired_at - {interval}, \
                 expires_at = expires_at - {interval} WHERE vin = '{vin}'"
        ))
        .await;
    }
}

fn vehicle_locked_body() -> Value {
    json!({
        "type": "about:blank", "title": "Another job holds this vehicle", "status": 409,
        "code": "VEHICLE_LOCKED"
    })
}

#[tokio::test]
async fn a_job_with_a_vin_takes_the_lock_and_each_kind_holds_it_until_its_default_expiry() {
    // V2 and V5: acquire holds 600 s and writeSettings 1800 s after the start deadline.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    for (kind, vin, allowance) in [("acquire", VIN, 600), ("writeSettings", VIN_2, 1800)] {
        let body = lab.body(json!({ "kind": kind, "vin": vin }));
        let id = job_id(&lab.post(&cookie, &format!("{kind}-1"), &body).await);
        // V2: the row names the job and its agent, and ends the allowance after the deadline.
        let row = lab
            .w
            .sql(&format!(
                "SELECT concat_ws('|', l.job_id = j.id, l.agent_id = j.agent_id, \
                     l.tenant_id = j.tenant_id, \
                     round(extract(epoch FROM l.expires_at - j.start_deadline))::bigint, \
                     l.acquired_at <= now() AND l.acquired_at > now() - interval '1 minute') \
                 FROM vehicle_locks l JOIN jobs j ON j.id = l.job_id WHERE l.vin = '{vin}'"
            ))
            .await;
        assert_eq!(row, format!("t|t|t|{allowance}|t"), "{kind}");
        assert_eq!(lab.field(&id, "vin").await, vin);

        // V5: 30 s before the default expiry the lock holds; 1 s after it, it is replaced.
        lab.exec(&format!(
            "UPDATE vehicle_locks l SET \
               acquired_at = acquired_at - (j.start_deadline + {allowance} * interval '1 second' \
                                           - interval '30 seconds' - now()), \
               expires_at = expires_at - (j.start_deadline + {allowance} * interval '1 second' \
                                           - interval '30 seconds' - now()) \
             FROM jobs j WHERE j.id = l.job_id AND l.vin = '{vin}'"
        ))
        .await;
        let held = lab.post(&cookie, &format!("{kind}-2"), &body).await;
        assert_problem(&held, 409, kind);
        assert_eq!(lab.locks().await.matches(&id).count(), 1, "{kind}");
        lab.age_lock(vin, "interval '31 seconds'").await;
        let next = job_id(&lab.post(&cookie, &format!("{kind}-3"), &body).await);
        assert_eq!(
            lab.locks().await.matches(&format!("{vin}>{next}")).count(),
            1,
            "{kind}"
        );
    }
    lab.finish().await;
}

#[tokio::test]
async fn a_second_job_for_a_locked_vehicle_is_a_409_that_names_no_holder_and_leaves_no_trace() {
    // V3, V4, V8, V9
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let held = lab.body(json!({ "vin": VIN }));
    let holder = job_id(&lab.post(&cookie, "k-holder", &held).await);
    let before = lab.w.sql("SELECT count(*) FROM instruction_keys").await;

    // V3: the body is the Problem with the code and nothing about the holder or the vehicle.
    let r = lab.post(&cookie, "k-second", &held).await;
    assert_problem(&r, 409, "V3");
    assert_eq!(json(&r), vehicle_locked_body());
    assert!(r.header("retry-after").is_none());
    assert!(!r.head.contains(VIN) && !r.body.contains(&holder));
    lab.assert_jobs(1, "V3").await;
    lab.assert_audits(1, "A5: the refused creation wrote no row")
        .await;
    assert_eq!(lab.locks().await, format!("{VIN}>{holder}"));
    assert_eq!(
        lab.w
            .sql("SELECT count(*) FROM jobs WHERE idempotency_key = 'k-second'")
            .await,
        "0"
    );
    assert_eq!(
        lab.w.sql("SELECT count(*) FROM instruction_keys").await,
        before
    );

    // V8 and V9: a job with no VIN, one with another VIN, and one for the same VIN in another
    // tenant are not refused, and only the ones with a VIN take a lock.
    let none = job_id(&lab.post(&cookie, "k-none", &lab.body(json!({}))).await);
    assert_eq!(lab.field(&none, "vin IS NULL").await, "true");
    let other_vin = lab.body(json!({ "vin": VIN_2 }));
    let other = job_id(&lab.post(&cookie, "k-other", &other_vin).await);
    assert_eq!(lab.locks().await, format!("{VIN}>{holder},{VIN_2}>{other}"));
    lab.exec(&format!(
        "DELETE FROM vehicle_locks WHERE vin = '{VIN}'; \
         INSERT INTO vehicle_locks (tenant_id, vin, job_id, agent_id, expires_at) \
         VALUES (gen_random_uuid(), '{VIN}', gen_random_uuid(), gen_random_uuid(), \
                 now() + interval '1 hour')"
    ))
    .await;
    let foreign = job_id(&lab.post(&cookie, "k-foreign", &held).await);
    assert_eq!(
        lab.w
            .sql(&format!(
                "SELECT count(*) FROM vehicle_locks WHERE vin = '{VIN}'"
            ))
            .await,
        "2"
    );
    assert_eq!(
        lab.w
            .sql(&format!(
                "SELECT job_id::text FROM vehicle_locks WHERE vin = '{VIN}' \
                 AND tenant_id = '{}'::uuid",
                lab.tenant().await
            ))
            .await,
        foreign
    );

    // V4: the refused key was not bound; once the lock is free it creates a job.
    lab.exec(&format!("DELETE FROM vehicle_locks WHERE vin = '{VIN}'"))
        .await;
    let later = job_id(&lab.post(&cookie, "k-second", &held).await);
    assert_ne!(later, holder);
    assert_eq!(lab.field(&later, "idempotency_key").await, "k-second");
    // A5: the later job has its row; the refused attempts left none.
    assert_eq!(
        lab.w
            .sql("SELECT count(*) FROM audit_log WHERE job_id IN (SELECT id FROM jobs)")
            .await,
        lab.jobs().await
    );
    lab.assert_one_audit_per_job("A5 V4").await;
    lab.finish().await;
}

#[tokio::test]
async fn an_expired_lock_is_replaced_by_the_next_job_and_an_unexpired_one_is_kept() {
    // V6
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let first = job_id(&lab.post(&cookie, "k1", &body).await);
    // One hour left: held.
    lab.exec("UPDATE vehicle_locks SET expires_at = now() + interval '1 hour'")
        .await;
    assert_problem(&lab.post(&cookie, "k2", &body).await, 409, "1 h left");
    assert_eq!(lab.locks().await, format!("{VIN}>{first}"));
    // Expired a millisecond ago: replaced, with a fresh acquisition time.
    lab.exec(
        "UPDATE vehicle_locks SET expires_at = now() - interval '1 millisecond', \
             acquired_at = now() - interval '2 hours'",
    )
    .await;
    let second = job_id(&lab.post(&cookie, "k3", &body).await);
    assert_eq!(lab.locks().await, format!("{VIN}>{second}"));
    assert_eq!(
        lab.w
            .sql("SELECT (acquired_at > now() - interval '1 minute')::text FROM vehicle_locks")
            .await,
        "true"
    );
    lab.assert_jobs(2, "V6").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_replay_of_the_holders_key_is_a_202_before_any_lock_is_read() {
    // V7 and V14: after the lock expired and another job took it, and while it is held.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let first = lab.post(&cookie, "k1", &body).await;
    let holder = job_id(&first);
    assert_eq!(
        normalized(&lab.post(&cookie, "k1", &body).await),
        normalized(&first)
    );
    lab.age_lock(VIN, "interval '1 day'").await;
    let taker = job_id(&lab.post(&cookie, "k2", &body).await);
    let replay = lab.post(&cookie, "k1", &body).await;
    assert_eq!(job_id(&replay), holder);
    assert_eq!(lab.locks().await, format!("{VIN}>{taker}"));
    lab.assert_jobs(2, "V7").await;
    // V14: the key's answers come before the lock's: another request under a used key is a
    // 422, and a user with no permission is a 403, whatever holds the vehicle.
    let other = lab.body(json!({ "vin": VIN, "vciId": "vci-9" }));
    assert_problem(&lab.post(&cookie, "k2", &other).await, 422, "V14 key");
    lab.user("viewer", &["viewer"]).await;
    let viewer = lab.login("viewer").await;
    assert_problem(
        &lab.post(&viewer, "k-viewer", &body).await,
        403,
        "V14 permission",
    );
    assert_problem(&lab.post(&cookie, "k3", &body).await, 409, "V14 lock");
    lab.assert_jobs(2, "V14").await;
    lab.finish().await;
}

#[tokio::test]
async fn creations_at_once_for_one_vehicle_make_one_job_on_one_server_or_two() {
    // V10
    let mut lab = Lab::new().await;
    let second = lab.another_server().await;
    let (c1, c2) = (lab.login("alice").await, login(second, "alice").await);
    let body = lab.body(json!({ "vin": VIN }));
    let (a, b) = tokio::join!(
        lab.post(&c1, "k-a", &body),
        post_to(second, &c2, "k-b", &body)
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [202, 409], "{} {}", a.body, b.body);
    lab.assert_jobs(1, "V10 two servers").await;
    lab.assert_one_audit_per_job("A8 V10 two servers").await;
    let other = lab.body(json!({ "vin": VIN_2 }));
    let (a, b) = tokio::join!(lab.post(&c1, "k-c", &other), lab.post(&c1, "k-d", &other));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [202, 409], "{} {}", a.body, b.body);
    lab.assert_jobs(2, "V10 one server").await;
    lab.assert_one_audit_per_job("A8 V10 one server").await;
    lab.finish().await;
}

#[tokio::test]
async fn two_takers_of_an_expired_lock_get_one_job_and_one_refusal() {
    // V12
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    job_id(&lab.post(&cookie, "k0", &body).await);
    lab.age_lock(VIN, "interval '1 day'").await;
    let (a, b) = tokio::join!(
        lab.post(&cookie, "k-a", &body),
        lab.post(&cookie, "k-b", &body)
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [202, 409], "{} {}", a.body, b.body);
    lab.assert_jobs(2, "V12").await;
    lab.assert_one_audit_per_job("A8 V12").await;
    assert_eq!(lab.w.sql("SELECT count(*) FROM vehicle_locks").await, "1");
    lab.finish().await;
}

impl Lab {
    /// An uncommitted lock row for `vin`, held by a transaction of the test.
    async fn hold_lock_uncommitted(&self, vin: &str) -> sqlx::Transaction<'static, Postgres> {
        let mut tx = self.w.pg.pool().begin().await.unwrap();
        sqlx::query(
            "INSERT INTO vehicle_locks (tenant_id, vin, job_id, agent_id, expires_at) \
             SELECT tenant_id, $2, gen_random_uuid(), id, now() + interval '1 hour' \
             FROM agents WHERE id = $1::uuid",
        )
        .bind(&self.agent_id)
        .bind(vin)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx
    }
}

#[tokio::test]
async fn a_lock_held_by_an_uncommitted_take_is_decided_when_it_ends_and_blocks_no_stamp() {
    // V11: a commit makes it a 409, a rollback a 202; the wait holds no agent row.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));

    let tx = lab.hold_lock_uncommitted(VIN).await;
    let waiting = spawn_post(lab.addr(), &cookie, "k-commit", &body);
    wait_for_lock(&lab, "INSERT INTO vehicle_locks").await;
    let stamp = format!(
        "UPDATE agents SET last_seen_at = now() WHERE id = '{}'::uuid",
        lab.agent_id
    );
    tokio::time::timeout(Duration::from_secs(2), lab.exec(&stamp))
        .await
        .expect("the stamp waits behind the creation");
    assert!(!waiting.is_finished(), "the creation ended early");
    tx.commit().await.unwrap();
    assert_problem(&waiting.await.unwrap(), 409, "holder committed");
    lab.assert_jobs(0, "holder committed").await;
    lab.assert_audits(0, "A5 holder committed").await;

    lab.exec(&format!("DELETE FROM vehicle_locks WHERE vin = '{VIN}'"))
        .await;
    let tx = lab.hold_lock_uncommitted(VIN).await;
    let waiting = spawn_post(lab.addr(), &cookie, "k-rollback", &body);
    wait_for_lock(&lab, "INSERT INTO vehicle_locks").await;
    tx.rollback().await.unwrap();
    let id = job_id(&waiting.await.unwrap());
    assert_eq!(lab.locks().await, format!("{VIN}>{id}"));
    lab.finish().await;
}

#[tokio::test]
async fn a_lock_held_uncommitted_for_more_than_five_seconds_is_a_503() {
    // V11
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let tx = lab.hold_lock_uncommitted(VIN).await;
    let started = Instant::now();
    let r = lab
        .post(&cookie, "k", &lab.body(json!({ "vin": VIN })))
        .await;
    assert_problem(&r, 503, "V11");
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    tx.rollback().await.unwrap();
    lab.assert_jobs(0, "V11").await;
    lab.assert_audits(0, "A7 V11").await;
    lab.finish().await;
}

#[tokio::test]
async fn the_same_vin_in_two_tenants_has_a_row_in_each_with_the_jobs_tenant() {
    // A9: the lock is per tenant, so the second tenant's job for the VIN is created; each job is
    // made through the route by a worker of its own tenant, and nothing edits `audit_log`.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let (cookie_2, agent_2, tenant_2) = lab.second_tenant().await;
    let first = job_id(
        &lab.post(&cookie, "k-1", &lab.body(json!({ "vin": VIN })))
            .await,
    );
    let second = job_id(
        &lab.post(
            &cookie_2,
            "k-2",
            &lab.body(json!({ "vin": VIN, "agentId": agent_2 })),
        )
        .await,
    );
    assert_ne!(first, second);
    assert_ne!(tenant_2, lab.field(&first, "tenant_id").await);
    assert_eq!(
        lab.field(&second, "tenant_id").await,
        tenant_2,
        "the second job is the second tenant's"
    );
    assert_eq!(
        lab.w
            .sql(&format!(
                "SELECT count(*) FROM audit_log a JOIN jobs j ON j.id = a.job_id \
                 AND j.tenant_id = a.tenant_id WHERE a.vin = '{VIN}' \
                   AND a.action = 'jobCreated'"
            ))
            .await,
        "2"
    );
    assert_eq!(
        lab.w
            .sql("SELECT count(DISTINCT tenant_id) FROM audit_log")
            .await,
        "2"
    );
    lab.assert_one_audit_per_job("A9").await;
    lab.finish().await;
}

#[tokio::test]
async fn an_audit_log_locked_for_five_seconds_is_a_503_that_creates_no_job_and_blocks_no_stamp() {
    // A11: the audit insert waits behind a lock on its table, after the job insert and before
    // the agent's row is locked, so the wait times out into a rollback that holds nothing.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let pool = lab.w.pg.pool();
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE audit_log IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let waiting = spawn_post(lab.addr(), &cookie, "k", &body);
    wait_for_lock(&lab, "INSERT INTO audit_log").await;
    // The agent's connect stamp is not blocked by the creation that waits.
    let stamp = format!(
        "UPDATE agents SET last_seen_at = now() WHERE id = '{}'::uuid",
        lab.agent_id
    );
    tokio::time::timeout(Duration::from_secs(2), lab.exec(&stamp))
        .await
        .expect("the stamp waits behind the creation");
    let started = Instant::now();
    let r = waiting.await.unwrap();
    assert_problem(&r, 503, "A11");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    lock.rollback().await.unwrap();
    lab.assert_jobs(0, "A11").await;
    assert_eq!(lab.locks().await, "", "A11");
    lab.assert_audits(0, "A11").await;
    // The key was not bound: the same request now creates its job and its row.
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    assert_eq!(lab.field(&id, "idempotency_key").await, "k");
    lab.assert_one_audit_per_job("A11 later").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_creation_waiting_on_the_audit_log_holds_no_vehicle_lock() {
    // A11: the audit row comes before the vehicle lock, so a creation that waits for the audit
    // insert has not taken the vehicle's lock row, and a take of it by another transaction is
    // not blocked (a take placed after the audit insert would leave it waiting behind the
    // creation's uncommitted row).
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let mut lock = lab.w.pg.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE audit_log IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let waiting = spawn_post(lab.addr(), &cookie, "k", &body);
    wait_for_lock(&lab, "INSERT INTO audit_log").await;
    let take = tokio::time::timeout(Duration::from_secs(2), lab.hold_take(VIN))
        .await
        .expect(
            "the take of the vehicle's lock waits behind the creation that waits for the audit row",
        );
    assert!(!waiting.is_finished(), "the creation ended early");
    take.rollback().await.unwrap();
    lock.rollback().await.unwrap();
    let id = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the creation does not end")
        .unwrap();
    let id = job_id(&id);
    assert_eq!(lab.locks().await, format!("{VIN}>{id}"));
    lab.assert_one_audit_per_job("A11 order").await;
    lab.finish().await;
}

#[tokio::test]
async fn the_refusal_for_a_locked_vehicle_is_logged_once_without_the_vehicle() {
    // V3: its own gate, the operator only.
    let (logs, _guard) = capture();
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let holder = job_id(&lab.post(&cookie, "k-holder", &body).await);
    for key in ["k-1", "k-2"] {
        assert_problem(&lab.post(&cookie, key, &body).await, 409, key);
    }
    let text = logs.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("the vehicle is locked"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains("operator_id="), "{}", lines[0]);
    // The holder's own "job created" line names its ID; the refusal's line names no holder.
    assert!(!text.contains(VIN), "{text}");
    assert!(!lines[0].contains(&holder), "{}", lines[0]);
    lab.finish().await;
}

impl Lab {
    /// An uncommitted take of the lock row of `vin` as the creation runs it, which locks the row.
    async fn hold_take(&self, vin: &str) -> sqlx::Transaction<'static, Postgres> {
        let mut tx = self.w.pg.pool().begin().await.unwrap();
        sqlx::query(
            "INSERT INTO vehicle_locks AS l (tenant_id, vin, job_id, agent_id, expires_at) \
             SELECT tenant_id, $2, gen_random_uuid(), id, now() + interval '1 hour' \
             FROM agents WHERE id = $1::uuid \
             ON CONFLICT (tenant_id, vin) DO UPDATE SET job_id = EXCLUDED.job_id, \
                 agent_id = EXCLUDED.agent_id, acquired_at = now(), \
                 expires_at = EXCLUDED.expires_at \
             WHERE l.expires_at <= now()",
        )
        .bind(&self.agent_id)
        .bind(vin)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx
    }
}

#[tokio::test]
async fn a_creation_waiting_on_an_uncommitted_replacement_of_an_expired_lock_is_decided_when_it_ends()
 {
    // V11 and V12: a commit leaves a live lock and a 409, a rollback leaves the expired one and a
    // 202.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    job_id(&lab.post(&cookie, "k0", &body).await);
    lab.age_lock(VIN, "interval '1 day'").await;

    let tx = lab.hold_take(VIN).await;
    let waiting = spawn_post(lab.addr(), &cookie, "k-commit", &body);
    wait_for_lock(&lab, "INSERT INTO vehicle_locks").await;
    assert!(!waiting.is_finished(), "the creation ended early");
    tx.commit().await.unwrap();
    assert_problem(&waiting.await.unwrap(), 409, "replacement committed");
    lab.assert_jobs(1, "replacement committed").await;
    lab.assert_audits(1, "A5: the refused creation left no row")
        .await;

    lab.age_lock(VIN, "interval '1 day'").await;
    let tx = lab.hold_take(VIN).await;
    let waiting = spawn_post(lab.addr(), &cookie, "k-rollback", &body);
    wait_for_lock(&lab, "INSERT INTO vehicle_locks").await;
    tx.rollback().await.unwrap();
    let id = job_id(&waiting.await.unwrap());
    assert_eq!(lab.locks().await, format!("{VIN}>{id}"));
    lab.assert_jobs(2, "replacement rolled back").await;
    lab.finish().await;
}

#[tokio::test]
async fn an_end_of_the_old_job_waits_for_an_uncommitted_take_and_then_deletes_nothing() {
    // V16: the queue ends J_old by a rejection while a take of its expired lock is uncommitted.
    let lab = Lab::build(instruction_key(), Some(settings(LONG))).await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "vin": VIN }));
    let old = job_id(&lab.post(&cookie, "k-old", &body).await);
    lab.age_lock(VIN, "interval '1 day'").await;
    let mut client = open(lab.addr(), &lab.agent_token().await).await;
    let (command_id, _) = command(&mut client).await;
    until_row(&lab.w, &old, SENT).await;

    let tx = lab.hold_take(VIN).await;
    reject(&mut client, &command_id, "badSignature").await;
    wait_for_lock(&lab, "WITH ended AS").await;
    tx.commit().await.unwrap();
    until_row(
        &lab.w,
        &old,
        "failed|1|true|false|instructionRejected|badSignature|true",
    )
    .await;
    let holder = lab.w.sql("SELECT job_id::text FROM vehicle_locks").await;
    assert_ne!(holder, old);
    assert_eq!(lab.locks().await, format!("{VIN}>{holder}"));
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The lock and the reported state (ADR-481): J17 to J19

impl Lab {
    /// A lab whose queue is on and whose agent is connected; the agent's session.
    async fn with_agent() -> (Lab, crate::control::Client, String) {
        let lab = Lab::build(instruction_key(), Some(settings(LONG))).await;
        let client = open(lab.addr(), &lab.agent_token().await).await;
        let cookie = lab.login("alice").await;
        (lab, client, cookie)
    }

    /// Creates a job for the agent with `vin` under `key` and reads its command off the agent's
    /// connection; the job's ID.
    async fn job_for(
        &self,
        client: &mut crate::control::Client,
        cookie: &str,
        key: &str,
        vin: &str,
    ) -> String {
        let r = self
            .post(cookie, key, &self.body(json!({ "vin": vin })))
            .await;
        assert_eq!(r.status, 202, "{}", r.head);
        let id = job_id(&r);
        command(client).await;
        id
    }
}

// J17
#[tokio::test]
async fn the_lock_is_held_until_the_agent_reports_an_end_and_a_recovery_is_an_end() {
    let (lab, mut client, cookie) = Lab::with_agent().await;
    let held = lab.job_for(&mut client, &cookie, "k-1", VIN).await;
    let other = lab.job_for(&mut client, &cookie, "k-2", VIN_2).await;
    let refused = lab.body(json!({ "vin": VIN }));
    assert_eq!(lab.post(&cookie, "k-3", &refused).await.status, 409);
    let audit_of_the_two = format!(
        "SELECT string_agg(row_to_json(a)::text, ',' ORDER BY id) FROM audit_log a \
         WHERE job_id IN ('{held}', '{other}')"
    );
    let created = lab.w.sql(&audit_of_the_two).await;
    assert_eq!(created.matches("jobCreated").count(), 2, "{created}");
    // A state that is not an end frees nothing.
    say(&mut client, 1, &held, "running", "").await;
    assert_eq!(lab.post(&cookie, "k-3", &refused).await.status, 409);
    assert_eq!(lab.locks().await, format!("{VIN}>{held},{VIN_2}>{other}"));
    // An end frees the job's own lock, and only that one.
    say(&mut client, 2, &held, "completed", "").await;
    assert_eq!(lab.locks().await, format!("{VIN_2}>{other}"));
    let r = lab.post(&cookie, "k-3", &refused).await;
    assert_eq!(r.status, 202, "{}", r.head);
    command(&mut client).await;
    // So does an agent's needsOnSiteRecovery, the one end a later report may replace.
    say(
        &mut client,
        3,
        &other,
        "needsOnSiteRecovery",
        r#","detail":"cable""#,
    )
    .await;
    assert_eq!(lab.locks().await, format!("{VIN}>{}", job_id(&r)));
    let r = lab
        .post(&cookie, "k-4", &lab.body(json!({ "vin": VIN_2 })))
        .await;
    assert_eq!(r.status, 202, "{}", r.head);
    // A10: the two ends added no row, and deleting the jobs leaves the rows as they were.
    lab.assert_audits(4, "A10 after the ends").await;
    assert_eq!(lab.w.sql(&audit_of_the_two).await, created);
    lab.exec("DELETE FROM jobs").await;
    lab.assert_audits(4, "A10 after the jobs' deletion").await;
    assert_eq!(lab.w.sql(&audit_of_the_two).await, created);
    lab.finish().await;
}

// J18
#[tokio::test]
async fn the_end_of_a_job_whose_lock_expired_and_was_taken_leaves_the_new_holders_row() {
    let (lab, mut client, cookie) = Lab::with_agent().await;
    let old = lab.job_for(&mut client, &cookie, "k-old", VIN).await;
    lab.age_lock(VIN, "interval '1 day'").await;
    let new = lab.job_for(&mut client, &cookie, "k-new", VIN).await;
    assert_eq!(lab.locks().await, format!("{VIN}>{new}"));
    say(&mut client, 1, &old, "completed", "").await;
    assert_eq!(lab.field(&old, "state").await, "completed");
    assert_eq!(lab.locks().await, format!("{VIN}>{new}"));
    lab.finish().await;
}

// J19
#[tokio::test]
async fn a_replay_of_a_failed_job_shows_its_failure_code_and_procedure_code_and_no_detail() {
    let (lab, mut client, cookie) = Lab::with_agent().await;
    let body = lab.body(json!({ "vin": VIN }));
    let r = lab.post(&cookie, "k-failed", &body).await;
    let job = job_id(&r);
    command(&mut client).await;
    assert!(json(&r).get("failure").is_none());
    say(
        &mut client,
        1,
        &job,
        "failed",
        r#","failureCode":"procedure","procedureCode":"P-17","detail":"SECRET-DETAIL""#,
    )
    .await;
    let replay = lab.post(&cookie, "k-failed", &body).await;
    assert_eq!(replay.status, 202, "{}", replay.head);
    assert_eq!(json(&replay)["state"], "failed");
    assert_eq!(
        json(&replay)["failure"],
        json!({ "code": "procedure", "procedureCode": "P-17" })
    );
    assert!(!replay.body.contains("SECRET-DETAIL"));
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The request's start deadline (ADR-487): F2 to F15 and the creation's half of the expiry

/// The database's clock in whole seconds since the epoch.
async fn db_seconds(lab: &Lab) -> i64 {
    lab.w
        .sql("SELECT floor(extract(epoch FROM now()))::bigint")
        .await
        .parse()
        .unwrap()
}

/// The RFC 3339 text of the instant `epoch` as the clock `offset_secs` east of UTC shows it, with
/// `suffix` (a fraction and the offset) after the seconds.
async fn rfc3339(lab: &Lab, epoch: i64, offset_secs: i64, suffix: &str) -> String {
    let local = lab
        .w
        .sql(&format!(
            "SELECT to_char(to_timestamp({}) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS')",
            epoch + offset_secs
        ))
        .await;
    format!("{local}{suffix}")
}

async fn utc(lab: &Lab, epoch: i64) -> String {
    rfc3339(lab, epoch, 0, "Z").await
}

// F2, F6, F7, W1, W2: the second the request names is the row's, the instruction's and the
// lock's base, whatever form it came in.
#[tokio::test]
async fn a_start_deadline_in_the_bounds_is_signed_stored_and_normalized_to_utc_seconds() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let deadline = now + 3600;
    let zoned = rfc3339(&lab, deadline, 9 * 3600, ".789+09:00").await;
    let body = lab.body(json!({ "vin": VIN, "startDeadline": zoned }));
    let first = lab.post(&cookie, "k", &body).await;
    let id = job_id(&first);
    let instruction = lab.instruction(&id).await;
    assert_eq!(instruction.start_deadline, deadline as u64);
    assert_eq!(
        lab.field(&id, "extract(epoch FROM start_deadline)::bigint")
            .await,
        deadline.to_string()
    );
    assert!(
        (now as u64..=now as u64 + 5).contains(&instruction.issued_at),
        "{}",
        instruction.issued_at
    );
    // W2: the lock ends an allowance after the deadline.
    assert_eq!(
        lab.w
            .sql(
                "SELECT extract(epoch FROM l.expires_at - j.start_deadline)::bigint \
                 FROM vehicle_locks l JOIN jobs j ON j.id = l.job_id"
            )
            .await,
        "600"
    );
    // F6, F7: the same instant in another form, or with another fraction, is the same request.
    let hash = lab.field(&id, "encode(request_hash, 'hex')").await;
    for form in [
        utc(&lab, deadline).await,
        rfc3339(&lab, deadline, 0, ".000Z").await,
        rfc3339(&lab, deadline, 0, "-00:00").await,
        rfc3339(&lab, deadline, -5 * 3600, ".999999999-05:00").await,
    ] {
        let same = lab.body(json!({ "vin": VIN, "startDeadline": form }));
        let again = lab.post(&cookie, "k", &same).await;
        assert_eq!(job_id(&again), id, "{form}");
        assert_eq!(hex_of(body_hash(same.as_bytes()).unwrap()), hash, "{form}");
    }
    lab.assert_jobs(1, "replays").await;
    lab.finish().await;
}

fn hex_of(hash: [u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

// F4, F14: a deadline outside the bounds is a 422 with its own code, after the permission and
// before the key, and leaves nothing behind.
#[tokio::test]
async fn a_start_deadline_outside_the_bounds_is_a_422_with_its_own_code_and_leaves_nothing() {
    let lab = Lab::new().await;
    lab.user("viewer", &["viewer"]).await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let outside = [
        utc(&lab, now + 10).await,
        utc(&lab, now + 86_400 + 3600).await,
        utc(&lab, now).await,
        utc(&lab, now - 3600).await,
        "0001-01-01T00:00:00Z".to_owned(),
        "9999-12-31T23:59:59Z".to_owned(),
    ];
    // F14: no permission is a 403 whatever the deadline.
    let viewer = lab.login("viewer").await;
    for text in &outside {
        let body = lab.body(json!({ "vin": VIN, "startDeadline": text }));
        assert_problem(&lab.post(&viewer, "k", &body).await, 403, text);
    }
    for text in &outside {
        let body = lab.body(json!({ "vin": VIN, "startDeadline": text }));
        let r = lab.post(&cookie, "k", &body).await;
        assert_problem(&r, 422, text);
        assert_eq!(json(&r)["code"], "START_DEADLINE_OUT_OF_BOUNDS", "{text}");
        assert!(!r.body.contains(VIN));
    }
    lab.assert_jobs(0, "refusals").await;
    lab.assert_audits(0, "no audit row").await;
    assert_eq!(lab.w.sql("SELECT count(*) FROM vehicle_locks").await, "0");
    // The key is unbound: the same key creates the job with a good deadline.
    let good = lab.body(json!({ "vin": VIN, "startDeadline": utc(&lab, now + 3600).await }));
    job_id(&lab.post(&cookie, "k", &good).await);
    // F14: with the instruction key not active, the bounds are judged before the key.
    lab.exec("UPDATE instruction_keys SET retired_at = now()")
        .await;
    let bad = lab.body(json!({ "startDeadline": outside[0] }));
    let r = lab.post(&cookie, "k2", &bad).await;
    assert_problem(&r, 422, "bounds before the key");
    assert_eq!(json(&r)["code"], "START_DEADLINE_OUT_OF_BOUNDS");
    let ok = lab.body(json!({ "startDeadline": utc(&lab, now + 3600).await }));
    assert_problem(&lab.post(&cookie, "k2", &ok).await, 503, "the key");
    lab.finish().await;
}

// F5: the shape rules answer 400 and bind nothing (the rules themselves run without a database,
// in `src/jobs.rs`).
#[tokio::test]
async fn a_malformed_start_deadline_is_a_400_and_creates_nothing() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    for bad in [
        json!(5),
        json!(""),
        json!("2030-01-01"),
        json!("2030-01-01T00:00:00"),
        json!("2030-01-01t00:00:00Z"),
        json!("2030-02-30T00:00:00Z"),
        json!("2030-01-01T00:00:60Z"),
    ] {
        let r = lab
            .post(&cookie, "k", &lab.body(json!({ "startDeadline": bad })))
            .await;
        assert_problem(&r, 400, &bad.to_string());
    }
    let null = format!(
        r#"{{"kind":"acquire","agentId":"{}","vciId":"v","irDigests":["{DIGEST_A}"],"startDeadline":null}}"#,
        lab.agent_id
    );
    assert_problem(&lab.post(&cookie, "k", &null).await, 400, "null");
    lab.assert_jobs(0, "refusals").await;
    lab.finish().await;
}

// F9, F10: a replay with another deadline, or with the field added or dropped, is the existing
// key conflict, a 422 with no code.
#[tokio::test]
async fn a_replay_with_another_deadline_or_the_field_added_or_dropped_is_a_key_conflict() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let named = lab.body(json!({ "startDeadline": utc(&lab, now + 3600).await }));
    let id = job_id(&lab.post(&cookie, "k", &named).await);
    let later = lab.body(json!({ "startDeadline": utc(&lab, now + 3601).await }));
    let r = lab.post(&cookie, "k", &later).await;
    assert_problem(&r, 422, "F9");
    assert!(json(&r).get("code").is_none(), "{}", r.body);
    assert!(!r.body.contains(&id));
    // F10: absent, then the default's own second named.
    let absent = lab.body(json!({}));
    let id2 = job_id(&lab.post(&cookie, "k2", &absent).await);
    let default = lab.instruction(&id2).await.issued_at + 900;
    let explicit = lab.body(json!({ "startDeadline": utc(&lab, default as i64).await }));
    assert_problem(&lab.post(&cookie, "k2", &explicit).await, 422, "F10");
    // And the reverse: named, then absent.
    assert_problem(&lab.post(&cookie, "k", &absent).await, 422, "F10 reverse");
    lab.assert_jobs(2, "conflicts").await;
    lab.finish().await;
}

// F8: a replay is answered before the bounds are judged, so it still works when its deadline has
// come too close to the replay's own time.
#[tokio::test]
async fn a_replay_with_the_same_deadline_is_not_judged_against_the_bounds_again() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let body = lab.body(json!({ "startDeadline": utc(&lab, now + 62).await }));
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    // Past this the deadline is under a minute from the replay's time.
    let wait = Instant::now() + Duration::from_secs(15);
    while db_seconds(&lab).await < now + 4 {
        assert!(Instant::now() < wait, "the clock did not move");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(job_id(&lab.post(&cookie, "k", &body).await), id);
    // A new key with the same body is judged, and refused.
    let r = lab.post(&cookie, "k-new", &body).await;
    assert_problem(&r, 422, "a new request");
    assert_eq!(json(&r)["code"], "START_DEADLINE_OUT_OF_BOUNDS");
    lab.finish().await;
}

// F12
#[tokio::test]
async fn two_requests_at_once_for_one_key_replay_on_the_same_deadline_and_conflict_on_another() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let one = lab.body(json!({ "startDeadline": utc(&lab, now + 3600).await }));
    let two = lab.body(json!({ "startDeadline": utc(&lab, now + 7200).await }));
    let (a, b) = tokio::join!(
        lab.post(&cookie, "same", &one),
        lab.post(&cookie, "same", &one)
    );
    assert_eq!(job_id(&a), job_id(&b));
    let (c, d) = tokio::join!(
        lab.post(&cookie, "other", &one),
        lab.post(&cookie, "other", &two)
    );
    let mut statuses = [c.status, d.status];
    statuses.sort();
    assert_eq!(statuses, [202, 422], "{} {}", c.body, d.body);
    lab.assert_jobs(2, "F12").await;
    lab.assert_one_audit_per_job("F12").await;
    lab.finish().await;
}

// R9, E7, F15, E6: an expired job frees its vehicle for the next creation, its replay shows the
// state with no failure, and the end wrote no audit row.
#[tokio::test]
async fn an_expired_job_frees_its_vehicle_and_its_replay_shows_expired_with_no_failure() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let now = db_seconds(&lab).await;
    let body = lab.body(json!({ "vin": VIN, "startDeadline": utc(&lab, now + 3600).await }));
    let id = job_id(&lab.post(&cookie, "k1", &body).await);
    let next = lab.body(json!({ "vin": VIN }));
    // F15: the vehicle is locked for the job's whole window.
    let held = lab.post(&cookie, "k2", &next).await;
    assert_problem(&held, 409, "F15");
    assert_eq!(json(&held)["code"], "VEHICLE_LOCKED");
    lab.exec("UPDATE jobs SET start_deadline = now() - interval '1 second'")
        .await;
    assert_eq!(server::job_expiry::expire(&lab.w.pool).await.unwrap(), 1);
    assert_eq!(lab.locks().await, "");
    // E7: the replay says expired and has no failure member.
    let replay = lab.post(&cookie, "k1", &body).await;
    assert_eq!(replay.status, 202, "{}", replay.head);
    assert_eq!(json(&replay)["jobId"], id);
    assert_eq!(json(&replay)["state"], "expired");
    assert!(json(&replay).get("failure").is_none(), "{}", replay.body);
    // R9: the next job for the vehicle is accepted and takes the lock.
    let second = job_id(&lab.post(&cookie, "k2", &next).await);
    assert_eq!(lab.locks().await, format!("{VIN}>{second}"));
    lab.assert_one_audit_per_job("E6").await;
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// What a job names must be stored (ADR-501)

/// The answer is a 422 with `code`, and no job, audit row or vehicle lock was written.
async fn assert_not_stored(lab: &Lab, r: &Reply, code: &str, what: &str) {
    assert_problem(r, 422, what);
    assert_eq!(json(r)["code"], code, "{what}");
    for digest in [DIGEST_A, DIGEST_B, DIGEST_C] {
        assert!(
            !r.body.contains(digest),
            "{what}: the problem names no digest"
        );
    }
    lab.assert_jobs(0, what).await;
    lab.assert_audits(0, what).await;
    assert_eq!(
        lab.w.sql("SELECT count(*) FROM vehicle_locks").await,
        "0",
        "{what}"
    );
}

#[tokio::test]
async fn a_job_naming_a_document_or_an_artifact_its_tenant_does_not_hold_is_a_422_with_its_code() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let (_, _, tenant_2) = lab.second_tenant().await;
    // DIGEST_C is a document and an artifact of the second tenant only: the check is per tenant.
    hold(&lab.w.pool, &tenant_2, &[DIGEST_C], &[DIGEST_C]).await;
    for (what, extra, code) in [
        (
            "one of two documents",
            json!({ "vin": VIN, "irDigests": [DIGEST_A, DIGEST_C] }),
            "DOCUMENT_NOT_STORED",
        ),
        (
            "the only document",
            json!({ "vin": VIN, "irDigests": [DIGEST_C], "artifactDigest": DIGEST_B }),
            "DOCUMENT_NOT_STORED",
        ),
        (
            "the artifact",
            json!({ "vin": VIN, "artifactDigest": DIGEST_C }),
            "ARTIFACT_NOT_STORED",
        ),
        (
            // A digest stored as a document is not an artifact: the tables are apart.
            "a document named as the artifact",
            json!({ "vin": VIN, "artifactDigest": DIGEST_A }),
            "ARTIFACT_NOT_STORED",
        ),
        (
            // The documents are judged first.
            "both",
            json!({ "vin": VIN, "irDigests": [DIGEST_C], "artifactDigest": DIGEST_C }),
            "DOCUMENT_NOT_STORED",
        ),
    ] {
        let r = lab.post(&cookie, "k", &lab.body(extra)).await;
        assert_not_stored(&lab, &r, code, what).await;
    }
    // The key is unbound: the same key creates the job once what it names is stored.
    let good = lab.body(json!({
        "vin": VIN,
        "irDigests": [DIGEST_B, DIGEST_A],
        "artifactDigest": DIGEST_B,
    }));
    job_id(&lab.post(&cookie, "k", &good).await);
    lab.assert_one_audit_per_job("stored").await;
    lab.finish().await;
}

#[tokio::test]
async fn the_store_check_answers_after_the_permission_and_the_deadline_and_before_the_key() {
    let lab = Lab::new().await;
    lab.user("viewer", &["viewer"]).await;
    let cookie = lab.login("alice").await;
    let viewer = lab.login("viewer").await;
    let unstored = json!({ "irDigests": [DIGEST_C] });
    // No permission is a 403 whatever the job names.
    assert_problem(
        &lab.post(&viewer, "k", &lab.body(unstored.clone())).await,
        403,
        "permission",
    );
    // A deadline out of its bounds is answered before the store.
    let now = db_seconds(&lab).await;
    let late = json!({ "irDigests": [DIGEST_C], "startDeadline": utc(&lab, now + 10).await });
    let r = lab.post(&cookie, "k", &lab.body(late)).await;
    assert_problem(&r, 422, "deadline");
    assert_eq!(json(&r)["code"], "START_DEADLINE_OUT_OF_BOUNDS");
    // An instruction key that is not the active one is judged after the store.
    lab.exec("UPDATE instruction_keys SET retired_at = now()")
        .await;
    let r = lab.post(&cookie, "k", &lab.body(unstored)).await;
    assert_not_stored(&lab, &r, "DOCUMENT_NOT_STORED", "before the key").await;
    assert_problem(
        &lab.post(&cookie, "k", &lab.body(json!({}))).await,
        503,
        "the key",
    );
    lab.assert_jobs(0, "refusals").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_replay_is_answered_before_the_store_check() {
    // ADR-477 item 5: a replay is of a job created once; what it named is not judged again.
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let body = lab.body(json!({ "artifactDigest": DIGEST_B }));
    let first = lab.post(&cookie, "k", &body).await;
    job_id(&first);
    // Nothing deletes these rows; a test may, as the superuser, to show the order.
    lab.exec("DELETE FROM ir_documents; DELETE FROM artifacts")
        .await;
    let again = lab.post(&cookie, "k", &body).await;
    assert_eq!(normalized(&again), normalized(&first));
    // A new key is a creation, and is judged.
    let r = lab.post(&cookie, "k-2", &body).await;
    assert_problem(&r, 422, "new key");
    assert_eq!(json(&r)["code"], "DOCUMENT_NOT_STORED");
    lab.assert_jobs(1, "one job").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_document_stored_through_the_route_can_be_named_by_a_job() {
    let lab = Lab::new().await;
    let cookie = lab.login("alice").await;
    let bytes = crate::documents::doc("job");
    let digest = crate::documents::name_of(&bytes);
    let body = lab.body(json!({ "irDigests": [digest] }));
    let r = lab.post(&cookie, "k", &body).await;
    assert_problem(&r, 422, "before the store");
    assert_eq!(json(&r)["code"], "DOCUMENT_NOT_STORED");
    let stored = crate::documents::put_document(lab.addr(), &cookie, &bytes).await;
    assert_eq!(stored.status, 201, "{} {}", stored.head, stored.body);
    let id = job_id(&lab.post(&cookie, "k", &body).await);
    assert_eq!(
        lab.w
            .sql(&format!(
                "SELECT array_to_string(ir_digests, ',') FROM jobs WHERE id = '{id}'"
            ))
            .await,
        digest
    );
    lab.finish().await;
}

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
use shared_crypto::instruction::{Envelope, verify};
use shared_crypto::{SigningKey, random_32};
use shared_proto::{INSTRUCTION_VERSION, JobInstruction, JobKind};
use sqlx::Postgres;

use crate::control::{open, quiet_for};
use crate::job_queue::{DELIVERED, LONG, ack, command, row, settings, until_row};
use crate::redeem::{Agent, Flaky, World, capture, instruction_key, json};
use crate::serving::{Reply, Running, exchange};
use crate::session::{audience_of, get_challenge, proof, send_session, session_body};

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const VIN: &str = "1HGCM82633A004352";
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
/// registered agent, and a server on it.
struct Lab {
    w: World,
    agent: Agent,
    agent_id: String,
    s: Running,
    hash: String,
    more: Vec<Running>,
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
        let server = Server::bind(&http(), w.pg.pool(), w.key.clone())
            .await
            .unwrap();
        let server = match queue {
            Some(settings) => server.with_job_queue(settings),
            None => server,
        };
        Lab {
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
        let server = Server::bind(&http(), self.w.pg.pool(), self.w.key.clone())
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

/// Waits until a statement that inserts a job waits for a lock, as the database reports it.
async fn wait_for_insert_lock(lab: &Lab) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while lab
        .w
        .sql(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE 'INSERT INTO jobs%'",
        )
        .await
        == "0"
    {
        assert!(
            Instant::now() < deadline,
            "no insert waits for the key's lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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

    // L5: a body over the limit without a session is a 401: the layer answers before the body.
    let big = post(
        vec![with_origin.clone(), json_type.clone(), key.clone()],
        &"x".repeat(20_000),
    )
    .await;
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

    // A body that grows past the limit without declaring a length is a 413 when it does.
    let chunk = "x".repeat(8193);
    let chunked = format!(
        "POST /api/v1/jobs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{}\r\n\
         Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        addr.port(),
        job_headers(addr, &cookie, "chunked").join("\r\n"),
        chunk.len(),
        chunk.len()
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
    assert_eq!(lab.w.sql("SELECT count(*) FROM audit_log").await, "0");
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
    assert_eq!(lab.instruction(&id).await.kind, JobKind::WriteSettings);
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
    assert_eq!(lab.field(&id, "encode(signed_job, 'hex')").await, signed);

    // K2: another body under the key is a 422 with no data of the job.
    let other = lab
        .post(&cookie, "k", &lab.body(json!({"vciId": "vci-2"})))
        .await;
    assert_problem(&other, 422, "K2");
    assert!(!other.body.contains(&id));
    lab.assert_jobs(1, "K2").await;

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
        lab.w
            .sql("SELECT string_agg(row_to_json(j)::text, ',') FROM jobs j")
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

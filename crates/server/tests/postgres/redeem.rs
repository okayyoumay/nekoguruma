//! `POST /api/v1/agents/register` over real TCP (ADR-463, ADR-465 "Redemption design"): the
//! rules that need no database run over a pool that points at nothing; the rest run against a
//! throwaway PostgreSQL. Keys and codes are made at run time.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use server::config::Http;
use server::registration::{self, IssueRequest};
use server::{InstructionKey, Server, db};
use shared_crypto::registration::{
    Mode, Preset, RegistrationCode, RegistrationTranscript, ResponseTranscript, sign_registration,
    verify_response,
};
use shared_crypto::{PublicKey, SignError, SignErrorKind, Signature, Signer, SigningKey};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use tracing_subscriber::fmt::MakeWriter;

use crate::serving::{Reply, Running, exchange};
use crate::support::{TestPostgres, free_port};

pub(crate) const LIMIT: Duration = Duration::from_secs(30);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// Requests

fn post(host: &str, content_type: Option<&str>, body: &str) -> String {
    let mut raw =
        format!("POST /api/v1/agents/register HTTP/1.1\r\n{host}\r\nConnection: close\r\n");
    if let Some(content_type) = content_type {
        raw.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw
}

async fn send(s: &Running, body: &str) -> Reply {
    exchange(s.addr, &post(&s.host(), Some("application/json"), body)).await
}

/// The reply without its `Date` header, for comparing two replies byte for byte.
pub(crate) fn bytes_of(r: &Reply) -> String {
    let head: Vec<&str> = r
        .head
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
        .collect();
    format!("{}\n\n{}", head.join("\n"), r.body)
}

pub(crate) struct Agent {
    pub(crate) key: SigningKey,
    pub(crate) public: PublicKey,
}

impl Agent {
    pub(crate) fn new() -> Agent {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        Agent { key, public }
    }

    pub(crate) fn id(&self) -> String {
        self.public.fingerprint_hex()
    }

    /// The registration request: `text` is what the agent types, `code` the code it stands for.
    fn body(
        &self,
        text: &str,
        code: &RegistrationCode,
        mode: Mode,
        server_key: &PublicKey,
    ) -> String {
        let proof = self.proof(code, mode, server_key);
        self.body_with(text, mode.as_str(), &proof)
    }

    fn proof(&self, code: &RegistrationCode, mode: Mode, server_key: &PublicKey) -> String {
        let transcript =
            RegistrationTranscript::new(code, &self.public, mode, server_key.fingerprint());
        STANDARD.encode(
            sign_registration(&self.key, &transcript)
                .unwrap()
                .to_bytes(),
        )
    }

    fn body_with(&self, code: &str, mode: &str, proof: &str) -> String {
        serde_json::json!({
            "code": code,
            "publicKey": self.public.to_spki_pem(),
            "mode": mode,
            "proof": proof,
        })
        .to_string()
    }
}

fn plain(code: &RegistrationCode) -> &str {
    code.as_str()
}

pub(crate) fn json(r: &Reply) -> serde_json::Value {
    serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.body))
}

// ---------------------------------------------------------------------------------------------
// Log capture

#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// How many lines name this refusal reason.
    fn reasons(&self, word: &str) -> usize {
        self.text()
            .lines()
            .filter(|l| {
                l.contains("registration refused") && l.contains(&format!("reason=\"{word}\""))
            })
            .count()
    }
}

/// Logs of the current thread from now on. The tests that read the log run on the current-thread
/// runtime, so the server's tasks log on this thread too.
pub(crate) fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    crate::support::pin_dispatchers();
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}

// ---------------------------------------------------------------------------------------------
// Servers

pub(crate) fn instruction_key() -> Arc<InstructionKey> {
    Arc::new(InstructionKey::from_signer(Arc::new(
        SigningKey::generate().unwrap(),
    )))
}

async fn bind(pool: PgPool, key: Arc<InstructionKey>) -> Running {
    let http = Http {
        port: 0,
        shutdown_grace_secs: 2,
    };
    Running::spawn(Server::bind(&http, pool, key).await.unwrap())
}

/// A server whose pool points at a port nothing listens on.
pub(crate) async fn dead(key: Arc<InstructionKey>) -> Running {
    let options = PgConnectOptions::new_without_pgpass()
        .host("127.0.0.1")
        .port(free_port())
        .ssl_mode(PgSslMode::Disable);
    bind(db::pool_lazy(options, Duration::from_secs(1)), key).await
}

/// A migrated database with the server key's row, and a server on it.
pub(crate) struct World {
    pub(crate) pg: TestPostgres,
    pub(crate) pool: PgPool,
    pub(crate) key: Arc<InstructionKey>,
    pub(crate) server: Running,
}

impl World {
    pub(crate) async fn new() -> World {
        World::with_key(instruction_key()).await
    }

    async fn with_key(key: Arc<InstructionKey>) -> World {
        let pg = TestPostgres::start();
        let pool = pg.pool();
        db::migrate_with(&pool, &db::MIGRATOR, LIMIT).await.unwrap();
        let world = World {
            server: bind(pool.clone(), key.clone()).await,
            pg,
            pool,
            key,
        };
        world.add_key(&world.key).await;
        world
    }

    async fn add_key(&self, key: &InstructionKey) {
        sqlx::query("INSERT INTO instruction_keys (key_id, public_key) VALUES ($1, $2)")
            .bind(key.key_id())
            .bind(key.public_key().to_spki_der())
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// A new tenant with an enabled operator of this subject; the tenant's and operator's IDs.
    pub(crate) async fn add_operator(&self, subject: &str) -> (String, String) {
        sqlx::query_as(
            "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'tenant') \
                        RETURNING id), \
                  o AS (INSERT INTO operators (id, tenant_id, subject, display_name) \
                        SELECT gen_random_uuid(), id, $1, 'operator' FROM t \
                        RETURNING id, tenant_id) \
             SELECT tenant_id::text, id::text FROM o",
        )
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn issue(&self, owner: &str, agent: &Agent) -> RegistrationCode {
        let request = IssueRequest::new(owner, "Bench PC", &agent.id(), None).unwrap();
        registration::issue(&self.pool, &request)
            .await
            .unwrap()
            .code
    }

    pub(crate) async fn redeem(&self, owner: &str, agent: &Agent) -> (RegistrationCode, Reply) {
        let code = self.issue(owner, agent).await;
        let body = agent.body(plain(&code), &code, Mode::User, &self.key.public_key());
        let reply = send(&self.server, &body).await;
        (code, reply)
    }

    pub(crate) async fn sql(&self, sql: &str) -> String {
        self.pg.scalar(sql).await
    }

    pub(crate) async fn finish(self) {
        self.server.finish().await;
    }
}

fn code_where(code: &RegistrationCode) -> String {
    format!("code_sha256 = decode('{}', 'hex')", hex(&code.sha256()))
}

// ---------------------------------------------------------------------------------------------
// Without a database

#[tokio::test]
async fn each_shape_rule_is_a_400_even_with_the_database_down() {
    let key = instruction_key();
    let s = dead(key.clone()).await;
    let agent = Agent::new();
    let code = RegistrationCode::generate().unwrap();
    let proof = agent.proof(&code, Mode::User, &key.public_key());
    let good = agent.body_with(plain(&code), "user", &proof);

    let mut cases: Vec<(&str, Reply)> = Vec::new();
    let wrong_type = [
        ("no content type", None),
        ("text/plain", Some("text/plain")),
        ("json prefix", Some("application/jsonx")),
    ];
    for (name, content_type) in wrong_type {
        let r = exchange(s.addr, &post(&s.host(), content_type, &good)).await;
        cases.push((name, r));
    }
    let other = RegistrationCode::generate().unwrap();
    let bodies = [
        ("not json", "nope".to_owned()),
        ("array", "[]".to_owned()),
        ("empty object", "{}".to_owned()),
        ("short code", agent.body_with("ngr1-0", "user", &proof)),
        ("other prefix", agent.body_with(&plain(&other).replace("ngr1", "ngr2"), "user", &proof)),
        ("mode", agent.body_with(plain(&code), "machine", &proof)),
        ("proof not base64", agent.body_with(plain(&code), "user", "!!")),
        ("proof short", agent.body_with(plain(&code), "user", &STANDARD.encode([1u8; 63]))),
        ("proof unpadded", agent.body_with(plain(&code), "user", proof.trim_end_matches('='))),
        (
            "key not a pem",
            serde_json::json!({"code": plain(&code), "publicKey": "x", "mode": "user", "proof": proof})
                .to_string(),
        ),
    ];
    for (name, body) in &bodies {
        cases.push((name, send(&s, body).await));
    }
    for (name, r) in &cases {
        assert_eq!(r.status, 400, "{name}: {}", r.head);
        assert_eq!(
            r.header("content-type"),
            Some("application/problem+json"),
            "{name}"
        );
        assert!(
            !r.body.contains(plain(&code)),
            "{name}: the code is not echoed"
        );
    }
    s.finish().await;
}

#[tokio::test]
async fn other_methods_get_405_with_allow() {
    let s = dead(instruction_key()).await;
    for method in ["GET", "PUT", "DELETE", "PATCH", "HEAD"] {
        let raw = format!(
            "{method} /api/v1/agents/register HTTP/1.1\r\n{}\r\nConnection: close\r\n\
             Content-Length: 0\r\n\r\n",
            s.host()
        );
        let r = exchange(s.addr, &raw).await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
        assert_eq!(r.header("allow"), Some("POST"), "{method}");
    }
    s.finish().await;
}

#[tokio::test]
async fn a_body_of_4096_bytes_is_read_and_one_more_is_413() {
    let s = dead(instruction_key()).await;
    let exact = send(&s, &"x".repeat(4096)).await;
    assert_eq!(exact.status, 400, "{}", exact.head);
    let over = send(&s, &"x".repeat(4097)).await;
    assert_eq!(over.status, 413, "{}", over.head);
    assert_eq!(
        over.header("content-type"),
        Some("application/problem+json")
    );
    s.finish().await;
}

#[tokio::test]
async fn a_body_that_does_not_arrive_within_five_seconds_is_408() {
    let s = dead(instruction_key()).await;
    // The head promises ten bytes and none follow.
    let raw = format!(
        "POST /api/v1/agents/register HTTP/1.1\r\n{}\r\nContent-Type: application/json\r\n\
         Content-Length: 10\r\n\r\n",
        s.host()
    );
    let started = std::time::Instant::now();
    let r = exchange(s.addr, &raw).await;
    assert_eq!(r.status, 408, "{}", r.head);
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    s.finish().await;
}

#[tokio::test]
async fn eleven_bad_proofs_give_ten_identical_403s_then_429_and_only_refusals_count() {
    let key = instruction_key();
    let s = dead(key.clone()).await;
    let agent = Agent::new();
    let other_server = SigningKey::generate().unwrap().public_key();
    let bad = |code: &RegistrationCode| {
        // A proof made for another server key ID.
        agent.body(plain(code), code, Mode::User, &other_server)
    };

    let mut first: Option<String> = None;
    for i in 0..10 {
        let code = RegistrationCode::generate().unwrap();
        let r = send(&s, &bad(&code)).await;
        assert_eq!(r.status, 403, "{i}: {}", r.head);
        assert_eq!(r.header("content-type"), Some("application/problem+json"));
        let bytes = bytes_of(&r);
        assert_eq!(*first.get_or_insert(bytes.clone()), bytes, "byte-identical");
        assert!(!r.body.contains(plain(&code)));
    }
    let code = RegistrationCode::generate().unwrap();
    let limited = send(&s, &bad(&code)).await;
    assert_eq!(limited.status, 429, "{}", limited.head);
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap();
    assert!((1..=60).contains(&wait), "{wait}");

    // Shape errors, the size limit and the method are answered as before while limited.
    assert_eq!(send(&s, "nope").await.status, 400);
    assert_eq!(send(&s, &"x".repeat(4097)).await.status, 413);
    let get = format!(
        "GET /api/v1/agents/register HTTP/1.1\r\n{}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        s.host()
    );
    assert_eq!(exchange(s.addr, &get).await.status, 405);
    // A limited answer is not counted: the next refusal is limited still.
    assert_eq!(send(&s, &bad(&code)).await.status, 429);
    s.finish().await;
}

#[tokio::test]
async fn a_valid_proof_against_a_dead_database_is_503_and_never_counted() {
    let key = instruction_key();
    let s = dead(key.clone()).await;
    let agent = Agent::new();
    for _ in 0..12 {
        let code = RegistrationCode::generate().unwrap();
        let body = agent.body(plain(&code), &code, Mode::User, &key.public_key());
        let r = send(&s, &body).await;
        assert_eq!(r.status, 503, "{}", r.head);
        assert_eq!(r.header("content-type"), Some("application/problem+json"));
        assert!(!r.body.contains("127.0.0.1"), "{}", r.body);
    }
    // Nothing was counted, so the first bad proof is still a 403.
    let code = RegistrationCode::generate().unwrap();
    let other_server = SigningKey::generate().unwrap().public_key();
    let r = send(
        &s,
        &agent.body(plain(&code), &code, Mode::User, &other_server),
    )
    .await;
    assert_eq!(r.status, 403);
    s.finish().await;
}

// ---------------------------------------------------------------------------------------------
// With a database

#[tokio::test]
async fn a_valid_registration_writes_one_transaction_and_signs_what_it_assigned() {
    let (logs, _guard) = capture();
    let w = World::new().await;
    let (tenant, owner) = w.add_operator("alice").await;
    let agent = Agent::new();
    let (code, r) = w.redeem("alice", &agent).await;

    assert_eq!(r.status, 201, "{} {}", r.head, r.body);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let body = json(&r);
    let agent_id = body["agentId"].as_str().unwrap().to_owned();
    assert_eq!(body["ownerId"], owner.as_str());
    assert_eq!(body["preset"], "personal");
    assert_eq!(body["allowRemote"], false);
    let server_pem = body["instructionKey"].as_str().unwrap();
    let server_key = PublicKey::from_spki_pem(server_pem).unwrap();
    assert_eq!(
        server_key.fingerprint_hex(),
        body["instructionKeyId"].as_str().unwrap()
    );
    assert_eq!(server_key.fingerprint_hex(), w.key.key_id());
    let signature = STANDARD
        .decode(body["signature"].as_str().unwrap())
        .unwrap();
    let transcript = ResponseTranscript::new(
        &code,
        &agent.public,
        &agent_id,
        &owner,
        Preset::Personal,
        false,
    )
    .unwrap();
    verify_response(
        &server_key,
        &transcript,
        &Signature::from_bytes(&signature).unwrap(),
    )
    .unwrap();
    // The signature covers the values it was made over and no others.
    let other = ResponseTranscript::new(
        &code,
        &agent.public,
        &agent_id,
        &owner,
        Preset::Shared,
        false,
    )
    .unwrap();
    assert!(
        verify_response(
            &server_key,
            &other,
            &Signature::from_bytes(&signature).unwrap()
        )
        .is_err()
    );

    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "1");
    assert_eq!(w.sql("SELECT id::text FROM agents").await, agent_id);
    assert_eq!(
        w.sql(
            "SELECT mode || '|' || preset || '|' || allow_remote || '|' || display_name FROM agents"
        )
        .await,
        "user|personal|false|Bench PC"
    );
    assert_eq!(
        w.sql("SELECT encode(public_key, 'hex') FROM agents").await,
        hex(&agent.public.to_spki_der())
    );
    assert_eq!(w.sql("SELECT tenant_id::text FROM agents").await, tenant);
    assert_eq!(w.sql("SELECT owner_id::text FROM agents").await, owner);
    assert_eq!(
        w.sql(
            "SELECT (registered_by IS NULL AND machine_hint IS NULL AND os_user_hint IS NULL \
               AND revoked_at IS NULL) FROM agents"
        )
        .await,
        "true"
    );
    assert_eq!(
        w.sql(
            "SELECT tenant_id::text || agent_id::text || operator_id::text \
               FROM agent_allowed_operators"
        )
        .await,
        format!("{tenant}{agent_id}{owner}")
    );
    let used = format!(
        "SELECT (used_at IS NOT NULL AND agent_id::text = '{agent_id}') \
         FROM agent_registration_codes WHERE {}",
        code_where(&code)
    );
    assert_eq!(w.sql(&used).await, "true");
    assert_eq!(w.sql("SELECT count(*) FROM audit_log").await, "0");

    let text = logs.text();
    assert!(text.contains("agent registered"), "{text}");
    for (i, secret) in [plain(&code), "Bench PC", owner.as_str(), "alice"]
        .into_iter()
        .enumerate()
    {
        // The message names the value by its position only, so a failure prints no secret.
        assert!(!text.contains(secret), "the log holds forbidden value #{i}");
    }
    w.finish().await;
}

#[tokio::test]
async fn a_typed_variant_of_the_code_redeems() {
    let w = World::new().await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let digits = &plain(&code)[5..];
    let typed = format!(
        "  NGR1-{}\r\n",
        digits.to_lowercase().replace('1', "l").replace('0', "o")
    );
    let body = agent.body(&typed, &code, Mode::Device, &w.key.public_key());
    let r = send(&w.server, &body).await;
    assert_eq!(r.status, 201, "{} {}", r.head, r.body);
    assert_eq!(w.sql("SELECT mode FROM agents").await, "device");
    w.finish().await;
}

#[tokio::test]
async fn every_refusal_is_the_same_403_and_its_reason_is_logged() {
    let (logs, _guard) = capture();
    let w = World::new().await;
    w.add_operator("alice").await;
    let (_, bob) = w.add_operator("bob").await;
    w.add_operator("carol").await;
    let server_key = w.key.public_key();
    let mut replies: Vec<Reply> = Vec::new();
    let mut codes: Vec<RegistrationCode> = Vec::new();

    // V1: a proof that covers another mode; refused before the database, the code untouched,
    // and the same code then redeems.
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let wrong_mode = agent.body_with(
        plain(&code),
        "user",
        &agent.proof(&code, Mode::Device, &server_key),
    );
    replies.push(send(&w.server, &wrong_mode).await);
    assert_eq!(logs.reasons("proof"), 1);
    assert_eq!(
        w.sql(&format!(
            "SELECT used_at IS NULL FROM agent_registration_codes WHERE {}",
            code_where(&code)
        ))
        .await,
        "true"
    );
    let ok = send(
        &w.server,
        &agent.body(plain(&code), &code, Mode::User, &server_key),
    )
    .await;
    assert_eq!(ok.status, 201, "{}", ok.body);
    codes.push(code.clone());

    // V3: the same code again is used.
    replies.push(
        send(
            &w.server,
            &agent.body(plain(&code), &code, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("used"), 1);

    // V2: a code that is not stored.
    let stranger = Agent::new();
    let unknown = RegistrationCode::generate().unwrap();
    replies.push(
        send(
            &w.server,
            &stranger.body(plain(&unknown), &unknown, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("absent"), 1);

    // V4: revoked.
    let revoked_agent = Agent::new();
    let revoked = w.issue("alice", &revoked_agent).await;
    sqlx::query("UPDATE agent_registration_codes SET revoked_at = now() WHERE code_sha256 = $1")
        .bind(&revoked.sha256()[..])
        .execute(&w.pool)
        .await
        .unwrap();
    replies.push(
        send(
            &w.server,
            &revoked_agent.body(plain(&revoked), &revoked, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("revoked"), 1);
    codes.push(revoked);

    // V5: expired.
    let late = Agent::new();
    let expired = w.issue("alice", &late).await;
    sqlx::query(
        "UPDATE agent_registration_codes SET created_at = now() - interval '2 hours', \
         expires_at = now() - interval '1 hour' WHERE code_sha256 = $1",
    )
    .bind(&expired.sha256()[..])
    .execute(&w.pool)
    .await
    .unwrap();
    replies.push(
        send(
            &w.server,
            &late.body(plain(&expired), &expired, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("expired"), 1);
    codes.push(expired);

    // V6: bound to another key; the right key still redeems it.
    let bound = Agent::new();
    let intruder = Agent::new();
    let held = w.issue("alice", &bound).await;
    replies.push(
        send(
            &w.server,
            &intruder.body(plain(&held), &held, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("other_key"), 1);
    let ok = send(
        &w.server,
        &bound.body(plain(&held), &held, Mode::User, &server_key),
    )
    .await;
    assert_eq!(ok.status, 201, "{}", ok.body);
    codes.push(held);

    // V8: the owner is disabled since issuance, and an owner that is not in the code's tenant.
    let a = Agent::new();
    let disabled = w.issue("bob", &a).await;
    sqlx::query("UPDATE operators SET disabled_at = now() WHERE id = $1::uuid")
        .bind(&bob)
        .execute(&w.pool)
        .await
        .unwrap();
    replies.push(
        send(
            &w.server,
            &a.body(plain(&disabled), &disabled, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("owner"), 1);
    let b = Agent::new();
    let moved = w.issue("carol", &b).await;
    sqlx::query(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), 'elsewhere') \
                    RETURNING id) \
         UPDATE operators SET tenant_id = (SELECT id FROM t) WHERE subject = 'carol'",
    )
    .execute(&w.pool)
    .await
    .unwrap();
    // The first server's gate for this reason stays shut for a minute, so a second server (its own
    // gates) answers this one and logs it.
    let second = bind(w.pool.clone(), w.key.clone()).await;
    replies.push(
        send(
            &second,
            &b.body(plain(&moved), &moved, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("owner"), 2);
    codes.push(disabled);
    codes.push(moved);

    // V7: the code's instruction key is retired.
    let c = Agent::new();
    let orphan = w.issue("alice", &c).await;
    sqlx::query("UPDATE instruction_keys SET retired_at = now()")
        .execute(&w.pool)
        .await
        .unwrap();
    replies.push(
        send(
            &w.server,
            &c.body(plain(&orphan), &orphan, Mode::User, &server_key),
        )
        .await,
    );
    assert_eq!(logs.reasons("instruction_key"), 1);
    codes.push(orphan);

    assert_eq!(replies.len(), 9);
    for (i, r) in replies.iter().enumerate() {
        assert_eq!(r.status, 403, "{i}: {}", r.head);
        assert_eq!(bytes_of(r), bytes_of(&replies[0]), "{i}");
    }
    let text = logs.text();
    for (i, code) in codes.iter().enumerate() {
        assert!(!text.contains(plain(code)), "the log holds code #{i}");
    }
    for (i, secret) in ["Bench PC", "alice", "bob", "carol"]
        .into_iter()
        .enumerate()
    {
        assert!(!text.contains(secret), "the log holds forbidden value #{i}");
    }
    second.finish().await;
    w.finish().await;
}

#[tokio::test]
async fn a_code_issued_under_a_key_that_was_since_replaced_is_refused() {
    let (logs, _guard) = capture();
    let a = instruction_key();
    let w = World::with_key(a.clone()).await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;

    // A reset: the old row is retired and a new key is active; a server runs with the new key.
    let b = instruction_key();
    sqlx::query("UPDATE instruction_keys SET retired_at = now()")
        .execute(&w.pool)
        .await
        .unwrap();
    w.add_key(&b).await;
    let s = bind(w.pool.clone(), b.clone()).await;
    let r = send(
        &s,
        &agent.body(plain(&code), &code, Mode::User, &b.public_key()),
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(logs.reasons("instruction_key"), 1);
    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "0");
    s.finish().await;
    w.finish().await;
}

#[tokio::test]
async fn a_new_code_for_the_same_key_revokes_the_old_rows_in_every_tenant_and_other_keys_stay() {
    let w = World::new().await;
    let (alice_tenant, _) = w.add_operator("alice").await;
    let (bob_tenant, _) = w.add_operator("bob").await;
    let x = Agent::new();
    let unrevoked = |key: &Agent| {
        format!(
            "SELECT count(*) FROM agents WHERE public_key = decode('{}', 'hex') AND revoked_at IS NULL",
            hex(&key.public.to_spki_der())
        )
    };

    // C3: the same key, a new code.
    let (_, first) = w.redeem("alice", &x).await;
    assert_eq!(first.status, 201, "{}", first.body);
    let (_, second) = w.redeem("alice", &x).await;
    assert_eq!(second.status, 201, "{}", second.body);
    let (first_id, second_id) = (
        json(&first)["agentId"].clone(),
        json(&second)["agentId"].clone(),
    );
    assert_ne!(first_id, second_id);
    assert_eq!(
        w.sql(&format!(
            "SELECT (revoked_at IS NOT NULL) FROM agents WHERE id = '{}'",
            first_id.as_str().unwrap()
        ))
        .await,
        "true"
    );
    assert_eq!(w.sql(&unrevoked(&x)).await, "1");
    assert_eq!(
        w.sql("SELECT count(*) FROM agent_allowed_operators").await,
        "2",
        "the old row stays"
    );

    // C4: the earlier row is in another tenant.
    let (_, third) = w.redeem("bob", &x).await;
    assert_eq!(third.status, 201, "{}", third.body);
    assert_eq!(w.sql(&unrevoked(&x)).await, "1");
    assert_eq!(
        w.sql(&format!(
            "SELECT tenant_id::text FROM agents WHERE public_key = decode('{}', 'hex') \
             AND revoked_at IS NULL",
            hex(&x.public.to_spki_der())
        ))
        .await,
        bob_tenant
    );
    assert_eq!(
        w.sql(&format!(
            "SELECT count(*) FROM agents WHERE tenant_id = '{alice_tenant}' AND revoked_at IS NULL"
        ))
        .await,
        "0"
    );

    // C5: a different key leaves the old row alone.
    let y = Agent::new();
    let (_, other) = w.redeem("alice", &y).await;
    assert_eq!(other.status, 201, "{}", other.body);
    assert_eq!(w.sql(&unrevoked(&x)).await, "1");
    assert_eq!(w.sql(&unrevoked(&y)).await, "1");
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_code_sent_twice_at_once_redeems_exactly_once() {
    // The server's tasks run on other threads, so the log is captured process-wide; only this
    // test's own code is looked for in it.
    let logs = Captured::default();
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    let w = World::new().await;
    w.add_operator("alice").await;
    for round in 0..20 {
        let agent = Agent::new();
        let code = w.issue("alice", &agent).await;
        // The captured request, replayed at the same moment.
        let body = agent.body(plain(&code), &code, Mode::User, &w.key.public_key());
        let (addr, host) = (w.server.addr, w.server.host());
        let raw = post(&host, Some("application/json"), &body);
        let one = tokio::spawn({
            let raw = raw.clone();
            async move { exchange(addr, &raw).await }
        });
        let two = tokio::spawn(async move { exchange(addr, &raw).await });
        let (one, two) = (one.await.unwrap(), two.await.unwrap());
        let statuses = [one.status, two.status];
        // The loser is refused as used: 403, or 429 once ten refusals of the loop are counted.
        assert_eq!(
            statuses.iter().filter(|s| **s == 201).count(),
            1,
            "{round}: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|s| [201, 403, 429].contains(s)),
            "{round}: {statuses:?}"
        );
        assert_eq!(
            w.sql(&format!(
                "SELECT count(*) FROM agents WHERE public_key = decode('{}', 'hex')",
                hex(&agent.public.to_spki_der())
            ))
            .await,
            "1",
            "{round}"
        );
        // The code row names the winner's agent.
        let winner = if one.status == 201 { &one } else { &two };
        let loser = if one.status == 201 { &two } else { &one };
        assert_eq!(
            w.sql(&format!(
                "SELECT agent_id::text FROM agent_registration_codes WHERE {}",
                code_where(&code)
            ))
            .await,
            json(winner)["agentId"].as_str().unwrap(),
            "{round}"
        );
        if round == 0 {
            // The first refusal of the router is logged: as used, with this code's redacted form.
            assert_eq!(loser.status, 403, "{}", loser.head);
            let line = logs
                .text()
                .lines()
                .find(|l| l.contains(&format!("{code:?}")))
                .map(str::to_owned);
            let line = line.expect("the refusal is logged with the code's redacted form");
            assert!(line.contains("reason=\"used\""), "{line}");
        }
    }
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_codes_for_one_key_at_once_both_redeem_and_one_row_stays_unrevoked() {
    let w = World::new().await;
    w.add_operator("alice").await;
    w.add_operator("bob").await;
    for round in 0..20 {
        let agent = Agent::new();
        let first = w.issue("alice", &agent).await;
        let second = w.issue("bob", &agent).await;
        let addr = w.server.addr;
        let host = w.server.host();
        let server_key = w.key.public_key();
        let go = |code: RegistrationCode| {
            let raw = post(
                &host,
                Some("application/json"),
                &agent.body(plain(&code), &code, Mode::User, &server_key),
            );
            tokio::spawn(async move { exchange(addr, &raw).await })
        };
        let (a, b) = (go(first), go(second));
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(
            (a.status, b.status),
            (201, 201),
            "{round}: {} {}",
            a.body,
            b.body
        );
        let key = hex(&agent.public.to_spki_der());
        assert_eq!(
            w.sql(&format!(
                "SELECT count(*) FROM agents WHERE public_key = decode('{key}', 'hex')"
            ))
            .await,
            "2",
            "{round}"
        );
        assert_eq!(
            w.sql(&format!("SELECT count(*) FROM agents WHERE public_key = decode('{key}', 'hex') AND revoked_at IS NULL")).await,
            "1",
            "{round}"
        );
    }
    w.finish().await;
}

#[tokio::test]
async fn a_statement_failure_rolls_everything_back_and_the_same_code_redeems_after() {
    let (logs, _guard) = capture();
    let w = World::new().await;
    let (_, owner) = w.add_operator("alice").await;
    let agent = Agent::new();
    // An earlier row of the key: the revoke must be rolled back with the rest.
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, display_name, mode, public_key) \
         SELECT gen_random_uuid(), tenant_id, 'earlier', 'user', $1 FROM operators",
    )
    .bind(agent.public.to_spki_der())
    .execute(&w.pool)
    .await
    .unwrap();
    w.pg.execute(
        "CREATE FUNCTION fail_insert() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'test failure'; END $$",
    )
    .await;
    w.pg.execute(
        "CREATE TRIGGER fail_allowed BEFORE INSERT ON agent_allowed_operators \
         FOR EACH ROW EXECUTE FUNCTION fail_insert()",
    )
    .await;

    let (code, failed) = w.redeem("alice", &agent).await;
    assert_eq!(failed.status, 503, "{} {}", failed.head, failed.body);
    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "1");
    assert_eq!(
        w.sql("SELECT (revoked_at IS NULL) FROM agents").await,
        "true"
    );
    assert_eq!(
        w.sql(&format!(
            "SELECT (used_at IS NULL AND agent_id IS NULL) FROM agent_registration_codes WHERE {}",
            code_where(&code)
        ))
        .await,
        "true"
    );
    let text = logs.text();
    assert!(text.contains("registration unavailable"), "{text}");
    assert!(text.contains("SQLSTATE P0001"), "{text}");
    assert!(
        !text.contains(&owner) && !text.contains(plain(&code)),
        "{text}"
    );

    w.pg.execute("DROP TRIGGER fail_allowed ON agent_allowed_operators")
        .await;
    let retry = send(
        &w.server,
        &agent.body(plain(&code), &code, Mode::User, &w.key.public_key()),
    )
    .await;
    assert_eq!(retry.status, 201, "{}", retry.body);
    assert_eq!(
        w.sql("SELECT count(*) FROM agents WHERE revoked_at IS NULL")
            .await,
        "1"
    );
    w.finish().await;
}

/// A signer that fails while `fail` is set.
struct Flaky {
    key: SigningKey,
    fail: Arc<AtomicBool>,
}

impl Signer for Flaky {
    fn public_key(&self) -> PublicKey {
        self.key.public_key()
    }

    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(SignError::new(SignErrorKind::Unavailable));
        }
        self.key.sign(message)
    }
}

#[tokio::test]
async fn a_signer_failure_is_500_before_the_commit_and_the_code_still_redeems() {
    let (logs, _guard) = capture();
    let fail = Arc::new(AtomicBool::new(true));
    let key = Arc::new(InstructionKey::from_signer(Arc::new(Flaky {
        key: SigningKey::generate().unwrap(),
        fail: fail.clone(),
    })));
    let w = World::with_key(key).await;
    w.add_operator("alice").await;
    let agent = Agent::new();

    let (code, failed) = w.redeem("alice", &agent).await;
    assert_eq!(failed.status, 500, "{} {}", failed.head, failed.body);
    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "0");
    assert_eq!(
        w.sql("SELECT count(*) FROM agent_allowed_operators").await,
        "0"
    );
    assert_eq!(
        w.sql(&format!(
            "SELECT used_at IS NULL FROM agent_registration_codes WHERE {}",
            code_where(&code)
        ))
        .await,
        "true"
    );
    assert!(logs.text().contains("registration failed"));

    fail.store(false, Ordering::SeqCst);
    let retry = send(
        &w.server,
        &agent.body(plain(&code), &code, Mode::User, &w.key.public_key()),
    )
    .await;
    assert_eq!(retry.status, 201, "{}", retry.body);
    w.finish().await;
}

#[tokio::test]
async fn while_limited_an_unknown_code_is_429_and_a_valid_registration_is_201() {
    let w = World::new().await;
    w.add_operator("alice").await;
    let server_key = w.key.public_key();
    let intruder = Agent::new();
    let elsewhere = SigningKey::generate().unwrap().public_key();
    for _ in 0..10 {
        let code = RegistrationCode::generate().unwrap();
        let r = send(
            &w.server,
            &intruder.body(plain(&code), &code, Mode::User, &elsewhere),
        )
        .await;
        assert_eq!(r.status, 403);
    }
    let unknown = RegistrationCode::generate().unwrap();
    let r = send(
        &w.server,
        &intruder.body(plain(&unknown), &unknown, Mode::User, &server_key),
    )
    .await;
    assert_eq!(r.status, 429, "{}", r.head);
    assert!(r.header("retry-after").is_some());

    let agent = Agent::new();
    let (code, ok) = w.redeem("alice", &agent).await;
    assert_eq!(ok.status, 201, "{}", ok.body);
    // The code is now used; its replay is refused, and the refusal is limited.
    let replay = send(
        &w.server,
        &agent.body(plain(&code), &code, Mode::User, &server_key),
    )
    .await;
    assert_eq!(replay.status, 429);
    w.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Proof encodings, the declared size, the guard

/// The same signature with `s` replaced by the group order minus `s`: the high-S twin, which the
/// signer never produces and the verifier refuses.
pub(crate) fn high_s(signature: &[u8]) -> Vec<u8> {
    // The order of the P-256 group, big endian.
    const ORDER: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63,
        0x25, 0x51,
    ];
    let mut out = signature.to_vec();
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = i16::from(ORDER[i]) - i16::from(signature[32 + i]) - borrow;
        borrow = i16::from(d < 0);
        out[32 + i] = u8::try_from(d.rem_euclid(256)).unwrap();
    }
    out
}

#[tokio::test]
async fn a_high_s_proof_is_refused_and_a_non_canonical_base64_proof_is_400() {
    let key = instruction_key();
    let s = dead(key.clone()).await;
    let agent = Agent::new();
    let code = RegistrationCode::generate().unwrap();
    let proof = agent.proof(&code, Mode::User, &key.public_key());
    // Control: the proof as made reaches the (dead) database.
    let ok = send(&s, &agent.body_with(plain(&code), "user", &proof)).await;
    assert_eq!(ok.status, 503, "{}", ok.head);

    let raw = STANDARD.decode(&proof).unwrap();
    let twin = high_s(&raw);
    assert_ne!(twin, raw);
    let r = send(
        &s,
        &agent.body_with(plain(&code), "user", &STANDARD.encode(&twin)),
    )
    .await;
    assert_eq!(r.status, 403, "V1: a high-S proof is a refusal: {}", r.head);

    // The last data character carries four bits that must be zero; set one of them.
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let body = proof.trim_end_matches('=');
    let (head, last) = body.split_at(body.len() - 1);
    let index = alphabet.find(last).unwrap();
    assert_eq!(index & 0x0f, 0, "canonical trailing bits are zero");
    let bent = format!(
        "{head}{}{}",
        &alphabet[index | 1..=index | 1],
        &proof[body.len()..]
    );
    assert!(
        STANDARD.decode(&bent).is_err(),
        "the standard decoder is strict too"
    );
    let r = send(&s, &agent.body_with(plain(&code), "user", &bent)).await;
    assert_eq!(r.status, 400, "H10: {}", r.head);
    s.finish().await;
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_answered_413_before_the_body_is_read() {
    let s = dead(instruction_key()).await;
    let raw = post(&s.host(), Some("application/json"), &"x".repeat(65536));
    let r = exchange(s.addr, &raw).await;
    assert_eq!(r.status, 413, "{}", r.head);
    s.finish().await;
}

#[tokio::test]
async fn a_request_the_guard_redirects_never_reaches_the_code() {
    let w = World::new().await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let body = agent.body(plain(&code), &code, Mode::User, &w.key.public_key());
    let host = format!("Host: localhost:{}", w.server.addr.port());
    let r = exchange(w.server.addr, &post(&host, Some("application/json"), &body)).await;
    assert_eq!(r.status, 307, "{}", r.head);
    assert_eq!(
        w.sql(&format!(
            "SELECT used_at IS NULL FROM agent_registration_codes WHERE {}",
            code_where(&code)
        ))
        .await,
        "true"
    );
    let ok = send(&w.server, &body).await;
    assert_eq!(ok.status, 201, "{}", ok.body);
    w.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Waiting on the code row

/// Waits until some session waits on a lock.
async fn wait_for_lock_wait(w: &World) {
    for _ in 0..200 {
        if w.sql("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'")
            .await
            != "0"
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no session waits on a lock");
}

/// A transaction that has consumed the code and not ended: the state the first of two requests
/// for one code is in.
async fn hold_consumed(
    w: &World,
    code: &RegistrationCode,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = w.pool.begin().await.unwrap();
    let rows =
        sqlx::query("UPDATE agent_registration_codes SET used_at = now() WHERE code_sha256 = $1")
            .bind(&code.sha256()[..])
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
    assert_eq!(rows, 1);
    tx
}

#[tokio::test]
async fn a_request_that_waits_on_the_code_is_refused_as_used_when_the_first_commits() {
    let (logs, _guard) = capture();
    let w = World::new().await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let body = agent.body(plain(&code), &code, Mode::User, &w.key.public_key());
    let holder = hold_consumed(&w, &code).await;

    let (addr, host) = (w.server.addr, w.server.host());
    let waiter =
        tokio::spawn(
            async move { exchange(addr, &post(&host, Some("application/json"), &body)).await },
        );
    wait_for_lock_wait(&w).await;
    holder.commit().await.unwrap();
    let r = waiter.await.unwrap();
    assert_eq!(r.status, 403, "{}", r.head);
    assert_eq!(logs.reasons("used"), 1);
    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "0");
    w.finish().await;
}

#[tokio::test]
async fn a_request_that_waits_on_the_code_redeems_when_the_first_rolls_back() {
    let w = World::new().await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let body = agent.body(plain(&code), &code, Mode::User, &w.key.public_key());
    let holder = hold_consumed(&w, &code).await;

    let (addr, host) = (w.server.addr, w.server.host());
    let waiter =
        tokio::spawn(
            async move { exchange(addr, &post(&host, Some("application/json"), &body)).await },
        );
    wait_for_lock_wait(&w).await;
    holder.rollback().await.unwrap();
    let r = waiter.await.unwrap();
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(w.sql("SELECT count(*) FROM agents").await, "1");
    w.finish().await;
}

#[tokio::test]
async fn a_lock_wait_past_five_seconds_is_503_and_the_code_redeems_after() {
    let (logs, _guard) = capture();
    let w = World::new().await;
    w.add_operator("alice").await;
    let agent = Agent::new();
    let code = w.issue("alice", &agent).await;
    let body = agent.body(plain(&code), &code, Mode::User, &w.key.public_key());
    let holder = hold_consumed(&w, &code).await;

    let started = std::time::Instant::now();
    let r = send(&w.server, &body).await;
    assert_eq!(r.status, 503, "{}", r.head);
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // The lock and statement limits are both 5 s, so either one ends the wait.
    let text = logs.text();
    assert!(
        text.contains("SQLSTATE 55P03") || text.contains("SQLSTATE 57014"),
        "{text}"
    );
    holder.rollback().await.unwrap();

    let ok = send(&w.server, &body).await;
    assert_eq!(ok.status, 201, "{}", ok.body);
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_transaction_is_read_committed_whatever_the_database_default() {
    let w = World::new().await;
    w.add_operator("alice").await;
    w.add_operator("bob").await;
    w.pg.execute("ALTER DATABASE postgres SET default_transaction_isolation = 'repeatable read'")
        .await;
    // New sessions take the setting; a pool of its own has no older ones.
    assert_eq!(
        w.sql("SELECT current_setting('default_transaction_isolation')")
            .await,
        "repeatable read"
    );
    let server = bind(w.pg.pool(), w.key.clone()).await;
    for round in 0..5 {
        let agent = Agent::new();
        let first = w.issue("alice", &agent).await;
        let second = w.issue("bob", &agent).await;
        let (addr, host) = (server.addr, server.host());
        let server_key = w.key.public_key();
        let go = |code: RegistrationCode| {
            let raw = post(
                &host,
                Some("application/json"),
                &agent.body(plain(&code), &code, Mode::User, &server_key),
            );
            tokio::spawn(async move { exchange(addr, &raw).await })
        };
        let (a, b) = (go(first), go(second));
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(
            (a.status, b.status),
            (201, 201),
            "{round}: {} {}",
            a.body,
            b.body
        );
        assert_eq!(
            w.sql(&format!(
                "SELECT count(*) FROM agents WHERE public_key = decode('{}', 'hex') \
                 AND revoked_at IS NULL",
                hex(&agent.public.to_spki_der())
            ))
            .await,
            "1",
            "{round}"
        );
    }
    server.finish().await;
    w.finish().await;
}

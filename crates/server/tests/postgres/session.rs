//! `POST /api/v1/agent/challenge` and `POST /api/v1/agent/session` over real TCP (ADR-466 items 1
//! to 4, ADR-467): the rules that need no database run over a pool that points at nothing; the
//! rest run against a throwaway PostgreSQL. Keys, challenges and nonces are made at run time, and
//! no assertion message prints a secret: values are named by index.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use server::config::Http;
use server::job_queue::Settings;
use server::local_guard::pinned_origin;
use server::{Hub, Server, Sessions};
use shared_crypto::proof::{Audience, ProofTranscript, sign_proof};
use shared_crypto::{SigningKey, random_32};
use shared_proto::{AgentId, NONCE_HEADER};

use crate::redeem::{Agent, Flaky, World, bytes_of, capture, dead, high_s, instruction_key, json};
use crate::serving::{Reply, Running, exchange};
use crate::support::{Answered, assert_unproved};

const CHALLENGE: &str = "/api/v1/agent/challenge";
const SESSION: &str = "/api/v1/agent/session";

// ---------------------------------------------------------------------------------------------
// Requests

fn post(host: &str, path: &str, content_type: Option<&str>, body: &str) -> String {
    post_with(host, path, content_type, &[], body)
}

/// [`post`] with more header lines.
fn post_with(
    host: &str,
    path: &str,
    content_type: Option<&str>,
    headers: &[String],
    body: &str,
) -> String {
    crate::support::post_with(host, path, content_type, headers, body)
}

async fn ask_challenge(s: &Running) -> Reply {
    exchange(s.addr, &post(&s.host(), CHALLENGE, None, "")).await
}

pub(crate) async fn send_session(s: &Running, body: &str) -> Reply {
    exchange(
        s.addr,
        &post(&s.host(), SESSION, Some("application/json"), body),
    )
    .await
}

/// A challenge from the route, as the bytes it stands for.
pub(crate) async fn get_challenge(s: &Running) -> [u8; 32] {
    let r = ask_challenge(s).await;
    assert_eq!(r.status, 200, "{}", r.head);
    let text = json(&r)["challenge"].as_str().unwrap().to_owned();
    STANDARD.decode(text).unwrap().try_into().unwrap()
}

pub(crate) fn session_body(
    id: &str,
    challenge: &[u8; 32],
    nonce: &[u8; 32],
    proof: &[u8],
) -> String {
    serde_json::json!({
        "agentId": id,
        "challenge": STANDARD.encode(challenge),
        "nonce": STANDARD.encode(nonce),
        "proof": STANDARD.encode(proof),
    })
    .to_string()
}

pub(crate) fn audience_of(port: u16) -> Audience {
    Audience::parse_canonical(&pinned_origin(port)).unwrap()
}

/// The audience of a server other than the one on `port`.
fn other_port(port: u16) -> u16 {
    if port < u16::MAX { port + 1 } else { port - 1 }
}

pub(crate) fn proof(
    key: &SigningKey,
    audience: &Audience,
    id: &str,
    challenge: [u8; 32],
    nonce: [u8; 32],
) -> Vec<u8> {
    let transcript = ProofTranscript::new(audience, id, challenge, nonce).unwrap();
    sign_proof(key, &transcript).unwrap().to_bytes().to_vec()
}

/// A body that is well formed and holds a random proof: enough to be shaped, never valid.
fn shaped_body(challenge: &[u8; 32]) -> String {
    let id = "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab";
    session_body(id, challenge, &random_32().unwrap(), &[3u8; 64])
}

fn assert_problem(r: &Reply, status: u16, what: &str) {
    assert_eq!(r.status, status, "{what}: {}", r.head);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
}

fn retry_after(r: &Reply) -> u64 {
    r.header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Without a database

#[tokio::test]
async fn a_challenge_is_32_random_bytes_and_the_body_must_be_empty() {
    let s = dead(instruction_key()).await;
    let first = ask_challenge(&s).await;
    assert_eq!(first.status, 200, "{}", first.head);
    assert_eq!(first.header("content-type"), Some("application/json"));
    assert_eq!(first.header("cache-control"), Some("no-store"));
    let text = json(&first)["challenge"].as_str().unwrap().to_owned();
    assert_eq!(text.len(), 44);
    assert_eq!(STANDARD.decode(&text).unwrap().len(), 32);
    assert_ne!(
        get_challenge(&s).await.to_vec(),
        STANDARD.decode(&text).unwrap()
    );

    // Any byte is a 400, whatever the Content-Type.
    for (i, body) in ["{}", " ", "x"].iter().enumerate() {
        let raw = post(&s.host(), CHALLENGE, Some("application/json"), body);
        assert_problem(&exchange(s.addr, &raw).await, 400, &format!("body {i}"));
    }
    for method in ["GET", "PUT", "DELETE"] {
        let raw = format!(
            "{method} {CHALLENGE} HTTP/1.1\r\n{}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            s.host()
        );
        let r = exchange(s.addr, &raw).await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
        assert_eq!(r.header("allow"), Some("POST"), "{method}");
    }
    let big = post(&s.host(), CHALLENGE, None, &"x".repeat(4097));
    assert_problem(&exchange(s.addr, &big).await, 413, "4097 bytes");
    s.finish().await;
}

#[tokio::test]
async fn a_body_that_stalls_is_408_on_both_routes() {
    let s = dead(instruction_key()).await;
    let stalled = |path: &str| {
        format!(
            "POST {path} HTTP/1.1\r\n{}\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n",
            s.host()
        )
    };
    let started = std::time::Instant::now();
    let (raw_a, raw_b) = (stalled(CHALLENGE), stalled(SESSION));
    let (a, b) = tokio::join!(exchange(s.addr, &raw_a), exchange(s.addr, &raw_b));
    assert_eq!(a.status, 408, "{}", a.head);
    assert_eq!(b.status, 408, "{}", b.head);
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    s.finish().await;
}

#[tokio::test]
async fn sixty_challenges_are_issued_a_minute_and_the_next_is_429_without_touching_the_store() {
    let s = dead(instruction_key()).await;
    let early = get_challenge(&s).await;
    for i in 1..60 {
        let r = ask_challenge(&s).await;
        assert_eq!(r.status, 200, "request {i}: {}", r.head);
    }
    for i in 0..2 {
        let r = ask_challenge(&s).await;
        assert_problem(&r, 429, &format!("over the cap, {i}"));
        assert!((1..=60).contains(&retry_after(&r)), "{i}");
    }
    // The cap does not touch the session route, and a challenge issued before it is still live:
    // it reaches the database (503, the database is down), it is not a 401.
    let r = send_session(&s, &shaped_body(&early)).await;
    assert_problem(&r, 503, "the early challenge");
    s.finish().await;
}

#[tokio::test]
async fn each_session_shape_rule_is_a_400_and_consumes_no_challenge() {
    let s = dead(instruction_key()).await;
    let c = get_challenge(&s).await;
    let good: serde_json::Value = serde_json::from_str(&shaped_body(&c)).unwrap();
    let with = |name: &str, value: &str| {
        let mut v = good.clone();
        v[name] = serde_json::Value::String(value.to_owned());
        v.to_string()
    };
    let bodies = [
        ("not json", "nope".to_owned()),
        ("array", "[]".to_owned()),
        ("empty object", "{}".to_owned()),
        ("trailing bytes", format!("{good} x")),
        (
            "upper case id",
            with("agentId", &good["agentId"].as_str().unwrap().to_uppercase()),
        ),
        ("short id", with("agentId", "0a1b2c3d")),
        ("challenge not base64", with("challenge", "!!")),
        (
            "challenge short",
            with("challenge", &STANDARD.encode([1u8; 31])),
        ),
        ("nonce short", with("nonce", &STANDARD.encode([1u8; 31]))),
        ("proof short", with("proof", &STANDARD.encode([1u8; 63]))),
        (
            "proof unpadded",
            with("proof", STANDARD.encode([1u8; 64]).trim_end_matches('=')),
        ),
    ];
    for (name, body) in &bodies {
        assert_problem(&send_session(&s, body).await, 400, name);
    }
    let wrong_type = post(&s.host(), SESSION, Some("text/plain"), &good.to_string());
    assert_problem(&exchange(s.addr, &wrong_type).await, 400, "content type");
    let none = post(&s.host(), SESSION, None, &good.to_string());
    assert_problem(&exchange(s.addr, &none).await, 400, "no content type");
    let big = post(
        &s.host(),
        SESSION,
        Some("application/json"),
        &"x".repeat(4097),
    );
    assert_problem(&exchange(s.addr, &big).await, 413, "4097 bytes");

    // S4: a body that is not the expected shape and a target that cannot be proved: the shape's
    // own 400 answers, not the target's.
    let bad_target = format!("{SESSION}?{}", "a".repeat(2100));
    let r = exchange(
        s.addr,
        &post(&s.host(), &bad_target, Some("application/json"), "nope"),
    )
    .await;
    assert_problem(&r, 400, "shape error and bad target");
    assert!(
        r.body.contains("The body is not the expected JSON object"),
        "{}",
        r.body
    );

    // The challenge in those bodies is still live (503: it reached the dead database), and is
    // consumed by that presentation (a second one is 401).
    assert_problem(
        &send_session(&s, &good.to_string()).await,
        503,
        "first presentation",
    );
    let again = send_session(&s, &good.to_string()).await;
    assert_problem(&again, 401, "second presentation");
    assert_eq!(again.header("www-authenticate"), Some("Bearer"));
    s.finish().await;
}

#[tokio::test]
async fn an_unknown_challenge_is_401_before_the_database_and_is_never_counted() {
    let s = dead(instruction_key()).await;
    for i in 0..12 {
        let r = send_session(&s, &shaped_body(&random_32().unwrap())).await;
        assert_problem(&r, 401, &format!("unknown challenge {i}"));
        assert_eq!(r.header("www-authenticate"), Some("Bearer"), "{i}");
        assert!(r.header("retry-after").is_none(), "{i}");
    }
    s.finish().await;
}

#[tokio::test]
async fn the_guard_answers_both_routes_before_they_run_and_the_cap_is_untouched() {
    let s = dead(instruction_key()).await;
    let port = s.addr.port();
    for path in [CHALLENGE, SESSION] {
        let moved = format!("Host: localhost:{port}");
        let r = exchange(s.addr, &post(&moved, path, Some("application/json"), "")).await;
        assert_eq!(r.status, 307, "{path}: {}", r.head);
        let foreign = format!("{}\r\nOrigin: http://evil.example", s.host());
        let r = exchange(s.addr, &post(&foreign, path, Some("application/json"), "")).await;
        assert_eq!(r.status, 403, "{path}: {}", r.head);
    }
    // 65 refused requests issued nothing and used none of the cap of 60.
    for _ in 0..65 {
        let foreign = format!("{}\r\nOrigin: http://evil.example", s.host());
        exchange(s.addr, &post(&foreign, CHALLENGE, None, "")).await;
    }
    assert_eq!(ask_challenge(&s).await.status, 200);
    s.finish().await;
}

// ---------------------------------------------------------------------------------------------
// With a database

/// A migrated database with one registered agent, and a session server on a pool of its own
/// (`Server::run` closes the pool it is given).
pub(crate) struct Up {
    pub(crate) w: World,
    agent: Agent,
    pub(crate) id: String,
    pub(crate) s: Running,
    pub(crate) sessions: Sessions,
    /// The server's hub: the control channel tests send commands and take the inbox through it.
    pub(crate) hub: Hub,
    /// The server's operator sessions: a test that needs a session the login route cannot make
    /// (the login serves the local tenant only) puts one in this table.
    pub(crate) operator_sessions: server::OperatorSessions,
    /// The text of every challenge and nonce this test sent, for the log check.
    seen: std::sync::Mutex<Vec<String>>,
}

impl Up {
    pub(crate) async fn new() -> Up {
        Up::with_grace(2).await
    }

    /// Like [`Up::new`], with a shutdown grace period of `grace_secs` seconds.
    pub(crate) async fn with_grace(grace_secs: u64) -> Up {
        Up::with_queue(grace_secs, None).await
    }

    /// Like [`Up::with_grace`], with the delivery queue on when `queue` has its settings.
    pub(crate) async fn with_queue(grace_secs: u64, queue: Option<Settings>) -> Up {
        Up::with_tasks(grace_secs, queue, None).await
    }

    /// Like [`Up::with_queue`], with the job expiry task on when `expiry` has its settings.
    pub(crate) async fn with_tasks(
        grace_secs: u64,
        queue: Option<Settings>,
        expiry: Option<server::job_expiry::Settings>,
    ) -> Up {
        Up::with_backend(grace_secs, queue, expiry, Default::default()).await
    }

    /// Like [`Up::with_tasks`], with the data channel's object store and part size.
    pub(crate) async fn with_backend(
        grace_secs: u64,
        queue: Option<Settings>,
        expiry: Option<server::job_expiry::Settings>,
        backend: server::uploads::Backend,
    ) -> Up {
        Up::with_world(
            World::new().await,
            grace_secs,
            (queue, expiry, None),
            backend,
        )
        .await
    }

    /// Like [`Up::with_queue`], with the communication-log delivery task on (it runs beside the
    /// queue, which hands it the agents' answers).
    pub(crate) async fn with_comm_log(
        grace_secs: u64,
        queue: Settings,
        comm_log: server::comm_log_requests::Settings,
    ) -> Up {
        Up::with_world(
            World::new().await,
            grace_secs,
            (Some(queue), None, Some(comm_log)),
            Default::default(),
        )
        .await
    }

    /// Like [`Up::new`], with a signer that fails once the returned flag is set (ADR-504): the
    /// agent is registered while it still signs.
    pub(crate) async fn flaky() -> (Up, Arc<AtomicBool>) {
        Up::flaky_with(Default::default()).await
    }

    /// Like [`Up::flaky`], with the data channel's object store and part size.
    pub(crate) async fn flaky_with(backend: server::uploads::Backend) -> (Up, Arc<AtomicBool>) {
        let fail = Arc::new(AtomicBool::new(false));
        let key = Arc::new(server::InstructionKey::from_signer(Arc::new(Flaky {
            key: SigningKey::generate().unwrap(),
            fail: fail.clone(),
        })));
        let w = World::with_key(key).await;
        let up = Up::with_world(w, 2, (None, None, None), backend).await;
        (up, fail)
    }

    async fn with_world(
        w: World,
        grace_secs: u64,
        tasks: (
            Option<Settings>,
            Option<server::job_expiry::Settings>,
            Option<server::comm_log_requests::Settings>,
        ),
        backend: server::uploads::Backend,
    ) -> Up {
        let (queue, expiry, comm_log) = tasks;
        w.add_operator("alice").await;
        let agent = Agent::new();
        let id = w.register(&agent).await;
        let http = Http {
            port: 0,
            shutdown_grace_secs: grace_secs,
        };
        let server = Server::bind_with_store(&http, w.pg.pool(), w.key.clone(), backend)
            .await
            .unwrap();
        let server = match queue {
            Some(settings) => server.with_job_queue(settings),
            None => server,
        };
        let server = match expiry {
            Some(settings) => server.with_job_expiry(settings),
            None => server,
        };
        let server = match comm_log {
            Some(settings) => server.with_comm_log_requests(settings),
            None => server,
        };
        let sessions = server.sessions();
        let hub = server.hub();
        let operator_sessions = server.operator_sessions();
        Up {
            w,
            agent,
            id,
            s: Running::spawn(server),
            sessions,
            hub,
            operator_sessions,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn audience(&self) -> Audience {
        audience_of(self.s.addr.port())
    }

    /// A valid exchange for `key` as agent `id`, with a fresh challenge from the route.
    pub(crate) async fn exchange_as(&self, key: &SigningKey, id: &str) -> Reply {
        let c = self.challenge_and_nonce().await;
        let p = proof(key, &self.audience(), id, c.0, c.1);
        send_session(&self.s, &session_body(id, &c.0, &c.1, &p)).await
    }

    async fn challenge_and_nonce(&self) -> ([u8; 32], [u8; 32]) {
        let pair = (get_challenge(&self.s).await, random_32().unwrap());
        let mut seen = self.seen.lock().unwrap();
        seen.push(STANDARD.encode(pair.0));
        seen.push(STANDARD.encode(pair.1));
        drop(seen);
        pair
    }

    /// A valid exchange of this agent with another server `s` than this one's.
    pub(crate) async fn valid_on(&self, s: &Running) -> Reply {
        let (challenge, nonce) = (get_challenge(s).await, random_32().unwrap());
        let p = proof(
            &self.agent.key,
            &audience_of(s.addr.port()),
            &self.id,
            challenge,
            nonce,
        );
        send_session(s, &session_body(&self.id, &challenge, &nonce, &p)).await
    }

    pub(crate) async fn valid(&self) -> Reply {
        self.exchange_as(&self.agent.key, &self.id).await
    }

    /// A refusal: a proof by a key that is not the agent's.
    async fn refused(&self) -> Reply {
        self.exchange_as(&SigningKey::generate().unwrap(), &self.id)
            .await
    }

    pub(crate) async fn finish(self) {
        self.s.finish().await;
        self.w.finish().await;
    }
}

impl World {
    /// Registers `agent` for the operator `alice` and returns the agent ID the server assigned.
    pub(crate) async fn register(&self, agent: &Agent) -> String {
        let (_, r) = self.redeem("alice", agent).await;
        assert_eq!(r.status, 201, "{}", r.head);
        json(&r)["agentId"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn a_valid_exchange_gives_a_token_that_authenticates_and_writes_nothing() {
    let u = Up::new().await;
    let r = u.valid().await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let v = json(&r);
    assert_eq!(v["expiresInSecs"], 300);
    let token = v["token"].as_str().unwrap();
    assert_eq!(STANDARD.decode(token).unwrap().len(), 32);
    let agent = AgentId(u.id.clone());
    assert_eq!(u.sessions.authenticate(token), Some(agent.clone()));
    assert_eq!(u.sessions.authenticate(token), Some(agent));
    assert_eq!(
        u.w.sql(&format!(
            "SELECT last_seen_at IS NULL FROM agents WHERE id = '{}'",
            u.id
        ))
        .await,
        "true"
    );
    u.finish().await;
}

#[tokio::test]
async fn a_challenge_serves_one_presentation_whatever_its_outcome() {
    let u = Up::new().await;
    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &u.audience(), &u.id, c.0, c.1);
    let body = session_body(&u.id, &c.0, &c.1, &p);
    assert_eq!(send_session(&u.s, &body).await.status, 200);
    let again = send_session(&u.s, &body).await;
    assert_problem(&again, 401, "the same request again");
    assert_eq!(again.header("www-authenticate"), Some("Bearer"));

    // A 403 consumes its challenge too.
    let c = u.challenge_and_nonce().await;
    let bad = proof(
        &SigningKey::generate().unwrap(),
        &u.audience(),
        &u.id,
        c.0,
        c.1,
    );
    let body = session_body(&u.id, &c.0, &c.1, &bad);
    assert_problem(&send_session(&u.s, &body).await, 403, "a bad proof");
    assert_problem(
        &send_session(&u.s, &body).await,
        401,
        "its second presentation",
    );
    u.finish().await;
}

#[tokio::test]
async fn every_refusal_is_the_same_403_and_its_reason_is_logged_once() {
    let (logs, _guard) = capture();
    let u = Up::new().await;
    let audience = u.audience();
    let port = u.s.addr.port();
    let mut secrets: Vec<String> = Vec::new();
    let mut replies: Vec<(&str, Reply)> = Vec::new();

    // An agent ID no row has.
    let stranger = u.w.sql("SELECT gen_random_uuid()").await;
    replies.push((
        "unknown agent",
        u.exchange_as(&u.agent.key, &stranger).await,
    ));

    // A row revoked by SQL, with a valid signature by its own key.
    let second = Agent::new();
    let second_id = u.w.register(&second).await;
    sqlx::query("UPDATE agents SET revoked_at = now() WHERE id = $1::uuid")
        .bind(&second_id)
        .execute(&u.w.pool)
        .await
        .unwrap();
    replies.push((
        "revoked by SQL",
        u.exchange_as(&second.key, &second_id).await,
    ));

    // A row revoked by registering its key again: the old ID is refused, the new one is not.
    let third = Agent::new();
    let third_old = u.w.register(&third).await;
    let third_new = u.w.register(&third).await;
    assert_ne!(third_old, third_new);
    replies.push((
        "revoked by registering again",
        u.exchange_as(&third.key, &third_old).await,
    ));

    // Another key's signature.
    replies.push(("another key", u.refused().await));

    // A proof for another audience, another challenge, another agent ID, and a high-S twin.
    let c = u.challenge_and_nonce().await;
    let other_audience = audience_of(other_port(port));
    let p = proof(&u.agent.key, &other_audience, &u.id, c.0, c.1);
    secrets.push(STANDARD.encode(&p));
    replies.push((
        "another audience",
        send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &p)).await,
    ));

    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &audience, &u.id, random_32().unwrap(), c.1);
    replies.push((
        "another challenge",
        send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &p)).await,
    ));

    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &audience, &second_id, c.0, c.1);
    replies.push((
        "another agent ID",
        send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &p)).await,
    ));

    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &audience, &u.id, c.0, c.1);
    secrets.push(STANDARD.encode(high_s(&p)));
    replies.push((
        "high-S",
        send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &high_s(&p))).await,
    ));

    let first = bytes_of(&replies[0].1);
    for (name, r) in &replies {
        assert_problem(r, 403, name);
        assert_eq!(bytes_of(r), first, "{name}: the 403 differs from the first");
    }

    // The new ID of the re-registered key is accepted, and the log never held a secret.
    let ok = u.exchange_as(&third.key, &third_new).await;
    assert_eq!(ok.status, 200, "{}", ok.head);
    secrets.push(json(&ok)["token"].as_str().unwrap().to_owned());

    secrets.extend(u.seen.lock().unwrap().iter().cloned());
    let text = logs.text();
    for word in ["unknown_agent", "revoked", "proof"] {
        let lines = text
            .lines()
            .filter(|l| l.contains("session refused") && l.contains(&format!("reason=\"{word}\"")))
            .count();
        assert_eq!(lines, 1, "reason {word}: one gated line");
    }
    assert!(text.contains("session issued"));
    for (i, secret) in secrets.iter().enumerate() {
        assert!(!text.contains(secret.as_str()), "secret {i} is in the log");
    }
    u.finish().await;
}

#[tokio::test]
async fn the_eleventh_refusal_is_429_and_a_401_never_counts_or_becomes_one() {
    let u = Up::new().await;
    // Stale challenges first: they are 401, never counted.
    for i in 0..12 {
        let r = send_session(&u.s, &shaped_body(&random_32().unwrap())).await;
        assert_problem(&r, 401, &format!("stale {i}"));
    }
    for i in 0..10 {
        assert_problem(&u.refused().await, 403, &format!("refusal {i}"));
    }
    let limited = u.refused().await;
    assert_problem(&limited, 429, "the eleventh refusal");
    assert!((1..=60).contains(&retry_after(&limited)));

    // While limited: a stale challenge is still 401, a shape error 400, a valid exchange 200,
    // and the next refusal is limited still.
    let stale = send_session(&u.s, &shaped_body(&random_32().unwrap())).await;
    assert_problem(&stale, 401, "stale while limited");
    assert_problem(
        &send_session(&u.s, "nope").await,
        400,
        "shape while limited",
    );
    assert_eq!(u.valid().await.status, 200);
    assert_problem(&u.refused().await, 429, "the next refusal");
    u.finish().await;
}

#[tokio::test]
async fn a_database_failure_is_503_and_is_not_counted() {
    let u = Up::new().await;
    for i in 0..9 {
        assert_problem(&u.refused().await, 403, &format!("refusal {i}"));
    }
    // A held exclusive lock on the table makes the row read wait past its lock timeout.
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let r = u.valid().await;
    assert_problem(&r, 503, "the locked table");
    drop(tx);

    // The 503 consumed its challenge and counted nothing: the tenth refusal is still a 403.
    assert_problem(&u.refused().await, 403, "the tenth refusal");
    assert_problem(&u.refused().await, 429, "the eleventh refusal");
    u.finish().await;
}

#[tokio::test]
async fn a_stored_key_that_does_not_decode_is_500_and_is_not_counted() {
    let u = Up::new().await;
    let broken = Agent::new();
    let broken_id = u.w.register(&broken).await;
    sqlx::query("UPDATE agents SET public_key = '\\x01' WHERE id = $1::uuid")
        .bind(&broken_id)
        .execute(&u.w.pool)
        .await
        .unwrap();
    for i in 0..9 {
        assert_problem(&u.refused().await, 403, &format!("refusal {i}"));
    }
    let r = u.exchange_as(&broken.key, &broken_id).await;
    assert_problem(&r, 500, "the broken key");
    assert_problem(&u.refused().await, 403, "the tenth refusal");
    assert_problem(&u.refused().await, 429, "the eleventh refusal");
    u.finish().await;
}

#[tokio::test]
async fn a_challenge_and_a_token_expire_when_the_clock_moves() {
    let u = Up::new().await;
    // Back to back, with no request in flight: the clock is paused only for the jump.
    let jump = |secs: u64| async move {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(secs)).await;
        tokio::time::resume();
    };

    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &u.audience(), &u.id, c.0, c.1);
    jump(50).await;
    let r = send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &p)).await;
    assert_eq!(r.status, 200, "a challenge 50 s old is live: {}", r.head);
    let token = json(&r)["token"].as_str().unwrap().to_owned();

    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &u.audience(), &u.id, c.0, c.1);
    jump(61).await;
    let r = send_session(&u.s, &session_body(&u.id, &c.0, &c.1, &p)).await;
    assert_problem(&r, 401, "a challenge 61 s old");

    // The token is now about 61 s old.
    jump(229).await;
    assert!(
        u.sessions.authenticate(&token).is_some(),
        "a token about 290 s old"
    );
    jump(11).await;
    assert!(
        u.sessions.authenticate(&token).is_none(),
        "a token about 301 s old"
    );
    u.finish().await;
}

#[tokio::test]
async fn a_token_lives_its_whole_lifetime_from_the_mint_after_a_slow_row_read() {
    let u = Up::new().await;
    // A lock on the table, held for 3 s (inside the 5 s lock timeout), delays the row read.
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let release = async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(tx);
    };
    let (r, ()) = tokio::join!(u.valid(), release);
    assert_eq!(r.status, 200, "the delayed exchange: {}", r.head);
    let token = json(&r)["token"].as_str().unwrap().to_owned();

    // Had the lifetime started before the wait, the token would be over 300 s old here.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(298)).await;
    tokio::time::resume();
    assert!(
        u.sessions.authenticate(&token).is_some(),
        "a token about 298 s past its mint"
    );
    u.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_valid_request_sent_twice_at_once_gets_one_token() {
    let u = Up::new().await;
    for round in 0..20 {
        let c = u.challenge_and_nonce().await;
        let p = proof(&u.agent.key, &u.audience(), &u.id, c.0, c.1);
        let body = session_body(&u.id, &c.0, &c.1, &p);
        let (a, b) = tokio::join!(send_session(&u.s, &body), send_session(&u.s, &body));
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 401], "round {round}");
    }
    u.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The answer proof (ADR-504)

/// A session request body for a fresh challenge, and the nonce in it.
async fn fresh_body(u: &Up) -> (String, [u8; 32]) {
    let c = u.challenge_and_nonce().await;
    let p = proof(&u.agent.key, &u.audience(), &u.id, c.0, c.1);
    (session_body(&u.id, &c.0, &c.1, &p), c.1)
}

/// Checks that `r` is the proved 200 of the session request `body` made with `nonce`.
fn assert_proved_session(u: &Up, nonce: [u8; 32], target: &str, body: &str, r: &Reply) {
    let public = u.w.key.public_key();
    let audience = u.audience();
    Answered {
        public: &public,
        audience: &audience,
        agent_id: &u.id,
        nonce,
        method: "POST",
        target,
        request_body: body.as_bytes(),
    }
    .verify(200, r);
}

#[tokio::test]
async fn a_session_200_is_proved_with_the_bodys_nonce() {
    let u = Up::new().await;
    let (body, nonce) = fresh_body(&u).await;
    let r = send_session(&u.s, &body).await;
    assert_proved_session(&u, nonce, SESSION, &body, &r);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    // The token in the proved body is the one the table holds.
    let token = json(&r)["token"].as_str().unwrap().to_owned();
    assert_eq!(u.sessions.authenticate(&token), Some(AgentId(u.id.clone())));

    // The proof is bound to the request: another nonce, target or status does not verify it.
    let public = u.w.key.public_key();
    let audience = u.audience();
    let verifies = |nonce: [u8; 32], method: &str, target: &str, status: u16| {
        let other = Answered {
            public: &public,
            audience: &audience,
            agent_id: &u.id,
            nonce,
            method,
            target,
            request_body: body.as_bytes(),
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| other.verify(status, &r))).is_ok()
    };
    assert!(verifies(nonce, "POST", SESSION, 200), "the control case");
    assert!(!verifies(random_32().unwrap(), "POST", SESSION, 200));
    assert!(!verifies(nonce, "GET", SESSION, 200));
    assert!(!verifies(nonce, "POST", CHALLENGE, 200));
    assert!(!verifies(nonce, "POST", SESSION, 201));
    u.finish().await;
}

#[tokio::test]
async fn the_session_route_reads_no_nonce_header() {
    let u = Up::new().await;
    let other = STANDARD.encode(random_32().unwrap());
    let header = |value: &str| format!("{NONCE_HEADER}: {value}");
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("another valid nonce", vec![header(&other)]),
        ("a malformed nonce", vec![header("not base64")]),
        ("two values", vec![header(&other), header(&other)]),
    ];
    for (name, headers) in cases {
        let (body, nonce) = fresh_body(&u).await;
        let raw = post_with(
            &u.s.host(),
            SESSION,
            Some("application/json"),
            &headers,
            &body,
        );
        let r = exchange(u.s.addr, &raw).await;
        assert_eq!(r.status, 200, "{name}: {}", r.head);
        assert_proved_session(&u, nonce, SESSION, &body, &r);
    }
    u.finish().await;
}

#[tokio::test]
async fn a_session_target_that_cannot_be_proved_is_400_and_takes_no_challenge() {
    let u = Up::new().await;
    let (body, nonce) = fresh_body(&u).await;
    let long = format!("{SESSION}?{}", "a".repeat(2100));
    // A raw byte outside ASCII in the query: the parser may refuse it before the route does,
    // which is a 400 too.
    let raw_byte = format!("{SESSION}?\u{e9}");
    for (name, target) in [("over-long", long), ("non-ASCII", raw_byte)] {
        let raw = post(&u.s.host(), &target, Some("application/json"), &body);
        let r = exchange(u.s.addr, &raw).await;
        assert_problem(&r, 400, name);
        assert_unproved(&r, name);
    }
    // The same challenge, presented at a target that can be proved, still succeeds.
    let r = send_session(&u.s, &body).await;
    assert_proved_session(&u, nonce, SESSION, &body, &r);
    u.finish().await;
}

#[tokio::test]
async fn no_session_refusal_carries_a_proof() {
    let u = Up::new().await;
    let host = u.s.host();
    let mut replies: Vec<(&str, Reply)> = Vec::new();

    // 400: a shape error.
    replies.push(("400", send_session(&u.s, "nope").await));
    // 401: a challenge the store does not hold.
    replies.push((
        "401",
        send_session(&u.s, &shaped_body(&random_32().unwrap())).await,
    ));
    // 403: a proof by another key.
    replies.push(("403", u.refused().await));
    // 405 and 413.
    let get = format!(
        "GET {SESSION} HTTP/1.1\r\n{host}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    replies.push(("405", exchange(u.s.addr, &get).await));
    let big = post(&host, SESSION, Some("application/json"), &"x".repeat(4097));
    replies.push(("413", exchange(u.s.addr, &big).await));
    // The request guard.
    let foreign = format!("{host}\r\nOrigin: http://evil.example");
    let raw = post(&foreign, SESSION, Some("application/json"), "nope");
    replies.push(("guard 403", exchange(u.s.addr, &raw).await));
    // 408: a body that stalls.
    let stalled = format!(
        "POST {SESSION} HTTP/1.1\r\n{host}\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n"
    );
    replies.push(("408", exchange(u.s.addr, &stalled).await));
    // 500: a stored key that does not decode.
    let broken = Agent::new();
    let broken_id = u.w.register(&broken).await;
    sqlx::query("UPDATE agents SET public_key = '\\x01' WHERE id = $1::uuid")
        .bind(&broken_id)
        .execute(&u.w.pool)
        .await
        .unwrap();
    replies.push(("500", u.exchange_as(&broken.key, &broken_id).await));
    // 503: a table that stays locked past the lock timeout.
    let mut tx = u.w.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE agents IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    replies.push(("503", u.valid().await));
    drop(tx);
    // 429: past ten refusals within a minute (9 so far counted: 403 above is the first).
    for _ in 0..9 {
        u.refused().await;
    }
    replies.push(("429", u.refused().await));

    let wanted = [400, 401, 403, 405, 413, 403, 408, 500, 503, 429];
    for ((name, r), status) in replies.iter().zip(wanted) {
        assert_eq!(r.status, status, "{name}: {}", r.head);
        assert_unproved(r, name);
    }
    u.finish().await;
}

#[tokio::test]
async fn a_signing_failure_at_the_session_route_leaves_no_token() {
    let (logs, _guard) = capture();
    let (u, fail) = Up::flaky().await;
    let (body, nonce) = fresh_body(&u).await;
    fail.store(true, Ordering::SeqCst);
    let r = send_session(&u.s, &body).await;
    assert_problem(&r, 503, "the signer fails");
    assert_unproved(&r, "the signer fails");
    assert!(!r.body.contains("token"), "{}", r.body);
    assert_eq!(format!("{:?}", u.sessions), "Sessions { count: 0 }");
    // The challenge is consumed.
    assert_problem(&send_session(&u.s, &body).await, 401, "the same challenge");
    // The failure is logged once, with neither the target nor a nonce.
    let text = logs.text();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("an answer could not be proved"))
            .count(),
        1,
        "{text}"
    );
    assert!(!text.contains(&STANDARD.encode(nonce)));
    assert!(!text.contains(SESSION), "{text}");

    // The next exchange succeeds once the signer does.
    fail.store(false, Ordering::SeqCst);
    let (body, nonce) = fresh_body(&u).await;
    let r = send_session(&u.s, &body).await;
    assert_proved_session(&u, nonce, SESSION, &body, &r);
    assert_eq!(format!("{:?}", u.sessions), "Sessions { count: 1 }");
    u.finish().await;
}

#[tokio::test]
async fn the_challenge_answer_is_never_proved() {
    let s = dead(instruction_key()).await;
    let nonce = STANDARD.encode(random_32().unwrap());
    let headers = [format!("{NONCE_HEADER}: {nonce}")];
    let raw = post_with(&s.host(), CHALLENGE, None, &headers, "");
    let r = exchange(s.addr, &raw).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_unproved(&r, "the challenge");
    s.finish().await;
}

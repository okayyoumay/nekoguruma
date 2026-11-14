//! `POST /api/v1/agent/challenge` and `POST /api/v1/agent/session` over real TCP (ADR-466 items 1
//! to 4, ADR-467): the rules that need no database run over a pool that points at nothing; the
//! rest run against a throwaway PostgreSQL. Keys, challenges and nonces are made at run time, and
//! no assertion message prints a secret: values are named by index.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use server::config::Http;
use server::job_queue::Settings;
use server::local_guard::pinned_origin;
use server::{Hub, Server, Sessions};
use shared_crypto::proof::{Audience, ProofTranscript, sign_proof};
use shared_crypto::{SigningKey, random_32};
use shared_proto::AgentId;

use crate::redeem::{Agent, World, bytes_of, capture, dead, high_s, instruction_key, json};
use crate::serving::{Reply, Running, exchange};

const CHALLENGE: &str = "/api/v1/agent/challenge";
const SESSION: &str = "/api/v1/agent/session";

// ---------------------------------------------------------------------------------------------
// Requests

fn post(host: &str, path: &str, content_type: Option<&str>, body: &str) -> String {
    let mut raw = format!("POST {path} HTTP/1.1\r\n{host}\r\nConnection: close\r\n");
    if let Some(content_type) = content_type {
        raw.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw
}

async fn ask_challenge(s: &Running) -> Reply {
    exchange(s.addr, &post(&s.host(), CHALLENGE, None, "")).await
}

async fn send_session(s: &Running, body: &str) -> Reply {
    exchange(
        s.addr,
        &post(&s.host(), SESSION, Some("application/json"), body),
    )
    .await
}

/// A challenge from the route, as the bytes it stands for.
async fn get_challenge(s: &Running) -> [u8; 32] {
    let r = ask_challenge(s).await;
    assert_eq!(r.status, 200, "{}", r.head);
    let text = json(&r)["challenge"].as_str().unwrap().to_owned();
    STANDARD.decode(text).unwrap().try_into().unwrap()
}

fn session_body(id: &str, challenge: &[u8; 32], nonce: &[u8; 32], proof: &[u8]) -> String {
    serde_json::json!({
        "agentId": id,
        "challenge": STANDARD.encode(challenge),
        "nonce": STANDARD.encode(nonce),
        "proof": STANDARD.encode(proof),
    })
    .to_string()
}

fn audience_of(port: u16) -> Audience {
    Audience::parse_canonical(&pinned_origin(port)).unwrap()
}

/// The audience of a server other than the one on `port`.
fn other_port(port: u16) -> u16 {
    if port < u16::MAX { port + 1 } else { port - 1 }
}

fn proof(
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
    sessions: Sessions,
    /// The server's hub: the control channel tests send commands and take the inbox through it.
    pub(crate) hub: Hub,
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
        let w = World::new().await;
        w.add_operator("alice").await;
        let agent = Agent::new();
        let id = w.register(&agent).await;
        let http = Http {
            port: 0,
            shutdown_grace_secs: grace_secs,
        };
        let server = Server::bind(&http, w.pg.pool(), w.key.clone())
            .await
            .unwrap();
        let server = match queue {
            Some(settings) => server.with_job_queue(settings),
            None => server,
        };
        let sessions = server.sessions();
        let hub = server.hub();
        Up {
            w,
            agent,
            id,
            s: Running::spawn(server),
            sessions,
            hub,
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

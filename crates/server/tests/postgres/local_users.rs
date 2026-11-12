//! Local users over real TCP and as database operations (design 13.3, ADR-469): the login,
//! logout and whoami routes, the session layer, and `local_user::add`, `set_password` and
//! `disable`. The rules that need no database run over a pool that points at nothing; the rest
//! run against a throwaway PostgreSQL. Passwords are made up per test and no assertion message
//! prints one.
//!
//! The case names in the comments (L for login, C for concurrency, S for the session layer, O for
//! logout, M for whoami, K for clocks, U for the commands) are the rows of ADR-469's table.
//! The tests that read the log run on the current-thread runtime, so the server's tasks log on
//! the test's thread, and the clock moves only in jumps (`pause`, `advance`, `resume` back to
//! back) while the server is at rest.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use server::config::Http;
use server::local_guard::pinned_origin;
use server::local_user::{
    self, DUMMY_PASSWORD, Hasher, LOCAL_TENANT_ID, NewUser, ROLES, UserError,
};
use server::{Server, db};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::redeem::{capture, dead, instruction_key, json};
use crate::serving::{Reply, Running, exchange};
use crate::support::TestPostgres;

const LIMIT: Duration = Duration::from_secs(30);
const PASSWORD: &str = "correct horse battery";
const CLEARED: &str = "ngr_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict";

// ---------------------------------------------------------------------------------------------
// Requests

fn origin(addr: SocketAddr) -> String {
    pinned_origin(addr.port())
}

fn build(
    addr: SocketAddr,
    method: &str,
    path: &str,
    origin: Option<&str>,
    headers: &[String],
    body: &str,
) -> String {
    let mut raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    if let Some(origin) = origin {
        raw.push_str(&format!("Origin: {origin}\r\n"));
    }
    for header in headers {
        raw.push_str(header);
        raw.push_str("\r\n");
    }
    raw.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw
}

fn json_headers(cookie: Option<&str>) -> Vec<String> {
    let mut headers = vec!["Content-Type: application/json".to_owned()];
    if let Some(cookie) = cookie {
        headers.push(format!("Cookie: {cookie}"));
    }
    headers
}

async fn login_raw(addr: SocketAddr, body: &str, cookie: Option<&str>) -> Reply {
    let raw = build(
        addr,
        "POST",
        "/api/v1/login",
        Some(&origin(addr)),
        &json_headers(cookie),
        body,
    );
    exchange(addr, &raw).await
}

fn login_body(username: &str, password: &str) -> String {
    serde_json::json!({ "username": username, "password": password }).to_string()
}

async fn login(addr: SocketAddr, username: &str, password: &str) -> Reply {
    login_raw(addr, &login_body(username, password), None).await
}

async fn login_with_cookie(addr: SocketAddr, username: &str, cookie: &str) -> Reply {
    login_raw(addr, &login_body(username, PASSWORD), Some(cookie)).await
}

async fn get_me(addr: SocketAddr, cookies: &[&str]) -> Reply {
    let headers: Vec<String> = cookies.iter().map(|c| format!("Cookie: {c}")).collect();
    exchange(addr, &build(addr, "GET", "/api/v1/me", None, &headers, "")).await
}

async fn post_logout(addr: SocketAddr, cookies: &[String]) -> Reply {
    let headers: Vec<String> = cookies.iter().map(|c| format!("Cookie: {c}")).collect();
    let raw = build(
        addr,
        "POST",
        "/api/v1/logout",
        Some(&origin(addr)),
        &headers,
        "",
    );
    exchange(addr, &raw).await
}

/// `ngr_session=<token>` from a Set-Cookie header.
fn cookie_of(reply: &Reply) -> String {
    let value = reply
        .header("set-cookie")
        .unwrap_or_else(|| panic!("no Set-Cookie: {}", reply.head));
    value.split(';').next().unwrap().to_owned()
}

fn assert_problem(r: &Reply, status: u16, what: &str) {
    assert_eq!(r.status, status, "{what}: {}", r.head);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
}

fn assert_signed_out(r: &Reply, what: &str) {
    assert_problem(r, 401, what);
    assert_eq!(r.header("set-cookie"), Some(CLEARED), "{what}");
}

fn retry_after(r: &Reply) -> u64 {
    r.header("retry-after")
        .expect("Retry-After")
        .parse()
        .unwrap()
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

/// A jump of the clock: the clock is paused only for it, with no request in flight.
async fn jump(secs: u64) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(secs)).await;
    tokio::time::resume();
}

// ---------------------------------------------------------------------------------------------
// The world

/// A migrated database and a server on it.
struct Lab {
    pg: TestPostgres,
    pool: sqlx::PgPool,
    server: Running,
    hasher: Hasher,
}

impl Lab {
    async fn new() -> Lab {
        let pg = TestPostgres::start();
        let pool = pg.pool();
        db::migrate_with(&pool, &db::MIGRATOR, LIMIT)
            .await
            .expect("the migrations apply");
        let http = Http {
            port: 0,
            shutdown_grace_secs: 2,
        };
        let server = Server::bind(&http, pool.clone(), instruction_key())
            .await
            .unwrap();
        let hasher = server.hasher();
        Lab {
            pg,
            pool,
            server: Running::spawn(server),
            hasher,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.server.addr
    }

    /// Adds a user the way the command does and returns the operator ID.
    async fn user(&self, name: &str, display: &str, roles: &[&str], password: &str) -> String {
        add_user(&self.pool, name, display, roles, password)
            .await
            .unwrap()
    }

    async fn sql(&self, sql: &str, bind: &str) {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(bind)
            .execute(&self.pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    async fn finish(self) {
        self.server.finish().await;
    }
}

async fn add_user(
    pool: &sqlx::PgPool,
    name: &str,
    display: &str,
    roles: &[&str],
    password: &str,
) -> Result<String, UserError> {
    let hash = Hasher::new().hash(password.to_owned()).await.unwrap();
    let roles: Vec<String> = roles.iter().map(|r| (*r).to_owned()).collect();
    let user = NewUser::new(name, display, &roles).unwrap();
    local_user::add(pool, &user, &hash)
        .await
        .map(|added| added.operator_id)
}

// ---------------------------------------------------------------------------------------------
// Login: success

#[tokio::test]
async fn a_login_answers_the_whoami_document_with_the_cookie_and_logs_only_the_id() {
    let (log, _guard) = capture();
    let lab = Lab::new().await;
    let alice = lab
        .user("alice", "Alice A", &["viewer", "admin"], PASSWORD)
        .await;
    let bob = lab.user("bob", "Bob B", &[], PASSWORD).await;

    // L18
    let r = login(lab.addr(), "alice", PASSWORD).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let cookie = cookie_of(&r);
    let token = cookie.strip_prefix("ngr_session=").unwrap();
    assert_eq!(token.len(), 43, "{cookie}");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_eq!(
        r.header("set-cookie").unwrap(),
        format!("{cookie}; HttpOnly; SameSite=Strict; Path=/")
    );
    assert_eq!(
        json(&r),
        serde_json::json!({
            "operatorId": alice,
            "username": "alice",
            "displayName": "Alice A",
            "roles": ["admin", "viewer"],
        })
    );
    // L19
    let r = login(lab.addr(), "bob", PASSWORD).await;
    assert_eq!(r.status, 200);
    assert_eq!(json(&r)["roles"], serde_json::json!([]));
    assert_eq!(json(&r)["operatorId"], bob.as_str());
    let bob_cookie = cookie_of(&r);
    assert_ne!(bob_cookie, cookie);

    // M1: the name and the roles are read fresh at each request, S15.
    let r = get_me(lab.addr(), &[&cookie]).await;
    assert_eq!(r.status, 200, "{}", r.head);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(json(&r)["displayName"], "Alice A");
    lab.sql(
        "UPDATE operators SET display_name = $1 WHERE subject = 'alice'",
        "Alice Renamed",
    )
    .await;
    lab.sql(
        "INSERT INTO operator_roles (tenant_id, operator_id, role) \
         SELECT tenant_id, id, $1 FROM operators WHERE subject = 'alice'",
        "approver",
    )
    .await;
    let r = get_me(lab.addr(), &[&cookie]).await;
    assert_eq!(json(&r)["displayName"], "Alice Renamed");
    assert_eq!(
        json(&r)["roles"],
        serde_json::json!(["admin", "approver", "viewer"])
    );
    assert_eq!(
        json(&get_me(lab.addr(), &[&bob_cookie]).await)["username"],
        "bob"
    );
    // S16: HEAD on a GET route is answered as GET, without a body.
    let head = exchange(
        lab.addr(),
        &build(
            lab.addr(),
            "HEAD",
            "/api/v1/me",
            None,
            &[format!("Cookie: {cookie}")],
            "",
        ),
    )
    .await;
    assert_eq!(head.status, 200, "{}", head.head);
    assert!(head.body.is_empty(), "{}", head.body);

    let text = log.text();
    assert!(
        text.contains("login") && text.contains(&alice),
        "the login is logged with the ID: {text}"
    );
    for secret in [PASSWORD, token, "alice@", &cookie] {
        assert!(!text.contains(secret), "the log holds {secret:?}: {text}");
    }
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Login: refusals

#[tokio::test]
async fn every_refusal_is_one_401_and_costs_a_verification_and_logs_its_reason() {
    let (log, _guard) = capture();
    let lab = Lab::new().await;
    let alice = lab.user("alice", "Alice", &[], PASSWORD).await;
    let nocred = lab.user("nocred", "No Credential", &[], PASSWORD).await;
    let gone = lab.user("gone", "Disabled", &[], PASSWORD).await;
    lab.sql(
        "DELETE FROM operator_credentials WHERE operator_id = $1::uuid",
        &nocred,
    )
    .await;
    lab.sql(
        "UPDATE operators SET disabled_at = now() WHERE id = $1::uuid",
        &gone,
    )
    .await;

    let addr = lab.addr();
    // L11 unknown (and L16: with the password the dummy hash was made from), L12 no
    // credential, L13 disabled with the right password, L15 wrong password.
    let attempts: Vec<(&str, &str, &str)> = vec![
        ("unknown_user", "nosuchuser", PASSWORD),
        ("unknown_user", "nosuchuser2", DUMMY_PASSWORD),
        ("no_credential", "nocred", PASSWORD),
        ("disabled", "gone", PASSWORD),
        ("wrong_password", "alice", "not the password"),
    ];
    let mut answers = Vec::new();
    for (reason, user, password) in &attempts {
        // The refusal waits for the hasher's only permit: it costs one verification.
        let permit = lab.hasher.hold().await;
        let pending = tokio::spawn({
            let (user, password) = ((*user).to_owned(), (*password).to_owned());
            async move { login(addr, &user, &password).await }
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!pending.is_finished(), "{reason}: answered without a hash");
        drop(permit);
        let r = pending.await.unwrap();
        assert_problem(&r, 401, reason);
        assert!(r.header("set-cookie").is_none(), "{reason}");
        let challenge = r.header("www-authenticate").expect("WWW-Authenticate");
        assert!(!challenge.to_ascii_lowercase().starts_with("basic"));
        answers.push(normalized(&r));
    }
    // Byte for byte the same answer.
    for answer in &answers {
        assert_eq!(answer, &answers[0]);
    }

    // The log says why, with the ID where there is one, and never what was typed.
    let text = log.text();
    for reason in [
        "unknown_user",
        "no_credential",
        "disabled",
        "wrong_password",
    ] {
        assert!(
            text.contains(&format!("reason=\"{reason}\"")),
            "{reason}: {text}"
        );
    }
    for (id, word) in [
        (&nocred, "no_credential"),
        (&gone, "disabled"),
        (&alice, "wrong_password"),
    ] {
        let line = text
            .lines()
            .find(|l| l.contains(&format!("reason=\"{word}\"")))
            .unwrap();
        assert!(line.contains(id.as_str()), "{word}: {line}");
    }
    for secret in [
        PASSWORD,
        DUMMY_PASSWORD,
        "not the password",
        "nosuchuser",
        "$argon2",
    ] {
        assert!(!text.contains(secret), "the log holds {secret:?}: {text}");
    }
    // A reason is logged once per 60 s: the two unknown users made one line.
    assert_eq!(text.matches("reason=\"unknown_user\"").count(), 1, "{text}");
    lab.finish().await;
}

#[tokio::test]
async fn a_stored_hash_that_is_not_an_argon2id_string_is_a_500_without_a_hash_and_keeps_the_slot() {
    let (log, _guard) = capture();
    let lab = Lab::new().await;
    let id = lab.user("alice", "Alice", &[], PASSWORD).await;
    let argon2i = "$argon2i$v=19$m=19456,t=2,p=1$bmdyLWR1bW15LXNhbHQtMDAx$\
                   CBpwRQcmMcl86Bvwd+qr4xVjgtpTvz6RD0Z8m1Z3Gm4";
    // L17, for a plain text and for another Argon2 variant.
    for stored in ["plain-text-password", argon2i] {
        lab.sql("UPDATE operator_credentials SET password_hash = $1", stored)
            .await;
        let r = login(lab.addr(), "alice", PASSWORD).await;
        assert_problem(&r, 500, stored);
        assert!(r.header("set-cookie").is_none());
        assert!(!r.body.contains(stored));
    }
    let text = log.text();
    assert!(text.contains("ERROR") && text.contains(&id), "{text}");
    assert!(
        !text.contains("plain-text-password") && !text.contains("argon2i"),
        "{text}"
    );
    assert!(!text.contains(PASSWORD));
    // The slot stays on a 500: eight more fill the ten, the next is limited.
    for i in 0..8 {
        let r = login(lab.addr(), "alice", PASSWORD).await;
        assert_eq!(r.status, 500, "{i}");
    }
    let r = login(lab.addr(), "alice", PASSWORD).await;
    assert_problem(&r, 429, "eleventh");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Login: the limiter

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eleven_concurrent_wrong_logins_are_ten_401s_and_one_429_and_a_full_limiter_needs_no_database_or_hash()
 {
    let mut lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let addr = lab.addr();
    // C1
    let tasks: Vec<_> = (0..11)
        .map(|_| tokio::spawn(async move { login(addr, "alice", "wrong password!").await }))
        .collect();
    let mut statuses = Vec::new();
    for task in tasks {
        let r = task.await.unwrap();
        if r.status == 429 {
            assert_problem(&r, 429, "limited");
            assert!((1..=60).contains(&retry_after(&r)));
        }
        statuses.push(r.status);
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        10,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        1,
        "{statuses:?}"
    );

    // L9: the right password is limited too, with no hash (the permit is held) and no database
    // (it is stopped).
    let permit = lab.hasher.hold().await;
    lab.pg.stop();
    let started = Instant::now();
    let r = login(addr, "alice", PASSWORD).await;
    assert_problem(&r, 429, "right password");
    assert!((1..=60).contains(&retry_after(&r)));
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(permit);
    lab.server.finish().await;
}

#[tokio::test]
async fn only_a_200_gives_its_slot_back_and_a_400_never_takes_one() {
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let addr = lab.addr();
    // Shape refusals come before admission: they take no slot.
    for _ in 0..12 {
        let r = login_raw(addr, "{}", None).await;
        assert_problem(&r, 400, "shape");
    }
    for i in 0..9 {
        assert_eq!(
            login(addr, "alice", "wrong password!").await.status,
            401,
            "{i}"
        );
    }
    // Three right logins take a slot each and give it back.
    for i in 0..3 {
        assert_eq!(login(addr, "alice", PASSWORD).await.status, 200, "{i}");
    }
    // The tenth failure still fits, the eleventh does not.
    assert_eq!(login(addr, "alice", "wrong password!").await.status, 401);
    assert_problem(
        &login(addr, "alice", "wrong password!").await,
        429,
        "eleventh",
    );
    lab.finish().await;
}

#[tokio::test]
async fn a_client_that_disconnects_during_the_hash_keeps_its_slot() {
    // L25
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let addr = lab.addr();
    for i in 0..9 {
        assert_eq!(
            login(addr, "alice", "wrong password!").await.status,
            401,
            "{i}"
        );
    }
    let permit = lab.hasher.hold().await;
    {
        // The right password, so that a handler that ran to its end would give the slot back.
        let raw = build(
            addr,
            "POST",
            "/api/v1/login",
            Some(&origin(addr)),
            &json_headers(None),
            &login_body("alice", PASSWORD),
        );
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The request is waiting for the hasher; the client goes away.
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(permit);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_problem(
        &login(addr, "alice", "wrong password!").await,
        429,
        "slot kept",
    );
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Login: the request's rules, over a pool that points at nothing

#[tokio::test]
async fn the_requests_own_rules_are_judged_before_any_database_access() {
    let s = dead(instruction_key()).await;
    let addr = s.addr;
    let good = login_body("alice", PASSWORD);

    // L1: the guard answers first (ADR-456).
    let host_only = |host: &str, extra: &[String]| {
        let mut raw =
            format!("POST /api/v1/login HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for h in extra {
            raw.push_str(h);
            raw.push_str("\r\n");
        }
        raw.push_str(&format!("Content-Length: {}\r\n\r\n{good}", good.len()));
        raw
    };
    let r = exchange(addr, &host_only("evil.example", &[])).await;
    assert_eq!(r.status, 421);
    let r = exchange(addr, &host_only(&format!("localhost:{}", addr.port()), &[])).await;
    assert_eq!(r.status, 307);
    let foreign = "Origin: http://evil.example".to_owned();
    let r = exchange(
        addr,
        &host_only(
            &format!("127.0.0.1:{}", addr.port()),
            &[foreign, json_headers(None).remove(0)],
        ),
    )
    .await;
    assert_problem(&r, 403, "foreign Origin");

    // L2: anything but POST.
    for method in ["GET", "HEAD", "PUT", "DELETE", "PATCH"] {
        let r = exchange(
            addr,
            &build(addr, method, "/api/v1/login", Some(&origin(addr)), &[], ""),
        )
        .await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("POST"), "{method}");
    }

    // L3: no Origin is 403 before the cookie and the body, whatever they are.
    for body in [good.as_str(), "not json", ""] {
        let raw = build(
            addr,
            "POST",
            "/api/v1/login",
            None,
            &json_headers(Some("ngr_session=x")),
            body,
        );
        assert_problem(&exchange(addr, &raw).await, 403, "no Origin");
    }

    // L4: the body.
    let big = "x".repeat(4097);
    let raw = build(
        addr,
        "POST",
        "/api/v1/login",
        Some(&origin(addr)),
        &json_headers(None),
        &big,
    );
    assert_problem(&exchange(addr, &raw).await, 413, "4097 bytes");
    let stalled = format!(
        "POST /api/v1/login HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: {}\r\n\
         Content-Type: application/json\r\nContent-Length: 10\r\n\r\n",
        addr.port(),
        origin(addr)
    );
    let started = Instant::now();
    let r = exchange(addr, &stalled).await;
    assert_problem(&r, 408, "stalled body");
    assert!(started.elapsed() >= Duration::from_secs(5));

    // L5: the Content-Type.
    let send = |headers: Vec<String>| {
        let raw = build(
            addr,
            "POST",
            "/api/v1/login",
            Some(&origin(addr)),
            &headers,
            &good,
        );
        async move { exchange(addr, &raw).await }
    };
    assert_problem(&send(vec![]).await, 400, "no Content-Type");
    assert_problem(
        &send(vec!["Content-Type: text/plain".into()]).await,
        400,
        "text/plain",
    );
    assert_problem(
        &send(vec![
            "Content-Type: application/json".into(),
            "Content-Type: application/json".into(),
        ])
        .await,
        400,
        "repeated",
    );

    // L6: the shape. Each of these would be a 503 if it reached the database.
    for (what, body) in [
        ("not json", "nope".to_owned()),
        ("array", "[]".to_owned()),
        ("empty", "{}".to_owned()),
        ("missing", r#"{"username":"a"}"#.to_owned()),
        (
            "extra",
            r#"{"username":"a","password":"b","x":"y"}"#.to_owned(),
        ),
        (
            "duplicate",
            r#"{"username":"a","username":"b","password":"b"}"#.to_owned(),
        ),
        ("number", r#"{"username":"a","password":1}"#.to_owned()),
        ("null", r#"{"username":null,"password":"b"}"#.to_owned()),
        ("trailing", format!("{good} x")),
    ] {
        assert_problem(&login_raw(addr, &body, None).await, 400, what);
    }

    // L7 and L8: no database access, no hash.
    for (what, username, password) in [
        ("empty user name", String::new(), "pw".to_owned()),
        ("257-byte user name", "u".repeat(257), "pw".to_owned()),
        ("control in user name", "a\nb".to_owned(), "pw".to_owned()),
        ("DEL in user name", "a\u{7f}b".to_owned(), "pw".to_owned()),
        ("empty password", "alice".to_owned(), String::new()),
        ("1025-byte password", "alice".to_owned(), "p".repeat(1025)),
    ] {
        let r = login_raw(addr, &login_body(&username, &password), None).await;
        assert_problem(&r, 400, what);
    }
    // The bounds themselves pass: 256 bytes, 1 byte, 1024 bytes reach the database (503).
    for (what, username, password) in [
        ("256-byte user name", "u".repeat(256), "p".to_owned()),
        ("1024-byte password", "alice".to_owned(), "p".repeat(1024)),
        ("short password", "alice".to_owned(), "p".to_owned()),
    ] {
        let r = login_raw(addr, &login_body(&username, &password), None).await;
        assert_problem(&r, 503, what);
    }
    s.finish().await;
}

#[tokio::test]
async fn a_row_read_that_fails_is_a_503_that_keeps_the_slot_and_a_full_limiter_never_reaches_it() {
    // L10 and L9.
    let (log, _guard) = capture();
    let s = dead(instruction_key()).await;
    let addr = s.addr;
    let tasks: Vec<_> = (0..10)
        .map(|_| tokio::spawn(async move { login(addr, "alice", PASSWORD).await }))
        .collect();
    for task in tasks {
        assert_problem(&task.await.unwrap(), 503, "database down");
    }
    let started = Instant::now();
    let r = login(addr, "alice", PASSWORD).await;
    assert_problem(&r, 429, "eleventh");
    assert!((1..=60).contains(&retry_after(&r)));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a limited login must not wait for the pool: {:?}",
        started.elapsed()
    );
    // The refusal is logged once, as a warning, with no user name.
    let text = log.text();
    assert_eq!(text.matches("login limited").count(), 1, "{text}");
    assert!(!text.contains("alice") && !text.contains(PASSWORD));
    assert_problem(&login(addr, "alice", PASSWORD).await, 429, "again");
    assert_eq!(log.text().matches("login limited").count(), 1, "gated");
    s.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Login: the cookie it carries

#[tokio::test]
async fn a_login_replaces_a_presented_session_and_a_failure_leaves_it() {
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let addr = lab.addr();
    let first = cookie_of(&login(addr, "alice", PASSWORD).await);

    // L21: a failure with the cookie leaves the cookie and its session.
    let r = login_raw(addr, &login_body("alice", "wrong password!"), Some(&first)).await;
    assert_problem(&r, 401, "wrong password");
    assert!(r.header("set-cookie").is_none());
    let r = login_raw(addr, "{}", Some(&first)).await;
    assert_problem(&r, 400, "bad shape");
    assert!(r.header("set-cookie").is_none());
    assert_eq!(get_me(addr, &[&first]).await.status, 200);

    // L20: a success removes the presented session and issues a new token.
    let r = login_with_cookie(addr, "alice", &first).await;
    assert_eq!(r.status, 200);
    let second = cookie_of(&r);
    assert_ne!(second, first, "a presented token is never adopted");
    assert_signed_out(&get_me(addr, &[&first]).await, "the replaced session");
    assert_eq!(get_me(addr, &[&second]).await.status, 200);

    // L22: a malformed or unknown cookie is overwritten.
    let unknown = format!("ngr_session={}", "A".repeat(43));
    for bad in [
        "ngr_session=short",
        unknown.as_str(),
        "theme=dark; ngr_session=",
    ] {
        let r = login_with_cookie(addr, "alice", bad).await;
        assert_eq!(r.status, 200, "{bad}");
        let new = cookie_of(&r);
        assert_ne!(new, bad);
        assert_eq!(get_me(addr, &[&new]).await.status, 200);
    }
    // The earlier live session was not touched by those.
    assert_eq!(get_me(addr, &[&second]).await.status, 200);
    lab.finish().await;
}

#[tokio::test]
async fn at_sixteen_sessions_a_new_login_drops_the_least_recently_used() {
    // L24
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    lab.user("bob", "Bob", &[], PASSWORD).await;
    let addr = lab.addr();
    let bob = cookie_of(&login(addr, "bob", PASSWORD).await);
    let mut cookies = Vec::new();
    for _ in 0..16 {
        cookies.push(cookie_of(&login(addr, "alice", PASSWORD).await));
    }
    // The oldest is used, so the second is now the least recently used.
    assert_eq!(get_me(addr, &[&cookies[0]]).await.status, 200);
    let newest = cookie_of(&login(addr, "alice", PASSWORD).await);
    assert_signed_out(&get_me(addr, &[&cookies[1]]).await, "the evicted session");
    for cookie in cookies
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, c)| c)
    {
        assert_eq!(get_me(addr, &[cookie]).await.status, 200);
    }
    assert_eq!(get_me(addr, &[&newest]).await.status, 200);
    assert_eq!(
        get_me(addr, &[&bob]).await.status,
        200,
        "another operator's session stays"
    );
    lab.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_right_logins_at_once_make_two_valid_sessions() {
    // C2
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let addr = lab.addr();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { login(addr, "alice", PASSWORD).await }),
        tokio::spawn(async move { login(addr, "alice", PASSWORD).await }),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.status, b.status), (200, 200));
    let (a, b) = (cookie_of(&a), cookie_of(&b));
    assert_ne!(a, b);
    assert_eq!(get_me(addr, &[&a]).await.status, 200);
    assert_eq!(get_me(addr, &[&b]).await.status, 200);
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The session layer, against the database

#[tokio::test]
async fn a_disabled_operator_or_a_changed_credential_ends_the_session_at_its_next_request() {
    let lab = Lab::new().await;
    let addr = lab.addr();
    let alice = lab.user("alice", "Alice", &[], PASSWORD).await;
    let bob = lab.user("bob", "Bob", &[], PASSWORD).await;
    let carol = lab.user("carol", "Carol", &[], PASSWORD).await;
    let dave = lab.user("dave", "Dave", &[], PASSWORD).await;
    let erin = lab.user("erin", "Erin", &[], PASSWORD).await;
    let mut cookies = Vec::new();
    for name in ["alice", "bob", "carol", "dave", "erin"] {
        cookies.push(cookie_of(&login(addr, name, PASSWORD).await));
    }
    // M2: no session is answered by the layer's own rules.
    assert_problem(&get_me(addr, &[]).await, 401, "no cookie");

    // S12: disabled.
    local_user::disable(&lab.pool, "alice").await.unwrap();
    assert_signed_out(&get_me(addr, &[&cookies[0]]).await, "disabled");
    // The session was removed: enabling the operator again does not bring it back.
    lab.sql(
        "UPDATE operators SET disabled_at = NULL WHERE id = $1::uuid",
        &alice,
    )
    .await;
    assert_signed_out(&get_me(addr, &[&cookies[0]]).await, "enabled again");
    assert_eq!(login(addr, "alice", PASSWORD).await.status, 200);

    // S12: the operator row is gone.
    lab.sql(
        "DELETE FROM operator_credentials WHERE operator_id = $1::uuid",
        &bob,
    )
    .await;
    lab.sql("DELETE FROM operators WHERE id = $1::uuid", &bob)
        .await;
    assert_signed_out(&get_me(addr, &[&cookies[1]]).await, "operator gone");

    // S13: the credential row is gone.
    lab.sql(
        "DELETE FROM operator_credentials WHERE operator_id = $1::uuid",
        &carol,
    )
    .await;
    assert_signed_out(&get_me(addr, &[&cookies[2]]).await, "credential gone");

    // S13: a new password, even the same text under a new salt, ends the session.
    local_user::set_password(
        &lab.pool,
        "dave",
        &Hasher::new().hash(PASSWORD.to_owned()).await.unwrap(),
    )
    .await
    .unwrap();
    assert_signed_out(&get_me(addr, &[&cookies[3]]).await, "new password");
    assert_eq!(login(addr, "dave", PASSWORD).await.status, 200);

    // An unrelated change keeps the session: no timestamps are compared.
    lab.sql(
        "UPDATE operator_credentials SET updated_at = now() WHERE operator_id = $1::uuid",
        &erin,
    )
    .await;
    assert_eq!(get_me(addr, &[&cookies[4]]).await.status, 200);
    let _ = dave;
    lab.finish().await;
}

#[tokio::test]
async fn a_logout_during_the_row_read_makes_the_request_a_401_and_the_session_is_not_put_back() {
    // C5 and S14.
    let lab = Lab::new().await;
    let addr = lab.addr();
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let cookie = cookie_of(&login(addr, "alice", PASSWORD).await);

    // The row read waits for this lock.
    let blocker = lab.pg.pool();
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query("LOCK TABLE operators IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let pending = tokio::spawn({
        let cookie = cookie.clone();
        async move { get_me(addr, &[&cookie]).await }
    });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!pending.is_finished(), "the row read did not wait");
    let r = post_logout(addr, std::slice::from_ref(&cookie)).await;
    assert_eq!(r.status, 204);
    tx.commit().await.unwrap();
    assert_problem(&pending.await.unwrap(), 401, "logged out during the read");
    // Not re-inserted by the refresh.
    assert_signed_out(&get_me(addr, &[&cookie]).await, "after");
    lab.finish().await;
}

#[tokio::test]
async fn a_disable_or_a_new_password_during_a_login_still_gets_its_200_and_the_first_request_is_refused()
 {
    // C4
    let lab = Lab::new().await;
    let addr = lab.addr();
    lab.user("alice", "Alice", &[], PASSWORD).await;
    lab.user("bob", "Bob", &[], PASSWORD).await;
    for (name, change) in [("alice", "disable"), ("bob", "password")] {
        let permit = lab.hasher.hold().await;
        let pending = tokio::spawn(async move { login(addr, name, PASSWORD).await });
        // The row is read; the hash waits for the permit.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!pending.is_finished());
        if change == "disable" {
            local_user::disable(&lab.pool, name).await.unwrap();
        } else {
            let hash = Hasher::new()
                .hash("a different password".to_owned())
                .await
                .unwrap();
            local_user::set_password(&lab.pool, name, &hash)
                .await
                .unwrap();
        }
        drop(permit);
        let r = pending.await.unwrap();
        assert_eq!(r.status, 200, "{name}: {}", r.head);
        let cookie = cookie_of(&r);
        assert_signed_out(&get_me(addr, &[&cookie]).await, name);
    }
    lab.finish().await;
}

#[tokio::test]
async fn a_session_expires_at_the_idle_and_the_absolute_limit_across_jumps_of_the_clock() {
    // K5 at the route.
    let lab = Lab::new().await;
    let addr = lab.addr();
    lab.user("alice", "Alice", &[], PASSWORD).await;
    let idle = cookie_of(&login(addr, "alice", PASSWORD).await);
    let busy = cookie_of(&login(addr, "alice", PASSWORD).await);
    jump(29 * 60).await;
    assert_eq!(get_me(addr, &[&busy]).await.status, 200);
    jump(2 * 60).await;
    // `idle` went 31 minutes unused; `busy` was used 2 minutes ago.
    assert_signed_out(&get_me(addr, &[&idle]).await, "idle");
    assert_eq!(get_me(addr, &[&busy]).await.status, 200);
    // Kept busy every 29 minutes, the session still ends 8 hours after its login.
    let mut elapsed = 31 * 60;
    while elapsed + 29 * 60 < 8 * 3600 {
        jump(29 * 60).await;
        elapsed += 29 * 60;
        assert_eq!(get_me(addr, &[&busy]).await.status, 200, "{elapsed} s");
    }
    jump(8 * 3600 - elapsed).await;
    assert_signed_out(&get_me(addr, &[&busy]).await, "absolute");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Logout

#[tokio::test]
async fn a_logout_removes_every_session_presented_and_logs_the_id() {
    let (log, _guard) = capture();
    let lab = Lab::new().await;
    let addr = lab.addr();
    let id = lab.user("alice", "Alice", &[], PASSWORD).await;
    let a = cookie_of(&login(addr, "alice", PASSWORD).await);
    let b = cookie_of(&login(addr, "alice", PASSWORD).await);
    let c = cookie_of(&login(addr, "alice", PASSWORD).await);
    let d = cookie_of(&login(addr, "alice", PASSWORD).await);

    // O1: one session; 204, no-store, cookie cleared, session gone.
    let r = post_logout(addr, std::slice::from_ref(&d)).await;
    assert_eq!(r.status, 204, "{}", r.head);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("set-cookie"), Some(CLEARED));
    assert!(r.body.is_empty());
    assert_signed_out(&get_me(addr, &[&d]).await, "logged out");
    assert!(log.text().contains("logout") && log.text().contains(&id));

    // O3: several cookies, in one header and across headers, with a malformed one between.
    let r = post_logout(
        addr,
        &[format!("{a}; theme=x; ngr_session=zzz; {b}"), c.clone()],
    )
    .await;
    assert_eq!(r.status, 204);
    for cookie in [&a, &b, &c] {
        assert_signed_out(&get_me(addr, &[cookie]).await, "removed");
    }
    // A second logout is still 204 (O2, with a session that is gone).
    assert_eq!(post_logout(addr, &[a]).await.status, 204);
    assert!(!log.text().contains(&c));
    lab.finish().await;
}

#[tokio::test]
async fn a_logout_needs_no_database_and_no_session_and_has_its_own_rules() {
    let s = dead(instruction_key()).await;
    let addr = s.addr;
    // O2: no session, and no database to ask: still 204 with the cookie cleared.
    for cookies in [
        vec![],
        vec!["ngr_session=short".to_owned()],
        vec!["x=y".to_owned()],
    ] {
        let r = post_logout(addr, &cookies).await;
        assert_eq!(r.status, 204, "{cookies:?}");
        assert_eq!(r.header("set-cookie"), Some(CLEARED));
        assert_eq!(r.header("cache-control"), Some("no-store"));
    }
    // O4: no Origin.
    let raw = build(addr, "POST", "/api/v1/logout", None, &[], "");
    assert_problem(&exchange(addr, &raw).await, 403, "no Origin");
    // O5: a body.
    let raw = build(
        addr,
        "POST",
        "/api/v1/logout",
        Some(&origin(addr)),
        &[],
        "x",
    );
    assert_problem(&exchange(addr, &raw).await, 400, "body");
    let raw = build(
        addr,
        "POST",
        "/api/v1/logout",
        Some(&origin(addr)),
        &[],
        &"x".repeat(4097),
    );
    assert_problem(&exchange(addr, &raw).await, 413, "large body");
    // O6: another method.
    for method in ["GET", "PUT", "DELETE"] {
        let raw = build(addr, method, "/api/v1/logout", Some(&origin(addr)), &[], "");
        let r = exchange(addr, &raw).await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("POST"));
    }
    s.finish().await;
}

#[tokio::test]
async fn the_whoami_route_and_the_layer_answer_404_405_and_401_in_that_order() {
    // S1, S2 (the probe route in the library's own tests covers the non-GET Origin rule, since
    // `/me` takes GET only), S4, M2 over the wire.
    let s = dead(instruction_key()).await;
    let addr = s.addr;
    let r = exchange(addr, &build(addr, "GET", "/api/v1/nowhere", None, &[], "")).await;
    assert_problem(&r, 404, "unknown path");
    for method in ["POST", "PUT", "DELETE"] {
        // No Origin and no cookie: the method is judged first.
        let r = exchange(addr, &build(addr, method, "/api/v1/me", None, &[], "")).await;
        assert_eq!(r.status, 405, "{method}");
        assert!(r.header("allow").unwrap().contains("GET"));
    }
    // M2: no session is a plain 401 without Set-Cookie.
    let r = get_me(addr, &[]).await;
    assert_problem(&r, 401, "no session");
    assert!(r.header("set-cookie").is_none());
    assert!(
        !r.header("www-authenticate")
            .unwrap()
            .to_ascii_lowercase()
            .starts_with("basic")
    );
    // A cookie of the right shape but unknown: cleared (S8); malformed (S5).
    let unknown = format!("ngr_session={}", "A".repeat(43));
    assert_signed_out(&get_me(addr, &[&unknown]).await, "unknown token");
    assert_signed_out(
        &get_me(addr, &["ngr_session=short"]).await,
        "malformed token",
    );
    // S6: repeated, across headers.
    assert_signed_out(&get_me(addr, &[&unknown, &unknown]).await, "repeated");
    s.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The commands' database operations

#[tokio::test]
async fn the_role_list_in_code_is_the_check_in_the_migration() {
    let lab = Lab::new().await;
    let definition = lab
        .pg
        .scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'operator_roles'::regclass AND contype = 'c'",
        )
        .await;
    let mut in_check: Vec<&str> = definition
        .split('\'')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, word)| word)
        .collect();
    in_check.sort_unstable();
    let mut in_code = ROLES.to_vec();
    in_code.sort_unstable();
    assert_eq!(in_check, in_code, "{definition}");
    lab.finish().await;
}

#[tokio::test]
async fn add_creates_the_tenant_once_the_operator_its_roles_and_the_credential_in_one_transaction()
{
    let lab = Lab::new().await;
    assert_eq!(lab.pg.scalar("SELECT count(*) FROM tenants").await, "0");
    let a = lab
        .user("alice", "Alice A", &["viewer", "admin"], PASSWORD)
        .await;
    let b = lab.user("bob", "Bob B", &[], PASSWORD).await;
    assert_ne!(a, b);
    let tenant = |sql: &str| format!("SELECT {sql}");
    assert_eq!(lab.pg.scalar(&tenant("count(*) FROM tenants")).await, "1");
    assert_eq!(
        lab.pg
            .scalar(&format!(
                "SELECT id::text || '/' || name FROM tenants WHERE id = '{LOCAL_TENANT_ID}'"
            ))
            .await,
        format!("{LOCAL_TENANT_ID}/local")
    );
    assert_eq!(
        lab.pg
            .scalar(&format!(
                "SELECT count(*) FROM operators WHERE tenant_id = '{LOCAL_TENANT_ID}'"
            ))
            .await,
        "2"
    );
    assert_eq!(
        lab.pg
            .scalar(
                "SELECT string_agg(role, ',' ORDER BY role) FROM operator_roles r \
                 JOIN operators o ON o.id = r.operator_id WHERE o.subject = 'alice'"
            )
            .await,
        "admin,viewer"
    );
    assert_eq!(
        lab.pg
            .scalar("SELECT count(*) FROM operator_roles r JOIN operators o ON o.id = r.operator_id WHERE o.subject = 'bob'")
            .await,
        "0"
    );
    assert_eq!(
        lab.pg
            .scalar("SELECT count(*) FROM operator_credentials WHERE password_hash LIKE '$argon2id$v=19$m=19456,t=2,p=1$%'")
            .await,
        "2"
    );
    assert_eq!(
        lab.pg
            .scalar("SELECT count(*) FROM operators WHERE disabled_at IS NOT NULL")
            .await,
        "0"
    );
    lab.finish().await;
}

#[tokio::test]
async fn adding_an_existing_user_writes_nothing_and_one_of_two_adds_at_once_loses() {
    // U8
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &["admin"], PASSWORD).await;
    let before = lab
        .pg
        .scalar("SELECT (SELECT count(*) FROM operators) || '/' || (SELECT count(*) FROM operator_roles) || '/' || (SELECT count(*) FROM operator_credentials)")
        .await;
    let again = add_user(&lab.pool, "alice", "Other", &["viewer"], "another password").await;
    assert!(matches!(again, Err(UserError::AlreadyExists)), "{again:?}");
    assert_eq!(
        lab.pg
            .scalar("SELECT (SELECT count(*) FROM operators) || '/' || (SELECT count(*) FROM operator_roles) || '/' || (SELECT count(*) FROM operator_credentials)")
            .await,
        before
    );
    // The same subject in another tenant is a different user: the local tenant is the scope.
    lab.sql(
        "WITH t AS (INSERT INTO tenants (id, name) VALUES (gen_random_uuid(), $1) RETURNING id) \
         INSERT INTO operators (id, tenant_id, subject, display_name) \
         SELECT gen_random_uuid(), id, 'zed', 'Zed' FROM t",
        "other",
    )
    .await;
    assert!(
        add_user(&lab.pool, "zed", "Zed", &[], PASSWORD)
            .await
            .is_ok()
    );
    // Two adds at once, on a database with no local tenant yet.
    let fresh = Lab::new().await;
    let (x, y) = tokio::join!(
        add_user(&fresh.pool, "carol", "Carol", &["viewer"], PASSWORD),
        add_user(&fresh.pool, "carol", "Carol", &["viewer"], PASSWORD),
    );
    let outcomes = [x.is_ok(), y.is_ok()];
    assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1, "{outcomes:?}");
    let loser = if x.is_ok() { y } else { x };
    assert!(matches!(loser, Err(UserError::AlreadyExists)), "{loser:?}");
    assert_eq!(fresh.pg.scalar("SELECT count(*) FROM tenants").await, "1");
    assert_eq!(fresh.pg.scalar("SELECT count(*) FROM operators").await, "1");
    assert_eq!(
        fresh.pg.scalar("SELECT count(*) FROM operator_roles").await,
        "1"
    );
    fresh.finish().await;
    lab.finish().await;
}

#[tokio::test]
async fn set_password_replaces_or_creates_the_credential_and_refuses_unknown_and_disabled_users() {
    // U10
    let lab = Lab::new().await;
    let id = lab.user("alice", "Alice", &[], PASSWORD).await;
    lab.user("gone", "Gone", &[], PASSWORD).await;
    lab.sql(
        "UPDATE operator_credentials SET updated_at = '2000-01-01T00:00:00Z'",
        "",
    )
    .await;
    let new = Hasher::new()
        .hash("a brand new password".to_owned())
        .await
        .unwrap();
    assert_eq!(
        local_user::set_password(&lab.pool, "alice", &new)
            .await
            .unwrap(),
        id
    );
    assert_eq!(
        lab.pg
            .scalar(&format!(
                "SELECT (password_hash = '{new}') || '/' || (updated_at > now() - interval '1 minute') \
                 FROM operator_credentials WHERE operator_id = '{id}'"
            ))
            .await,
        "true/true"
    );
    assert_eq!(login(lab.addr(), "alice", PASSWORD).await.status, 401);
    assert_eq!(
        login(lab.addr(), "alice", "a brand new password")
            .await
            .status,
        200
    );
    // A missing credential row is created.
    lab.sql(
        "DELETE FROM operator_credentials WHERE operator_id = $1::uuid",
        &id,
    )
    .await;
    local_user::set_password(&lab.pool, "alice", &new)
        .await
        .unwrap();
    assert_eq!(
        lab.pg
            .scalar("SELECT count(*) FROM operator_credentials")
            .await,
        "2"
    );
    // Unknown, and disabled.
    assert!(matches!(
        local_user::set_password(&lab.pool, "nobody", &new).await,
        Err(UserError::Unknown)
    ));
    local_user::disable(&lab.pool, "gone").await.unwrap();
    let before = lab.pg.scalar("SELECT password_hash FROM operator_credentials o JOIN operators p ON p.id = o.operator_id WHERE p.subject = 'gone'").await;
    assert!(matches!(
        local_user::set_password(&lab.pool, "gone", &new).await,
        Err(UserError::Disabled)
    ));
    assert_eq!(
        lab.pg.scalar("SELECT password_hash FROM operator_credentials o JOIN operators p ON p.id = o.operator_id WHERE p.subject = 'gone'").await,
        before
    );
    lab.finish().await;
}

#[tokio::test]
async fn disable_sets_the_time_once_keeps_the_credential_and_is_idempotent() {
    // U11
    let lab = Lab::new().await;
    lab.user("alice", "Alice", &[], PASSWORD).await;
    assert!(matches!(
        local_user::disable(&lab.pool, "nobody").await,
        Err(UserError::Unknown)
    ));
    let first = local_user::disable(&lab.pool, "alice").await.unwrap();
    assert!(!first.already);
    assert_eq!(
        lab.pg
            .scalar("SELECT to_char(disabled_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM operators WHERE subject = 'alice'")
            .await,
        first.disabled_at
    );
    assert_eq!(
        lab.pg
            .scalar("SELECT count(*) FROM operator_credentials")
            .await,
        "1"
    );
    let stamp = lab
        .pg
        .scalar("SELECT disabled_at FROM operators WHERE subject = 'alice'")
        .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = local_user::disable(&lab.pool, "alice").await.unwrap();
    assert!(second.already);
    assert_eq!(second.disabled_at, first.disabled_at);
    assert_eq!(second.operator_id, first.operator_id);
    assert_eq!(
        lab.pg
            .scalar("SELECT disabled_at FROM operators WHERE subject = 'alice'")
            .await,
        stamp,
        "disabled_at is unchanged"
    );
    assert_eq!(login(lab.addr(), "alice", PASSWORD).await.status, 401);
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The commands as processes

mod command {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::time::{Duration, Instant};

    use server::db;

    use super::{LIMIT, PASSWORD};
    use crate::support::{TestPostgres, refused_url};

    fn write_config(dir: &Path, database_url: &str) -> PathBuf {
        let path = dir.join("ngr-server.toml");
        std::fs::write(
            &path,
            format!(
                "[instruction_key]\npath = '{}'\n\
                 [database]\nurl = \"{database_url}\"\nconnect_timeout_secs = 2\n",
                dir.join("instruction.key").display()
            ),
        )
        .unwrap();
        path
    }

    /// Runs `ngr-server user <args>` with `stdin` as its standard input, bounded by [`LIMIT`].
    pub(super) fn run(args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-server"));
        command
            .arg("user")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let id = child.id();
        if let Some(bytes) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            // The command may exit before it reads everything (an over-long input).
            let _ = pipe.write_all(bytes);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(LIMIT) {
            Ok(output) => output.unwrap(),
            Err(_) => panic!("process {id} did not exit within {LIMIT:?}"),
        }
    }

    pub(super) struct Setup {
        pub pg: TestPostgres,
        pub dir: tempfile::TempDir,
        pub config: PathBuf,
    }

    impl Setup {
        pub async fn new() -> Setup {
            let pg = TestPostgres::start();
            let pool = pg.pool();
            db::migrate_with(&pool, &db::MIGRATOR, LIMIT).await.unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = write_config(dir.path(), &pg.url());
            Setup { pg, dir, config }
        }

        pub fn add(&self, username: &str, extra: &[&str], stdin: &[u8]) -> Output {
            let config = self.config.to_str().unwrap();
            let mut args = vec![
                "add",
                "--config",
                config,
                "--username",
                username,
                "--name",
                "A Name",
            ];
            args.extend_from_slice(extra);
            run(&args, Some(stdin))
        }

        pub fn other(&self, sub: &str, username: &str, stdin: Option<&[u8]>) -> Output {
            run(
                &[
                    sub,
                    "--config",
                    self.config.to_str().unwrap(),
                    "--username",
                    username,
                ],
                stdin,
            )
        }

        pub async fn count(&self, what: &str) -> String {
            self.pg
                .scalar(&format!("SELECT count(*) FROM {what}"))
                .await
        }
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn password_line() -> Vec<u8> {
        format!("{PASSWORD}\n").into_bytes()
    }

    #[test]
    fn a_usage_error_exits_2_before_standard_input_or_the_database() {
        // U1, U2, U3. The URL points at nothing and standard input would hold a password: the
        // exit code 2 and the speed show that parsing stopped first.
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), &refused_url(&crate::support::random_password()));
        let config = config.to_str().unwrap();
        let cases: Vec<Vec<&str>> = vec![
            vec![],
            vec!["frobnicate", "--config", config, "--username", "a"],
            vec!["add", "--config", config, "--username", "a"],
            vec!["add", "--config", config, "--name", "A"],
            vec!["add", "--username", "a", "--name", "A"],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
                "--surprise",
                "1",
            ],
            vec!["add", "--config", config, "--username", "a", "--name"],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--username",
                "b",
                "--name",
                "A",
            ],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
                "--name",
                "B",
            ],
            vec![
                "add",
                "--config",
                config,
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
            ],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
                "--role",
                "root",
            ],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
                "--role",
                "Admin",
            ],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
                "--role",
                "admin",
                "--role",
                "admin",
            ],
            vec!["add", "--config", config, "--username", "A", "--name", "A"],
            vec!["add", "--config", config, "--username", "", "--name", "A"],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a b",
                "--name",
                "A",
            ],
            vec!["add", "--config", config, "--username", "a", "--name", ""],
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "a\nb",
            ],
            vec!["set-password", "--config", config],
            vec![
                "set-password",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                "A",
            ],
            vec![
                "disable",
                "--config",
                config,
                "--username",
                "a",
                "--role",
                "admin",
            ],
            vec!["disable", "--config", config, "--username", "Bad Name"],
        ];
        let long_name = "x".repeat(129);
        let long_user = "x".repeat(65);
        let more: Vec<Vec<&str>> = vec![
            vec![
                "add",
                "--config",
                config,
                "--username",
                "a",
                "--name",
                &long_name,
            ],
            vec![
                "add",
                "--config",
                config,
                "--username",
                &long_user,
                "--name",
                "A",
            ],
        ];
        for args in cases.iter().chain(more.iter()) {
            let started = Instant::now();
            let output = run(args, Some(&password_line()));
            assert_eq!(
                output.status.code(),
                Some(2),
                "{args:?}: {}",
                stderr(&output)
            );
            assert!(stderr(&output).contains("usage:"), "{args:?}");
            assert!(output.stdout.is_empty());
            assert!(started.elapsed() < Duration::from_secs(10), "{args:?}");
        }
        // A role may repeat only as different roles, and zero roles are fine (parse only).
    }

    /// U4: a terminal on standard input is refused with exit 2 and a hint, before the
    /// configuration is read. The terminal is a pseudo-terminal that util-linux's `script`
    /// provides (Linux only; a missing `script` fails the test).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_terminal_on_standard_input_is_exit_2_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), &refused_url("x"));
        for sub in ["add", "set-password"] {
            let mut line = format!(
                "{} user {sub} --config {} --username a",
                env!("CARGO_BIN_EXE_ngr-server"),
                config.display()
            );
            if sub == "add" {
                line.push_str(" --name A");
            }
            let output = Command::new("script")
                .args(["-qec", &line, "/dev/null"])
                .stdin(Stdio::null())
                .output()
                .expect("util-linux `script` is needed for this test");
            assert_eq!(output.status.code(), Some(2), "{sub}: {}", stderr(&output));
            // `script` merges the child's streams on its standard output.
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                text.contains("standard input is a terminal"),
                "{sub}: {text}"
            );
            assert!(text.contains("pipe or redirect"), "{sub}: {text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_value_that_is_not_utf8_is_a_usage_error() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), &refused_url("x"));
        let bad = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]);
        let output = Command::new(env!("CARGO_BIN_EXE_ngr-server"))
            .args(["user", "add", "--config"])
            .arg(&config)
            .arg("--username")
            .arg(bad)
            .args(["--name", "A"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }

    #[tokio::test]
    async fn add_prints_one_json_line_after_the_commit_and_the_user_can_sign_in() {
        // U9
        let s = Setup::new().await;
        let output = s.add(
            "alice",
            &["--role", "viewer", "--role", "admin"],
            &password_line(),
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let errors = stderr(&output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.matches('\n').count(), 1, "{stdout:?}");
        let line: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(line["username"], "alice");
        assert_eq!(line["roles"], serde_json::json!(["viewer", "admin"]));
        let id = line["operator_id"].as_str().unwrap();
        assert_eq!(
            s.pg.scalar(&format!(
                "SELECT subject || '/' || display_name FROM operators WHERE id = '{id}'"
            ))
            .await,
            "alice/A Name"
        );
        assert_eq!(
            s.pg.scalar("SELECT string_agg(role, ',' ORDER BY role) FROM operator_roles")
                .await,
            "admin,viewer"
        );
        assert!(!stdout.contains(PASSWORD));
        assert!(!errors.contains(PASSWORD));
        // No role is implied: none given means none stored.
        let output = s.add("bob", &[], &password_line());
        assert_eq!(output.status.code(), Some(0));
        let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(line["roles"], serde_json::json!([]));
        assert_eq!(s.count("operator_roles").await, "2");
        // The command never applied a migration nor touched the key file.
        assert!(!s.dir.path().join("instruction.key").exists());
    }

    #[tokio::test]
    async fn a_password_that_breaks_the_rules_exits_1_and_writes_nothing() {
        // U5 and U6.
        let s = Setup::new().await;
        let bom = "\u{feff}twelve chars!".as_bytes().to_vec();
        let too_long = "x".repeat(1025).into_bytes();
        let endless_ish = "x".repeat(100_000).into_bytes();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("newline only", b"\n".to_vec()),
            ("eleven", b"elevenchars\n".to_vec()),
            ("not utf-8", b"twelve \xff\xfe chars\n".to_vec()),
            ("byte order mark", bom),
            (
                "second line",
                b"twelve chars!!\nsecond line here\n".to_vec(),
            ),
            ("blank second line", b"twelve chars!!\n\n".to_vec()),
            ("control", b"twelve\tchars!!\n".to_vec()),
            ("carriage return", b"twelve chars!!\r".to_vec()),
            ("over 1024", too_long),
            ("far over", endless_ish),
        ];
        for (what, input) in &cases {
            let output = s.add("alice", &[], input);
            assert_eq!(output.status.code(), Some(1), "{what}: {}", stderr(&output));
            assert!(output.stdout.is_empty(), "{what}");
            assert!(
                !stderr(&output).contains("twelve"),
                "{what}: the password is echoed"
            );
            let output = s.other("set-password", "alice", Some(input));
            assert_eq!(output.status.code(), Some(1), "{what}");
        }
        assert_eq!(s.count("operators").await, "0");
        assert_eq!(s.count("tenants").await, "0");
        // What is accepted: CRLF, no newline at all, exactly twelve and exactly 1024.
        for (i, input) in [
            b"twelve chars\r\n".to_vec(),
            b"twelve chars".to_vec(),
            "x".repeat(1024).into_bytes(),
            format!("{}\r\n", "y".repeat(1024)).into_bytes(),
            " leading and trailing space ".as_bytes().to_vec(),
        ]
        .iter()
        .enumerate()
        {
            let output = s.add(&format!("u{i}"), &[], input);
            assert_eq!(output.status.code(), Some(0), "{i}: {}", stderr(&output));
        }
        // Nothing but one trailing newline is trimmed: the stored hash is of the exact text.
        let lab_hash = s.pg.scalar("SELECT password_hash FROM operator_credentials c JOIN operators o ON o.id = c.operator_id WHERE o.subject = 'u4'").await;
        assert_eq!(
            server::local_user::verify_password(&lab_hash, " leading and trailing space "),
            server::local_user::Verdict::Match
        );
        let hash = s.pg.scalar("SELECT password_hash FROM operator_credentials c JOIN operators o ON o.id = c.operator_id WHERE o.subject = 'u0'").await;
        assert_eq!(
            server::local_user::verify_password(&hash, "twelve chars"),
            server::local_user::Verdict::Match
        );
    }

    #[tokio::test]
    async fn the_configuration_the_database_and_the_migrations_are_checked_like_registration_code()
    {
        // U7
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nothing.toml");
        let output = run(
            &[
                "add",
                "--config",
                missing.to_str().unwrap(),
                "--username",
                "a",
                "--name",
                "A",
            ],
            Some(&password_line()),
        );
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(output.stdout.is_empty());

        // A database that is down: the URL and password are not shown.
        let secret = crate::support::random_password();
        let config = write_config(dir.path(), &refused_url(&secret));
        for sub in ["add", "set-password", "disable"] {
            let mut args = vec![sub, "--config", config.to_str().unwrap(), "--username", "a"];
            if sub == "add" {
                args.extend(["--name", "A"]);
            }
            let stdin = (sub != "disable").then(password_line);
            let output = run(&args, stdin.as_deref());
            assert_eq!(output.status.code(), Some(1), "{sub}");
            assert!(output.stdout.is_empty());
            let text = stderr(&output);
            assert!(text.contains("cannot connect to the database"), "{text}");
            assert!(
                !text.contains(&secret) && !text.contains("postgres://"),
                "{text}"
            );
        }

        // Never migrated: refused and the migrations table is not created.
        let pg = TestPostgres::start();
        let config = write_config(dir.path(), &pg.url());
        let output = run(
            &[
                "disable",
                "--config",
                config.to_str().unwrap(),
                "--username",
                "a",
            ],
            None,
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("not been migrated"));
        assert_eq!(
            pg.scalar("SELECT to_regclass('_sqlx_migrations')").await,
            ""
        );

        // Behind, ahead and changed.
        let pool = pg.pool();
        db::migrate_with(&pool, &db::MIGRATOR, LIMIT).await.unwrap();
        let last = db::MIGRATOR.iter().map(|m| m.version).max().unwrap();
        let attempt = || {
            run(
                &[
                    "add",
                    "--config",
                    config.to_str().unwrap(),
                    "--username",
                    "a",
                    "--name",
                    "A",
                ],
                Some(&password_line()),
            )
        };
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
            .bind(last)
            .execute(&pool)
            .await
            .unwrap();
        let behind = attempt();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES ($1, 'restored', true, decode($2, 'hex'), 0)",
        )
        .bind(last)
        .bind(
            db::MIGRATOR
                .iter()
                .find(|m| m.version == last)
                .unwrap()
                .checksum
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (9999, 'newer', true, decode('00', 'hex'), 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let ahead = attempt();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 9999")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') WHERE version = 1")
            .execute(&pool)
            .await
            .unwrap();
        let changed = attempt();
        for (name, output) in [("behind", behind), ("ahead", ahead), ("changed", changed)] {
            assert_eq!(output.status.code(), Some(1), "{name}");
            assert!(output.stdout.is_empty(), "{name}");
            assert!(
                stderr(&output).contains("migrations are not those"),
                "{name}"
            );
        }
        assert_eq!(pg.scalar("SELECT count(*) FROM operators").await, "0");
    }

    #[tokio::test]
    async fn adding_an_existing_user_exits_1_and_two_adds_at_once_leave_one_winner() {
        // U8
        let s = Setup::new().await;
        assert_eq!(
            s.add("alice", &["--role", "admin"], &password_line())
                .status
                .code(),
            Some(0)
        );
        let again = s.add("alice", &["--role", "viewer"], b"another password\n");
        assert_eq!(again.status.code(), Some(1));
        assert!(again.stdout.is_empty());
        assert!(
            stderr(&again).contains("already exists"),
            "{}",
            stderr(&again)
        );
        assert_eq!(s.count("operators").await, "1");
        assert_eq!(s.count("operator_roles").await, "1");

        // Two at once.
        let config = s.config.clone();
        let spawn = |name: &'static str| {
            let config = config.clone();
            std::thread::spawn(move || {
                run(
                    &[
                        "add",
                        "--config",
                        config.to_str().unwrap(),
                        "--username",
                        name,
                        "--name",
                        "N",
                    ],
                    Some(&password_line()),
                )
            })
        };
        let (a, b) = (spawn("bob"), spawn("bob"));
        let (a, b) = (a.join().unwrap(), b.join().unwrap());
        let codes = [a.status.code(), b.status.code()];
        assert!(
            codes.contains(&Some(0)) && codes.contains(&Some(1)),
            "{codes:?}"
        );
        let loser = if a.status.code() == Some(1) { &a } else { &b };
        assert!(
            stderr(loser).contains("already exists"),
            "{}",
            stderr(loser)
        );
        assert!(loser.stdout.is_empty());
        assert_eq!(s.count("operators").await, "2");
    }

    #[tokio::test]
    async fn set_password_and_disable_print_their_lines_and_follow_their_rules() {
        // U10 and U11 as processes.
        let s = Setup::new().await;
        assert_eq!(s.add("alice", &[], &password_line()).status.code(), Some(0));
        let before =
            s.pg.scalar("SELECT password_hash FROM operator_credentials")
                .await;

        let output = s.other("set-password", "alice", Some(b"a new password!\n"));
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let line: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(line["username"], "alice");
        assert!(line["operator_id"].is_string());
        assert_ne!(
            s.pg.scalar("SELECT password_hash FROM operator_credentials")
                .await,
            before
        );

        let output = s.other("set-password", "nobody", Some(&password_line()));
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let output = s.other("disable", "nobody", None);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());

        let output = s.other("disable", "alice", None);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let first: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(first["already_disabled"], false);
        assert_eq!(s.count("operator_credentials").await, "1");
        let output = s.other("set-password", "alice", Some(&password_line()));
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("disabled"));
        // Again: exit 0, the time unchanged, and the line says so.
        let output = s.other("disable", "alice", None);
        assert_eq!(output.status.code(), Some(0));
        let second: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(second["already_disabled"], true);
        assert_eq!(second["disabled_at"], first["disabled_at"]);
        assert_eq!(second["operator_id"], first["operator_id"]);
    }

    #[tokio::test]
    async fn a_failed_write_to_standard_output_exits_1_after_the_commit() {
        // U12
        let s = Setup::new().await;
        let mut command = Command::new(env!("CARGO_BIN_EXE_ngr-server"));
        command
            .args(["user", "add", "--config"])
            .arg(&s.config)
            .args(["--username", "alice", "--name", "A"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        // The reader is gone before the command has connected, let alone written.
        drop(child.stdout.take());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&password_line())
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let output = rx.recv_timeout(LIMIT).expect("the command exits").unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr(&output).contains("standard output"),
            "{}",
            stderr(&output)
        );
        assert_eq!(s.count("operators").await, "1", "the change is committed");
    }
}

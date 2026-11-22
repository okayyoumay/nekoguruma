//! The IR document store route (`PUT /api/v1/documents/{sha256}`, ADR-500) against a throwaway
//! PostgreSQL. The case names in the comments (R1 to R24) are the rows of the route's case table
//! in ADR-500. The object store is the upload tests' wrapper: with no real store behind it
//! where a test scripts a failure or counts calls, over the real pinned RustFS
//! (`NGR_TEST_RUSTFS_BIN`) where the bytes in the store matter (R18, R24). Operators are made by
//! SQL and given a session in the server's table, since the login route serves the local tenant
//! only and these tests need the agent's tenant and a second one. Digests, keys and tokens are
//! made at run time; no assertion message prints one.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use diag_ir::IR_SCHEMA_VERSION;
use diag_ir::declaration::{EcuDocumentT, EcuVariantT, encode_ecu_document};
use server::documents::{MAX_BYTES, MAX_CONCURRENT};
use server::local_guard::pinned_origin;
use server::operator_session::{COOKIE_NAME, Holder, fingerprint};
use server::storage::{MultipartStore, ObjectKey, StoreError};
use sha2::{Digest, Sha256};
use shared_crypto::random_32;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::downloads::{get, url_of};
use crate::redeem::{capture, json};
use crate::serving::{Reply, host_header};
use crate::storage::content;
use crate::uploads::{Fail, Fx, With, begin_for, hex};

// ---------------------------------------------------------------------------------------------
// Documents

/// A valid document whose bytes differ with `tag`.
pub(crate) fn doc(tag: &str) -> Vec<u8> {
    let mut variant = EcuVariantT::default();
    variant.short_name = Some(format!("Variant {tag}"));
    variant.part_numbers = Some(vec![format!("PN-{tag}"), "PN-2".to_owned()]);
    variant.sw_versions = Some(vec!["1.0".to_owned()]);
    encoded(variant, Vec::new())
}

fn encoded(variant: EcuVariantT, procedure: Vec<u8>) -> Vec<u8> {
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("odx".to_owned());
    document.source_digest = Some("cd".repeat(32));
    document.variant = Some(Box::new(variant));
    document.procedure = Some(procedure);
    encode_ecu_document(&document)
}

/// A valid document of exactly `total` bytes: its procedure part is padded until it fits.
pub(crate) fn document_of(total: usize) -> Vec<u8> {
    let mut variant = EcuVariantT::default();
    variant.short_name = Some("Large".to_owned());
    let base = encoded(variant.clone(), Vec::new()).len();
    let mut pad = total - base;
    loop {
        let bytes = encoded(variant.clone(), vec![7; pad]);
        match bytes.len().cmp(&total) {
            std::cmp::Ordering::Equal => return bytes,
            std::cmp::Ordering::Greater => pad -= bytes.len() - total,
            std::cmp::Ordering::Less => pad += total - bytes.len(),
        }
    }
}

pub(crate) fn name_of(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------------------------
// The lab

pub(crate) struct Operator {
    id: String,
    tenant: String,
    pub(crate) cookie: String,
}

struct Lab {
    fx: Fx,
    /// The agent's tenant.
    tenant: String,
    /// A worker of that tenant with an empty scope.
    worker: Operator,
}

impl Lab {
    async fn new(with: With) -> Lab {
        Lab::tuned(with, |backend| backend).await
    }

    async fn tuned(
        with: With,
        tune: impl FnOnce(server::uploads::Backend) -> server::uploads::Backend,
    ) -> Lab {
        let fx = Fx::tuned(with, tune).await;
        let tenant = fx
            .sql(&format!(
                "SELECT tenant_id::text FROM agents WHERE id = '{}'",
                fx.u.id
            ))
            .await;
        let worker = operator(&fx, &tenant, "wanda", &[("worker", "{}")]).await;
        Lab { fx, tenant, worker }
    }

    async fn finish(self) {
        self.fx.finish().await;
    }

    /// The rows of `ir_documents`.
    async fn rows(&self) -> String {
        self.fx.sql("SELECT count(*) FROM ir_documents").await
    }

    /// The `documentStored` rows of `audit_log`.
    async fn audits(&self) -> String {
        self.fx
            .sql("SELECT count(*) FROM audit_log WHERE action = 'documentStored'")
            .await
    }

    /// The calls the store has had: begin, put_part, complete, delete.
    fn calls(&self) -> [usize; 4] {
        let wrap = &self.fx.wrap;
        [
            wrap.calls()[0],
            wrap.puts(),
            wrap.completes(),
            wrap.calls()[3],
        ]
    }

    async fn store(&self, bytes: &[u8]) -> Reply {
        put_as(&self.fx, &self.worker, &name_of(bytes), bytes).await
    }

    /// Nothing was stored and nothing was asked of the store.
    async fn assert_untouched(&self, what: &str) {
        assert_eq!(self.rows().await, "0", "{what}: rows");
        assert_eq!(self.audits().await, "0", "{what}: audit rows");
        assert_eq!(self.calls(), [0; 4], "{what}: store calls");
    }
}

/// An operator of `tenant` with the roles `(role, scope)` and a session in the server's table.
async fn operator(fx: &Fx, tenant: &str, name: &str, roles: &[(&str, &str)]) -> Operator {
    operator_at(&fx.u.w.pool, &fx.u.operator_sessions, tenant, name, roles).await
}

/// [`operator`] for a server that is not the upload fixture's: `sessions` is that server's table.
pub(crate) async fn operator_at(
    pool: &sqlx::PgPool,
    sessions: &server::OperatorSessions,
    tenant: &str,
    name: &str,
    roles: &[(&str, &str)],
) -> Operator {
    let id: String = sqlx::query_scalar(
        "INSERT INTO operators (id, tenant_id, subject, display_name) \
         VALUES (gen_random_uuid(), $1::uuid, $2, $2) RETURNING id::text",
    )
    .bind(tenant)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    for (role, scope) in roles {
        sqlx::query(
            "INSERT INTO operator_roles (tenant_id, operator_id, role, scope) \
             VALUES ($1::uuid, $2::uuid, $3, $4::jsonb)",
        )
        .bind(tenant)
        .bind(&id)
        .bind(role)
        .bind(scope)
        .execute(pool)
        .await
        .unwrap();
    }
    // The text is the login's hash in a real server; a session remembers its fingerprint.
    let hash = format!("hash-of-{name}");
    sqlx::query(
        "INSERT INTO operator_credentials (operator_id, password_hash) VALUES ($1::uuid, $2)",
    )
    .bind(&id)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    let token = random_32().unwrap();
    sessions.establish(
        &[],
        Sha256::digest(token).into(),
        Holder {
            operator: id.clone(),
            tenant: tenant.to_owned(),
            fingerprint: fingerprint(&hash),
        },
        Instant::now(),
        SystemTime::now(),
    );
    Operator {
        id,
        tenant: tenant.to_owned(),
        cookie: format!("{COOKIE_NAME}={}", URL_SAFE_NO_PAD.encode(token)),
    }
}

// ---------------------------------------------------------------------------------------------
// Requests

/// The head of a request, ending at the blank line. `origin` is the pinned one or another text;
/// `length` is the whole header line that gives the body's framing.
fn head(
    fx: &Fx,
    method: &str,
    name: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    content_types: &[&str],
    length: &str,
) -> String {
    head_at(
        fx.u.s.addr,
        method,
        name,
        cookie,
        origin,
        content_types,
        length,
    )
}

fn head_at(
    addr: SocketAddr,
    method: &str,
    name: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    content_types: &[&str],
    length: &str,
) -> String {
    let mut raw = format!(
        "{method} /api/v1/documents/{name} HTTP/1.1\r\n{}\r\nConnection: close\r\n",
        host_header(addr)
    );
    if let Some(origin) = origin {
        raw.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(cookie) = cookie {
        raw.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    for content_type in content_types {
        raw.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    raw.push_str(length);
    raw.push_str("\r\n\r\n");
    raw
}

fn pinned(fx: &Fx) -> String {
    pinned_origin(fx.u.s.addr.port())
}

const OCTETS: &[&str] = &["application/octet-stream"];

/// The head of a well-formed store of `len` bytes by `who`.
fn good_head(fx: &Fx, who: &Operator, name: &str, len: usize) -> String {
    head(
        fx,
        "PUT",
        name,
        Some(&who.cookie),
        Some(&pinned(fx)),
        OCTETS,
        &format!("Content-Length: {len}"),
    )
}

/// Sends `head` and `body` (a failed write is ignored: the server may answer and close before it
/// has read everything) and reads the answer until the server closes. With `half_close` the
/// write side is shut after the body.
async fn exchange_bytes(fx: &Fx, head: String, body: &[u8], half_close: bool) -> Reply {
    exchange_bytes_at(fx.u.s.addr, head, body, half_close).await
}

async fn exchange_bytes_at(addr: SocketAddr, head: String, body: &[u8], half_close: bool) -> Reply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let (mut read, mut write) = stream.split();
    let writing = async {
        let _ = write.write_all(head.as_bytes()).await;
        let _ = write.write_all(body).await;
        if half_close {
            let _ = write.shutdown().await;
        }
    };
    let reading = async {
        let mut buf = Vec::new();
        let _ = read.read_to_end(&mut buf).await;
        buf
    };
    let (_, buf) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(writing, reading)
    })
    .await
    .expect("the server answers and closes the connection");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    Reply {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

async fn put_as(fx: &Fx, who: &Operator, name: &str, bytes: &[u8]) -> Reply {
    exchange_bytes(fx, good_head(fx, who, name, bytes.len()), bytes, false).await
}

/// Stores `bytes` under their own name through the route of the server at `addr`, as the
/// operator whose session `cookie` is: for the tests of other routes that need a stored
/// document (the job creation's check, ADR-501).
pub(crate) async fn put_document(addr: SocketAddr, cookie: &str, bytes: &[u8]) -> Reply {
    let head = head_at(
        addr,
        "PUT",
        &name_of(bytes),
        Some(cookie),
        Some(&pinned_origin(addr.port())),
        OCTETS,
        &format!("Content-Length: {}", bytes.len()),
    );
    exchange_bytes_at(addr, head, bytes, false).await
}

/// A request that has sent its head and `declared` bytes of body it never finishes: it holds a
/// store's slot until the stream is dropped.
async fn stalled(fx: &Fx, who: &Operator, declared: usize) -> TcpStream {
    let mut stream = TcpStream::connect(fx.u.s.addr).await.unwrap();
    let head = good_head(fx, who, &"0".repeat(64), declared);
    stream.write_all(head.as_bytes()).await.unwrap();
    stream
}

/// A problem document with `status` and, when given, the `code`; it carries no digest.
fn problem(r: &Reply, status: u16, code: Option<&str>, name: &str, what: &str) {
    assert_eq!(r.status, status, "{what}: {} {}", r.head, r.body);
    assert_eq!(
        r.header("content-type"),
        Some("application/problem+json"),
        "{what}"
    );
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
    assert_eq!(json(r)["code"].as_str(), code, "{what}");
    assert!(!r.body.contains(name) && !r.head.contains(name), "{what}");
}

fn closes(r: &Reply, what: &str) {
    assert_eq!(r.header("connection"), Some("close"), "{what}");
}

/// What a store that answers 200 or 201 says.
fn stored(r: &Reply, status: u16, name: &str, what: &str) {
    assert_eq!(r.status, status, "{what}: {} {}", r.head, r.body);
    assert_eq!(r.header("content-type"), Some("application/json"), "{what}");
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
    assert!(r.header("location").is_none(), "{what}");
    assert_eq!(json(r), serde_json::json!({ "sha256": name }), "{what}");
}

/// A name that is a digest but belongs to no body of these tests.
fn other_name() -> String {
    name_of(b"another body")
}

// ---------------------------------------------------------------------------------------------
// The guard, the session, the name, the Content-Type and the bounds (R1 to R5)

// R1
#[tokio::test]
async fn the_route_is_put_only_behind_the_guard() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let name = other_name();
    for method in ["GET", "POST", "DELETE", "PATCH"] {
        let r = exchange_bytes(
            fx,
            head(
                fx,
                method,
                &name,
                Some(&lab.worker.cookie),
                Some(&pinned(fx)),
                OCTETS,
                "Content-Length: 0",
            ),
            b"",
            false,
        )
        .await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
        let allow = r.header("allow").unwrap_or_default();
        assert!(allow.contains("PUT") && !allow.contains("GET"), "{allow}");
    }
    // The guard answers before routing: another Host (421), a Host of the other loopback name
    // (a redirect) and another Origin (403), whatever the session.
    let good = good_head(fx, &lab.worker, &name, 0);
    let other_host = good.replace(&fx.u.s.host(), "Host: 127.0.0.2:1");
    let r = exchange_bytes(fx, other_host, b"", false).await;
    assert_eq!(r.status, 421, "{}", r.head);
    let localhost = good.replace(
        &fx.u.s.host(),
        &format!("Host: localhost:{}", fx.u.s.addr.port()),
    );
    let r = exchange_bytes(fx, localhost, b"", false).await;
    assert_eq!(r.status, 307, "{}", r.head);
    let foreign = good.replace(&pinned(fx), "http://evil.example");
    let r = exchange_bytes(fx, foreign, b"", false).await;
    assert_eq!(r.status, 403, "{}", r.head);
    lab.assert_untouched("R1").await;
    lab.finish().await;
}

// R2
#[tokio::test]
async fn a_store_needs_the_origin_and_a_session() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let name = other_name();
    let send = |cookie: Option<String>, origin: Option<String>| {
        let h = head(
            fx,
            "PUT",
            &name,
            cookie.as_deref(),
            origin.as_deref(),
            OCTETS,
            "Content-Length: 100",
        );
        async move { exchange_bytes(fx, h, b"", false).await }
    };
    let cookie = lab.worker.cookie.clone();
    // No Origin, or one that is not the pinned one: 403 before the cookie is read.
    let r = send(Some(cookie.clone()), None).await;
    assert_eq!(r.status, 403, "{}", r.head);
    let r = send(Some(cookie.clone()), Some("http://evil.example".to_owned())).await;
    assert_eq!(r.status, 403, "{}", r.head);
    // No cookie, a malformed one and an unknown token: 401.
    let r = send(None, Some(pinned(fx))).await;
    assert_eq!(r.status, 401, "{}", r.head);
    let r = send(Some(format!("{COOKIE_NAME}=short")), Some(pinned(fx))).await;
    assert_eq!(r.status, 401, "{}", r.head);
    let unknown = format!("{COOKIE_NAME}={}", URL_SAFE_NO_PAD.encode([9u8; 32]));
    let r = send(Some(unknown), Some(pinned(fx))).await;
    assert_eq!(r.status, 401, "{}", r.head);
    lab.assert_untouched("R2").await;
    lab.finish().await;
}

// R3
#[tokio::test]
async fn the_name_is_64_lowercase_hex_digits() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let good = other_name();
    let bad = [
        good.to_uppercase(),
        good[..63].to_owned(),
        format!("{good}0"),
        format!("g{}", &good[1..]),
        format!("sha256:{}", &good[7..]),
    ];
    for name in bad {
        // The body is announced and never sent: a route that read it first would wait.
        let r = exchange_bytes(fx, good_head(fx, &lab.worker, &name, 100), b"", false).await;
        assert_eq!(r.status, 400, "{}", r.head);
        closes(&r, "R3");
        assert_eq!(r.header("content-type"), Some("application/problem+json"));
    }
    lab.assert_untouched("R3").await;
    lab.finish().await;
}

#[tokio::test]
async fn a_name_that_does_not_decode_is_the_routes_own_400() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    for name in ["%ff", "%c3%28", "%ff%ff"] {
        let r = exchange_bytes(fx, good_head(fx, &lab.worker, name, 100), b"", false).await;
        assert_eq!(r.status, 400, "{name}: {}", r.head);
        closes(&r, "undecodable name");
        assert_eq!(r.header("content-type"), Some("application/problem+json"));
        assert_eq!(
            json(&r)["title"],
            "The name must be 64 lowercase hexadecimal digits"
        );
    }
    lab.assert_untouched("undecodable name").await;
    lab.finish().await;
}

// R4
#[tokio::test]
async fn the_body_is_an_octet_stream() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let name = other_name();
    for types in [
        vec![],
        vec!["application/json"],
        vec!["text/plain"],
        vec!["application/octet-stream", "application/octet-stream"],
    ] {
        let h = head(
            fx,
            "PUT",
            &name,
            Some(&lab.worker.cookie),
            Some(&pinned(fx)),
            &types,
            "Content-Length: 100",
        );
        let r = exchange_bytes(fx, h, b"", false).await;
        assert_eq!(r.status, 400, "{types:?}: {}", r.head);
        closes(&r, "R4");
    }
    lab.assert_untouched("R4").await;
    lab.finish().await;
}

// R5
#[tokio::test]
async fn the_bound_is_16_mib_inclusive() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    // One byte over, declared: 413 before a byte is sent.
    let r = exchange_bytes(
        fx,
        good_head(fx, &lab.worker, &other_name(), MAX_BYTES + 1),
        b"",
        false,
    )
    .await;
    problem(&r, 413, None, &other_name(), "declared over the bound");
    closes(&r, "R5");
    lab.assert_untouched("R5").await;
    // Exactly the bound passes the length rules and is judged as a document: these bytes are
    // not one.
    let body = content(5, MAX_BYTES);
    let name = name_of(&body);
    let r = put_as(fx, &lab.worker, &name, &body).await;
    problem(
        &r,
        400,
        Some("INVALID_IR_DOCUMENT"),
        &name,
        "exactly the bound",
    );
    lab.assert_untouched("R5 exactly").await;
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// Permission, the slots and the body (R6 to R8)

// R6
#[tokio::test]
async fn only_a_worker_with_an_empty_scope_stores() {
    let (logs, _guard) = capture();
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let none = operator(fx, &lab.tenant, "none", &[]).await;
    let viewer = operator(fx, &lab.tenant, "viewer", &[("viewer", "{}")]).await;
    let scoped = operator(fx, &lab.tenant, "scoped", &[("worker", r#"{"oem":["X"]}"#)]).await;
    let admin = operator(fx, &lab.tenant, "admin", &[("admin", "{}")]).await;
    let name = other_name();
    let mut first: Option<String> = None;
    for (who, what) in [
        (&none, "no role"),
        (&viewer, "viewer"),
        (&scoped, "scoped worker"),
        (&admin, "administrator"),
    ] {
        // The body is announced and never sent: it is not read before the permission is judged.
        let r = exchange_bytes(fx, good_head(fx, who, &name, 100), b"", false).await;
        problem(&r, 403, None, &name, what);
        closes(&r, what);
        let body = r.body.clone();
        assert_eq!(first.get_or_insert(body.clone()), &body, "{what}: one body");
    }
    lab.assert_untouched("R6").await;
    // The reason goes to the log, which names no digest.
    let text = logs.text();
    assert!(text.contains("the caller is not a worker"), "{text}");
    assert!(text.contains(&none.id), "the operator is named");
    assert!(!text.contains(&name));
    // A worker with an empty scope stores.
    let r = lab.store(&doc("a")).await;
    assert_eq!(r.status, 201, "{}", r.head);
    lab.finish().await;
}

// R7
#[tokio::test]
async fn a_third_concurrent_store_is_turned_away() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    assert_eq!(MAX_CONCURRENT, 2);
    let mut held: Vec<TcpStream> = Vec::new();
    // A small request that gets a slot is judged (its body does not hash to the name) and
    // gives the slot back; the two stalled ones have taken theirs when this one is turned away.
    let who = &lab.worker;
    let probe = |body: &'static [u8]| async move {
        exchange_bytes(
            fx,
            good_head(fx, who, &other_name(), body.len()),
            body,
            false,
        )
        .await
    };
    let mut turned_away = None;
    for _ in 0..200 {
        // A stalled request the server has already answered (a slow database can refuse it) holds
        // no slot: it is replaced, so that two are held when the probe is sent.
        held.retain(|stream| {
            matches!(
                stream.try_read(&mut [0u8; 1]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
            )
        });
        while held.len() < MAX_CONCURRENT {
            held.push(stalled(fx, who, 1000).await);
        }
        let r = probe(b"y").await;
        if r.status == 503 {
            turned_away = Some(r);
            break;
        }
        assert_eq!(r.status, 400, "{}", r.head);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = turned_away.expect("a third store is turned away while two are held");
    assert_eq!(r.header("retry-after"), Some("5"), "{}", r.head);
    closes(&r, "R7");
    assert_eq!(json(&r)["title"], "Too many documents are being stored");
    // The slots come back when the stalled requests end.
    drop(held);
    let mut freed = false;
    for _ in 0..200 {
        if probe(b"y").await.status == 400 {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(freed, "the slots are given back");
    lab.assert_untouched("R7").await;
    // Both slots are free: two stores at once both succeed.
    let (a, b) = (doc("slot a"), doc("slot b"));
    let (ra, rb) = tokio::join!(lab.store(&a), lab.store(&b));
    assert_eq!(
        (ra.status, rb.status),
        (201, 201),
        "{} / {}",
        ra.head,
        rb.head
    );
    lab.finish().await;
}

// R8
#[tokio::test]
async fn a_body_past_the_bound_or_the_time_is_cut() {
    let lab = Lab::tuned(With::Fake, |b| {
        b.with_document_body_timeout(Duration::from_secs(3))
    })
    .await;
    let fx = &lab.fx;
    let name = other_name();

    // It grows past the bound: no length is declared, and the answer comes with the byte that
    // passes the bound (nothing more is sent, so no input is left unread).
    let h = head(
        fx,
        "PUT",
        &name,
        Some(&lab.worker.cookie),
        Some(&pinned(fx)),
        OCTETS,
        "Transfer-Encoding: chunked",
    );
    let mut body = format!("{:x}\r\n", MAX_BYTES + 1).into_bytes();
    body.extend(std::iter::repeat_n(1u8, MAX_BYTES + 1));
    let r = exchange_bytes(fx, h, &body, false).await;
    problem(&r, 413, None, &name, "grows past the bound");
    closes(&r, "R8 413");

    // It stalls: the time budget is the route's own.
    let mut stream = TcpStream::connect(fx.u.s.addr).await.unwrap();
    stream
        .write_all(good_head(fx, &lab.worker, &name, 1000).as_bytes())
        .await
        .unwrap();
    stream.write_all(b"0123456789").await.unwrap();
    let began = Instant::now();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .expect("the server answers")
        .ok();
    let text = String::from_utf8_lossy(&buf).into_owned();
    assert!(text.starts_with("HTTP/1.1 408"), "{text}");
    assert!(text.to_ascii_lowercase().contains("connection: close"));
    assert!(
        began.elapsed() >= Duration::from_secs(2),
        "the budget is 3 seconds here"
    );

    // It breaks: the peer ends it early. Whether the answer reaches a peer that has gone is not
    // the point; the request ends and stores nothing.
    let mut stream = TcpStream::connect(fx.u.s.addr).await.unwrap();
    stream
        .write_all(good_head(fx, &lab.worker, &name, 100).as_bytes())
        .await
        .unwrap();
    stream.write_all(b"0123456789").await.unwrap();
    stream.shutdown().await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .expect("the connection ends")
        .ok();
    let text = String::from_utf8_lossy(&buf).into_owned();
    assert!(
        text.is_empty() || text.starts_with("HTTP/1.1 400"),
        "{text}"
    );
    // When the answer does arrive, its title fits an octet stream, not a JSON object.
    if !text.is_empty() {
        assert!(
            text.contains("The request body broke before its end"),
            "{text}"
        );
        assert!(!text.contains("JSON"), "{text}");
    }

    lab.assert_untouched("R8").await;
    // The slots are free again.
    let r = lab.store(&doc("after")).await;
    assert_eq!(r.status, 201, "{}", r.head);
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The document (R9 to R12)

// R9
#[tokio::test]
async fn a_body_that_does_not_hash_to_its_name_is_refused_even_when_the_name_is_stored() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let (a, b) = (doc("a"), doc("b"));
    let r = put_as(fx, &lab.worker, &name_of(&b), &a).await;
    problem(&r, 400, Some("DIGEST_MISMATCH"), &name_of(&b), "no row");
    lab.assert_untouched("R9").await;
    // With a row under the name, the answer is the same: it is not a 200.
    let r = lab.store(&b).await;
    assert_eq!(r.status, 201, "{}", r.head);
    let before = lab.calls();
    let r = put_as(fx, &lab.worker, &name_of(&b), &a).await;
    problem(&r, 400, Some("DIGEST_MISMATCH"), &name_of(&b), "a row");
    assert_eq!(lab.calls(), before);
    assert_eq!(lab.rows().await, "1");
    lab.finish().await;
}

// R10 and R23
#[tokio::test]
async fn a_second_store_answers_200_and_touches_nothing() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let bytes = doc("twice");
    let name = name_of(&bytes);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "first");
    let (calls, audits) = (lab.calls(), lab.audits().await);
    assert_eq!(calls, [1, 1, 1, 0]);
    let key = fx.sql("SELECT storage_key FROM ir_documents").await;
    let r = lab.store(&bytes).await;
    stored(&r, 200, &name, "second");
    assert_eq!(lab.calls(), calls, "no store call");
    assert_eq!(lab.audits().await, audits, "no audit row");
    assert_eq!(lab.rows().await, "1");
    assert_eq!(fx.sql("SELECT storage_key FROM ir_documents").await, key);

    // No parse: a row that exists for bytes that are no document answers 200 for them.
    let junk = b"not a document".to_vec();
    let junk_name = name_of(&junk);
    sqlx::query(
        "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
             variant_name, source_format, storage_key) \
         VALUES ($1::uuid, $2, 'ext', 1, 'v', 'odx', $3)",
    )
    .bind(&lab.tenant)
    .bind(&junk_name)
    // A key no object has: the object is lost (R23) and nothing repairs it.
    .bind(crate::uploads::v4())
    .execute(&fx.u.w.pool)
    .await
    .unwrap();
    let r = lab.store(&junk).await;
    stored(&r, 200, &junk_name, "no parse");
    assert_eq!(lab.calls(), calls, "R23: no store call, no repair");
    assert_eq!(lab.rows().await, "2");
    lab.finish().await;
}

// R11
#[tokio::test]
async fn only_a_document_the_loader_accepts_is_stored() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let valid = doc("valid");

    let mut wrong_schema = EcuDocumentT::default();
    wrong_schema.schema_version = IR_SCHEMA_VERSION + 1;
    wrong_schema.source_format = Some("odx".to_owned());
    wrong_schema.source_digest = Some("cd".repeat(32));
    wrong_schema.variant = Some(Box::new(EcuVariantT::default()));
    let mut no_variant = EcuDocumentT::default();
    no_variant.schema_version = IR_SCHEMA_VERSION;
    no_variant.source_format = Some("odx".to_owned());
    no_variant.source_digest = Some("cd".repeat(32));
    let mut wrong_identifier = valid.clone();
    wrong_identifier[4..8].copy_from_slice(b"XXXX");
    let mut flipped = valid.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    let nameless = encoded(EcuVariantT::default(), Vec::new());

    let cases: Vec<(&str, Vec<u8>)> = vec![
        // The empty body is stored under the empty hash's name: it is not a document.
        ("empty", Vec::new()),
        ("too short", vec![1, 2, 3]),
        ("wrong identifier", wrong_identifier),
        ("truncated", valid[..valid.len() / 2].to_vec()),
        ("other schema version", encode_ecu_document(&wrong_schema)),
        ("no variant", encode_ecu_document(&no_variant)),
        // A loaded document whose variant has no name (R12 on the route).
        ("no variant name", nameless),
        ("text", b"{\"a\":1}".to_vec()),
    ];
    for (what, bytes) in &cases {
        let name = name_of(bytes);
        let r = put_as(fx, &lab.worker, &name, bytes).await;
        problem(&r, 400, Some("INVALID_IR_DOCUMENT"), &name, what);
    }
    // A flipped last byte may or may not break a document; either way the answer is one of the
    // two, and a refusal stores nothing.
    let name = name_of(&flipped);
    let r = put_as(fx, &lab.worker, &name, &flipped).await;
    assert!(matches!(r.status, 201 | 400), "{}", r.head);
    let stored_rows = if r.status == 201 { "1" } else { "0" };
    assert_eq!(lab.rows().await, stored_rows);
    if r.status == 400 {
        lab.assert_untouched("R11").await;
    }
    let r = lab.store(&valid).await;
    stored(&r, 201, &name_of(&valid), "a document");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The store (R13 to R16)

// R13
#[tokio::test]
async fn without_a_store_the_route_is_unavailable() {
    let lab = Lab::new(With::Nothing).await;
    let bytes = doc("nostore");
    let r = lab.store(&bytes).await;
    problem(&r, 503, None, &name_of(&bytes), "no store");
    assert_eq!(lab.rows().await, "0");
    assert_eq!(lab.audits().await, "0");
    // The store is wanted only after the earlier rows: a body that is no document is 400 and a
    // digest the tenant holds is 200, with no store at all.
    let junk = b"no document".to_vec();
    let r = lab.store(&junk).await;
    problem(
        &r,
        400,
        Some("INVALID_IR_DOCUMENT"),
        &name_of(&junk),
        "R11 before R13",
    );
    let name = name_of(&bytes);
    sqlx::query(
        "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
             variant_name, source_format, storage_key) \
         VALUES ($1::uuid, $2, NULL, 1, 'v', 'odx', $3)",
    )
    .bind(&lab.tenant)
    .bind(&name)
    .bind(crate::uploads::v4())
    .execute(&lab.fx.u.w.pool)
    .await
    .unwrap();
    stored(&lab.store(&bytes).await, 200, &name, "R10 before R13");
    lab.finish().await;
}

// R14
#[tokio::test]
async fn a_failed_begin_stores_nothing() {
    let lab = Lab::new(With::Fake).await;
    let wrap = &lab.fx.wrap;
    let bytes = doc("begin");
    let name = name_of(&bytes);
    wrap.fail_begin(Some(Fail::Io));
    let r = lab.store(&bytes).await;
    problem(&r, 503, None, &name, "begin fails");
    wrap.fail_begin(Some(Fail::Random));
    let r = lab.store(&bytes).await;
    problem(&r, 500, None, &name, "no randomness");
    assert_eq!(lab.calls(), [2, 0, 0, 0], "nothing to delete, nothing sent");
    assert_eq!(lab.rows().await, "0");
    assert_eq!(lab.audits().await, "0");
    wrap.fail_begin(None);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "a retry");
    lab.finish().await;
}

// R15
#[tokio::test]
async fn a_failed_part_aborts_the_upload() {
    let (logs, _guard) = capture();
    let lab = Lab::new(With::Fake).await;
    let wrap = &lab.fx.wrap;
    let bytes = doc("part");
    let name = name_of(&bytes);
    wrap.fail_put(Some(Fail::Io));
    let r = lab.store(&bytes).await;
    problem(&r, 503, None, &name, "the part fails");
    assert_eq!(
        lab.calls(),
        [1, 1, 0, 1],
        "the upload is deleted, not completed"
    );
    assert_eq!(wrap.deleted(), wrap.issued(), "the key begin issued");
    assert_eq!(lab.rows().await, "0");
    assert_eq!(lab.audits().await, "0");
    // Even a delete that fails does not change the answer, and the log names neither the key
    // nor the digest.
    wrap.fail_delete(Some(Fail::Io));
    let r = lab.store(&bytes).await;
    problem(&r, 503, None, &name, "the part and the delete fail");
    let text = logs.text();
    assert!(text.contains("could not be deleted"), "{text}");
    assert!(!text.contains(&name));
    for key in wrap.issued() {
        assert!(!text.contains(&key));
    }
    wrap.fail_put(None);
    wrap.fail_delete(None);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "a retry");
    lab.finish().await;
}

// R16
#[tokio::test]
async fn no_row_without_a_read_back() {
    let lab = Lab::new(With::Fake).await;
    let wrap = &lab.fx.wrap;
    let bytes = doc("complete");
    let name = name_of(&bytes);
    // A mismatch (the store has deleted the object already), another failure, and a failure with
    // a delete that fails too: each is 503, the key is deleted again, and no row is written.
    for (fail, delete_fails) in [
        (Fail::HashMismatch, false),
        (Fail::LengthMismatch, false),
        (Fail::Io, false),
        (Fail::Io, true),
    ] {
        wrap.fail_complete(Some(fail));
        wrap.fail_delete(delete_fails.then_some(Fail::Io));
        let before = wrap.deleted().len();
        let r = lab.store(&bytes).await;
        problem(&r, 503, None, &name, "completion fails");
        assert_eq!(wrap.deleted().len(), before + 1);
        assert_eq!(lab.rows().await, "0");
        assert_eq!(lab.audits().await, "0");
    }
    assert_eq!(wrap.deleted(), wrap.issued());
    wrap.fail_complete(None);
    wrap.fail_delete(None);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "a retry");
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The record (R17 to R22)

// R17
#[tokio::test]
async fn a_stored_document_has_its_row_its_columns_and_its_audit_record() {
    let (logs, _guard) = capture();
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let bytes = doc("row");
    let name = name_of(&bytes);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "stored");

    let row = |column: &str| {
        let sql = format!("SELECT {column} FROM ir_documents WHERE digest = '{name}'");
        async move { fx.sql(&sql).await }
    };
    assert_eq!(row("tenant_id::text").await, lab.tenant);
    assert_eq!(row("extension_id IS NULL").await, "true");
    assert_eq!(row("schema_version").await, IR_SCHEMA_VERSION.to_string());
    assert_eq!(row("variant_name").await, "Variant row");
    assert_eq!(row("part_numbers::text").await, "{PN-row,PN-2}");
    assert_eq!(row("sw_versions::text").await, "{1.0}");
    assert_eq!(row("source_format").await, "odx");
    assert_eq!(row("source_digest").await, "cd".repeat(32));
    let key = fx.wrap.issued().remove(0);
    assert_eq!(row("storage_key").await, key);

    let audit = |column: &str| {
        let sql = format!("SELECT {column} FROM audit_log WHERE action = 'documentStored'");
        async move { fx.sql(&sql).await }
    };
    assert_eq!(lab.audits().await, "1");
    assert_eq!(audit("tenant_id::text").await, lab.tenant);
    assert_eq!(audit("operator_id::text").await, lab.worker.id);
    assert_eq!(audit("detail->>'digest'").await, name);
    assert_eq!(
        audit("(detail->>'size')::bigint").await,
        bytes.len().to_string()
    );
    assert_eq!(
        audit("agent_id IS NULL AND job_id IS NULL AND vin IS NULL").await,
        "true"
    );
    // The log of the store holds no digest, key or body.
    let text = logs.text();
    assert!(text.contains("document stored"), "{text}");
    assert!(!text.contains(&name) && !text.contains(&key));
    lab.finish().await;
}

// R18
#[tokio::test]
async fn two_racing_stores_leave_one_row_and_one_object() {
    let lab = Lab::new(With::Real).await;
    let fx = &lab.fx;
    let bytes = doc("race");
    let name = name_of(&bytes);
    // Both have begun their uploads before either records its row.
    fx.wrap.hold_begins(2);
    let (a, b) = tokio::join!(lab.store(&bytes), lab.store(&bytes));
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 201], "{} / {}", a.head, b.head);
    stored(if a.status == 201 { &a } else { &b }, 201, &name, "winner");
    stored(if a.status == 200 { &a } else { &b }, 200, &name, "loser");

    assert_eq!(lab.rows().await, "1");
    assert_eq!(lab.audits().await, "1", "only the winner is recorded");
    let issued = fx.wrap.issued();
    assert_eq!(issued.len(), 2);
    let kept = fx.sql("SELECT storage_key FROM ir_documents").await;
    let deleted = fx.wrap.deleted();
    assert_eq!(deleted.len(), 1, "the loser deletes its own object");
    assert_ne!(deleted[0], kept);
    assert!(issued.contains(&kept) && issued.contains(&deleted[0]));
    // One object: the row's is there, the loser's is gone.
    assert!(fx.wrap.get(&ObjectKey::parse(&kept).unwrap()).await.is_ok());
    assert!(matches!(
        fx.wrap.get(&ObjectKey::parse(&deleted[0]).unwrap()).await,
        Err(StoreError::NotFound)
    ));
    lab.finish().await;
}

// R19
#[tokio::test]
async fn a_key_another_table_holds_is_never_inserted() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let wrap = &fx.wrap;

    // The key of an artifact.
    let held = crate::uploads::v4();
    sqlx::query(
        "INSERT INTO artifacts (tenant_id, digest, size_bytes, storage_key) \
         VALUES ($1::uuid, $2, 1, $3)",
    )
    .bind(&lab.tenant)
    .bind("a".repeat(64))
    .bind(&held)
    .execute(&fx.u.w.pool)
    .await
    .unwrap();
    *wrap.begin_key.lock().unwrap() = Some(ObjectKey::parse(&held).unwrap());
    let bytes = doc("held");
    let r = lab.store(&bytes).await;
    problem(&r, 500, None, &name_of(&bytes), "an artifact's key");
    assert_eq!(lab.rows().await, "0");
    assert_eq!(lab.audits().await, "0");
    assert!(
        wrap.deleted().is_empty(),
        "the key may be another row's object"
    );

    // The key of an upload of a job (`job_objects`), made by the data channel's begin route.
    let upload_bytes = content(3, 100);
    begin_for(fx, &upload_bytes).await;
    let held = wrap.issued().pop().unwrap();
    assert_eq!(
        fx.sql(&format!(
            "SELECT count(*) FROM job_objects WHERE storage_key = '{held}'"
        ))
        .await,
        "1"
    );
    *wrap.begin_key.lock().unwrap() = Some(ObjectKey::parse(&held).unwrap());
    let r = lab.store(&bytes).await;
    problem(&r, 500, None, &name_of(&bytes), "a job object's key");
    assert_eq!(lab.rows().await, "0");
    assert!(wrap.deleted().is_empty());

    // With a fresh key the same bytes are stored.
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name_of(&bytes), "a fresh key");
    lab.finish().await;
}

// R19, the key of a document
#[tokio::test]
async fn a_key_another_document_holds_is_never_inserted_or_deleted() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let wrap = &fx.wrap;
    let first = doc("first");
    stored(&lab.store(&first).await, 201, &name_of(&first), "the first");
    let held = wrap.issued().remove(0);
    // `begin` reissues the key of the first document's object.
    *wrap.begin_key.lock().unwrap() = Some(ObjectKey::parse(&held).unwrap());
    let second = doc("second");
    let r = lab.store(&second).await;
    problem(&r, 500, None, &name_of(&second), "a document's key");
    assert_eq!(lab.rows().await, "1");
    assert_eq!(lab.audits().await, "1");
    assert!(
        wrap.deleted().is_empty(),
        "the first document's object stays"
    );
    assert_eq!(
        fx.sql("SELECT storage_key FROM ir_documents").await,
        held,
        "the first row is untouched"
    );
    lab.finish().await;
}

// R19 with a concurrent winner: the key's own holder decides, not the digest's row
#[tokio::test]
async fn a_key_another_table_holds_is_not_deleted_when_the_digest_was_stored_meanwhile() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let wrap = &fx.wrap;
    let held = crate::uploads::v4();
    sqlx::query(
        "INSERT INTO artifacts (tenant_id, digest, size_bytes, storage_key) \
         VALUES ($1::uuid, $2, 1, $3)",
    )
    .bind(&lab.tenant)
    .bind("b".repeat(64))
    .bind(&held)
    .execute(&fx.u.w.pool)
    .await
    .unwrap();
    *wrap.begin_key.lock().unwrap() = Some(ObjectKey::parse(&held).unwrap());
    let barrier = wrap.hold_begins(2);
    let bytes = doc("meanwhile");
    let name = name_of(&bytes);
    let winner = async {
        // The request has begun (it is at the barrier) when another store commits the digest.
        while wrap.issued().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        sqlx::query(
            "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
                 variant_name, source_format, storage_key) \
             VALUES ($1::uuid, $2, NULL, 1, 'v', 'odx', $3)",
        )
        .bind(&lab.tenant)
        .bind(&name)
        .bind(crate::uploads::v4())
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
        barrier.wait().await;
    };
    let (r, ()) = tokio::join!(lab.store(&bytes), winner);
    problem(&r, 500, None, &name, "a key held elsewhere");
    assert!(
        wrap.deleted().is_empty(),
        "the artifact's object must not be deleted"
    );
    assert_eq!(lab.rows().await, "1");
    lab.finish().await;
}

// R20
#[tokio::test]
async fn a_failed_record_rolls_the_row_back_and_deletes_the_object() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let bytes = doc("rollback");
    let name = name_of(&bytes);
    // The audit insert fails after the row's insert: the transaction rolls back.
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_audit() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'refused by the test'; END $$; \
         CREATE TRIGGER refuse_audit BEFORE INSERT ON audit_log \
         FOR EACH ROW EXECUTE FUNCTION refuse_audit()",
    )
    .execute(&fx.u.w.pool)
    .await
    .unwrap();
    let r = lab.store(&bytes).await;
    problem(&r, 503, None, &name, "the audit insert fails");
    assert_eq!(lab.rows().await, "0", "the row is rolled back");
    assert_eq!(fx.wrap.deleted(), fx.wrap.issued(), "the object is deleted");
    assert_eq!(lab.calls(), [1, 1, 1, 1]);
    sqlx::raw_sql("DROP TRIGGER refuse_audit ON audit_log")
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "a retry");
    assert_eq!(lab.audits().await, "1");
    // R21, a commit whose outcome is unknown, is not injected: the code keeps the object then
    // (documents.rs, `Recorded`'s caller) and ADR-500 says why.
    lab.finish().await;
}

// R22
#[tokio::test]
async fn a_digest_is_stored_per_tenant() {
    let lab = Lab::new(With::Fake).await;
    let fx = &lab.fx;
    let (other_tenant, _) = fx.u.w.add_operator("bob").await;
    let bob = operator(fx, &other_tenant, "bobby", &[("worker", "{}")]).await;
    assert_eq!(bob.tenant, other_tenant);

    let bytes = doc("shared");
    let name = name_of(&bytes);
    let r = lab.store(&bytes).await;
    stored(&r, 201, &name, "the first tenant");
    let r = put_as(fx, &bob, &name, &bytes).await;
    stored(&r, 201, &name, "the second tenant");
    assert_eq!(lab.rows().await, "2");
    assert_eq!(
        fx.sql("SELECT count(DISTINCT storage_key) FROM ir_documents")
            .await,
        "2",
        "each tenant has its own object"
    );
    assert_eq!(lab.calls()[0], 2);
    assert_eq!(lab.audits().await, "2");
    // Each tenant's second store is a 200.
    let r = put_as(fx, &bob, &name, &bytes).await;
    stored(&r, 200, &name, "the second tenant again");
    let r = lab.store(&bytes).await;
    stored(&r, 200, &name, "the first tenant again");
    assert_eq!(lab.calls()[0], 2);
    lab.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The real object store (R24)

// R24
#[tokio::test]
async fn a_stored_document_reads_back_byte_identical() {
    let lab = Lab::new(With::Real).await;
    let fx = &lab.fx;
    for bytes in [doc("small"), document_of(MAX_BYTES)] {
        assert!(bytes.len() <= MAX_BYTES);
        let name = name_of(&bytes);
        let r = lab.store(&bytes).await;
        stored(&r, 201, &name, "stored");
        let key = fx
            .sql(&format!(
                "SELECT storage_key FROM ir_documents WHERE digest = '{name}'"
            ))
            .await;
        let body = fx.wrap.get(&ObjectKey::parse(&key).unwrap()).await.unwrap();
        let mut object = Vec::new();
        let mut reader = body.reader;
        reader.read_to_end(&mut object).await.unwrap();
        assert!(
            object == bytes,
            "the object is the body ({} bytes)",
            bytes.len()
        );

        // The download route signs it for a job of the agent that names it.
        let mut digest = [0u8; 32];
        for (byte, pair) in digest.iter_mut().zip(name.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        let job = fx.job_naming(&fx.u.id, 1, vec![digest], None, None).await;
        let body =
            format!(r#"{{"jobId":"{job}","generation":1,"kind":"document","sha256":"{name}"}}"#);
        let r = fx
            .send(
                "POST",
                "/api/v1/agent/downloads",
                Some(&fx.token),
                Some("application/json"),
                &body,
            )
            .await;
        let (status, got) = get(&url_of(&r)).await;
        assert_eq!(status, 200);
        assert!(got == bytes, "the signed URL serves the body");
    }
    assert_eq!(lab.rows().await, "2");
    lab.finish().await;
}

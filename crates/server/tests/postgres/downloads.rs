//! The download URL route (`POST /api/v1/agent/downloads`): the judgment order and each refusal
//! against PostgreSQL, with the digest rows of `ir_documents` and `artifacts` seeded by hand, and
//! on the real pinned RustFS (`NGR_TEST_RUSTFS_BIN`) the URL it signs: a plain HTTP GET returns the
//! bytes the store holds. The store the route signs for is the upload tests' wrapper
//! (`uploads.rs`); an object is seeded the way the writer of the rows must (begin, PUT the parts,
//! complete, which reads the object back) and then given its row by SQL. Tokens, keys and digests
//! are made at run time; no assertion message prints one.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rusty_s3::{Bucket, Credentials as S3Credentials, S3Action, UrlStyle};
use server::storage::{GET_URL_EXPIRY, MultipartStore, ObjectKey};
use server::uploads::PART_BYTES;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::control::{nonce_line, token};
use crate::redeem::{Agent, bytes_of, capture, json};
use crate::serving::Reply;
use crate::storage::{content, put, sha256};
use crate::support::assert_unproved;
use crate::uploads::{Call, Fail, Fx, With, bad_nonce_lines, hex, problem, v4};

const PATH: &str = "/api/v1/agent/downloads";

fn body(job: &str, generation: u64, kind: &str, sha: &str) -> String {
    format!(r#"{{"jobId":"{job}","generation":{generation},"kind":"{kind}","sha256":"{sha}"}}"#)
}

async fn ask_as(fx: &Fx, token: Option<&str>, body: &str) -> Reply {
    fx.send("POST", PATH, token, Some("application/json"), body)
        .await
}

async fn ask(fx: &Fx, body: &str) -> Reply {
    ask_as(fx, Some(&fx.token), body).await
}

/// The request of a download with `body`, as the agent writes it.
fn download(body: &str) -> Call {
    Call {
        name: "download",
        method: "POST",
        target: PATH.to_owned(),
        content_type: Some("application/json"),
        body: body.to_owned(),
    }
}

/// The objects the fixture's job names, with a row in both tables for every digest that matters,
/// so that only the instruction's naming (or a missing row) decides.
struct Named {
    job: String,
    document: String,
    artifact: String,
    plan: String,
}

const DOCUMENT: [u8; 32] = [1; 32];
const ARTIFACT: [u8; 32] = [3; 32];
const PLAN: [u8; 32] = [4; 32];

impl Named {
    fn doc(&self, generation: u64) -> String {
        body(&self.job, generation, "document", &self.document)
    }

    fn art(&self, generation: u64) -> String {
        body(&self.job, generation, "artifact", &self.artifact)
    }
}

/// Inserts a digest row of the agent's tenant, with a key that no object has.
async fn row_of(fx: &Fx, table: &str, tenant_of: &str, digest: &str, key: &str) {
    let sql = match table {
        "ir_documents" => {
            "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
                 variant_name, source_format, storage_key) \
             SELECT tenant_id, $2, 'ext', 1, 'variant', 'odx', $3 FROM agents WHERE id = $1::uuid"
        }
        _ => {
            "INSERT INTO artifacts (tenant_id, digest, size_bytes, storage_key) \
             SELECT tenant_id, $2, 1, $3 FROM agents WHERE id = $1::uuid"
        }
    };
    sqlx::query(sql)
        .bind(tenant_of)
        .bind(digest)
        .bind(key)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
}

async fn named(fx: &Fx, generation: u64) -> Named {
    let job = fx
        .job_naming(
            &fx.u.id,
            generation,
            vec![DOCUMENT],
            Some(ARTIFACT),
            Some(PLAN),
        )
        .await;
    let digests = [hex(&DOCUMENT), hex(&ARTIFACT), hex(&PLAN)];
    for digest in &digests {
        for table in ["ir_documents", "artifacts"] {
            row_of(fx, table, &fx.u.id, digest, &v4()).await;
        }
    }
    let [document, artifact, plan] = digests;
    Named {
        job,
        document,
        artifact,
        plan,
    }
}

/// Puts `bytes` in the store as an object read back and matching, and returns its key.
async fn seed(fx: &Fx, bytes: &[u8]) -> ObjectKey {
    let upload = fx.wrap.begin().await.unwrap();
    for (i, chunk) in bytes.chunks(PART_BYTES as usize).enumerate() {
        let number = (i + 1) as u16;
        let url = fx.wrap.presign_part(&upload, number).unwrap();
        let (status, _) = put(url.expose(), chunk.to_vec()).await;
        assert_eq!(status, 200, "PUT part {number}");
    }
    fx.wrap
        .complete(&upload, sha256(bytes), bytes.len() as u64)
        .await
        .unwrap();
    upload.key
}

/// A plain HTTP GET: the status and the body.
pub(crate) async fn get(url: &str) -> (u16, Vec<u8>) {
    let url: url::Url = url.parse().unwrap();
    let addr = (
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap(),
    );
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(connection);
    let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    let request = hyper::Request::builder()
        .method("GET")
        .uri(target)
        .header("host", format!("{}:{}", addr.0, addr.1))
        .header("connection", "close")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

pub(crate) fn url_of(r: &Reply) -> String {
    assert_eq!(r.status, 200, "{}", r.head);
    json(r)["url"].as_str().unwrap().to_owned()
}

async fn set_agent_revoked(fx: &Fx, revoked: bool) {
    let sql = if revoked {
        "UPDATE agents SET revoked_at = now() WHERE id = $1::uuid"
    } else {
        "UPDATE agents SET revoked_at = NULL WHERE id = $1::uuid"
    };
    sqlx::query(sql)
        .bind(&fx.u.id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Order, shape and the token (R1 to R6)

/// R1: only POST is routed.
#[tokio::test]
async fn only_post_is_routed() {
    let fx = Fx::new(With::Nothing).await;
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let r = fx
            .send(method, PATH, Some(&fx.token), Some("application/json"), "")
            .await;
        assert_eq!(r.status, 405, "{method}: {}", r.head);
    }
    fx.finish().await;
}

/// R2: the body is read within 4096 bytes and 5 seconds, before the shape and the token.
#[tokio::test]
async fn the_body_is_bounded_and_read_before_anything_else() {
    let fx = Fx::new(With::Nothing).await;
    let good = body(&v4(), 1, "document", &hex(&DOCUMENT));
    // Exactly 4096 bytes pass the body step: a valid body padded with white space reaches the
    // token (401 with none), one byte more is a 413 whatever the token.
    let padded = format!("{good}{}", " ".repeat(4096 - good.len()));
    assert_eq!(padded.len(), 4096);
    assert_eq!(ask_as(&fx, None, &padded).await.status, 401);
    assert_eq!(ask(&fx, &padded).await.status, 404);
    let over = format!("{padded} ");
    for token in [None, Some(fx.token.as_str())] {
        let r = ask_as(&fx, token, &over).await;
        problem(&r, 413, "over the bound");
    }
    // Declared too long, with no byte sent: answered at once.
    let raw = format!(
        "POST {PATH} HTTP/1.1\r\n{}\r\nContent-Type: application/json\r\nContent-Length: 4097\r\n\r\n",
        fx.u.s.host()
    );
    let r = crate::serving::exchange(fx.u.s.addr, &raw).await;
    problem(&r, 413, "declared");
    // A body that does not arrive: 408 after 5 seconds.
    let raw = format!(
        "POST {PATH} HTTP/1.1\r\n{}\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n",
        fx.u.s.host()
    );
    let started = std::time::Instant::now();
    let r = crate::serving::exchange(fx.u.s.addr, &raw).await;
    problem(&r, 408, "late");
    assert!(started.elapsed() >= Duration::from_secs(5));
    fx.finish().await;
}

/// R3: a body of the wrong shape is a 400 before the token: no lookup, and no token dropped.
#[tokio::test]
async fn the_shape_is_judged_before_the_token_and_drops_no_token() {
    let fx = Fx::new(With::Nothing).await;
    let job = v4();
    let sha = hex(&DOCUMENT);
    let good = body(&job, 1, "document", &sha);
    let upper = job.to_uppercase();
    let bad = [
        "".to_owned(),
        "[]".to_owned(),
        "null".to_owned(),
        "{}".to_owned(),
        format!("{good}x"),
        format!(r#"{{"jobId":"{job}","generation":1,"kind":"document"}}"#),
        good.replace('}', ",\"extra\":1}"),
        good.replace('}', ",\"kind\":\"document\"}"),
        good.replace("\"generation\":1", "\"generation\":null"),
        good.replace("\"kind\":\"document\"", "\"kind\":null"),
        body(&upper, 1, "document", &sha),
        body("not-a-uuid", 1, "document", &sha),
        body(&job, 1, "Document", &sha),
        body(&job, 1, "result", &sha),
        body(&job, 1, "document", &"AB".repeat(32)),
        body(&job, 1, "document", &sha[1..]),
        body(&job, 1, "document", &format!("sha256:{sha}")),
        good.replace("\"generation\":1", "\"generation\":-1"),
        good.replace("\"generation\":1", "\"generation\":1.5"),
        good.replace("\"generation\":1", "\"generation\":\"1\""),
        good.replace("\"generation\":1", "\"generation\":9223372036854775808"),
        good.replace("\"generation\":1", "\"generation\":18446744073709551616"),
    ];
    let t1 = token(&fx.u).await;
    set_agent_revoked(&fx, true).await;
    for text in &bad {
        for t in [None, Some(t1.as_str())] {
            problem(&ask_as(&fx, t, text).await, 400, text);
        }
    }
    // Not JSON.
    let r = fx
        .send("POST", PATH, Some(&t1), Some("text/plain"), &good)
        .await;
    problem(&r, 400, "content type");
    let r = fx.send("POST", PATH, Some(&t1), None, &good).await;
    problem(&r, 400, "no content type");
    // The largest generation is a valid shape.
    let top = body(&job, i64::MAX as u64, "document", &sha);
    assert_eq!(ask_as(&fx, None, &top).await.status, 401);
    // No token was dropped: with the row live again, t1 is still a token.
    set_agent_revoked(&fx, false).await;
    assert_eq!(ask_as(&fx, Some(&t1), &good).await.status, 404);
    fx.finish().await;
}

/// R4: a token that is absent, malformed or unknown is one 401 with `WWW-Authenticate: Bearer`.
#[tokio::test]
async fn no_live_token_is_one_401() {
    let fx = Fx::new(With::Nothing).await;
    let good = body(&v4(), 1, "document", &hex(&DOCUMENT));
    let none = ask_as(&fx, None, &good).await;
    problem(&none, 401, "none");
    assert_eq!(none.header("www-authenticate"), Some("Bearer"));
    for t in ["", "x", "Bearer", &"A".repeat(43), &"A".repeat(500)] {
        let r = ask_as(&fx, Some(t), &good).await;
        problem(&r, 401, t);
        assert_eq!(bytes_of(&r), bytes_of(&none), "{t}");
    }
    let raw = format!(
        "POST {PATH} HTTP/1.1\r\n{}\r\nAuthorization: Basic abc\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{good}",
        fx.u.s.host(),
        good.len()
    );
    let r = crate::serving::exchange(fx.u.s.addr, &raw).await;
    problem(&r, 401, "another scheme");
    assert_eq!(r.body, none.body);
    fx.finish().await;
}

/// R5: a revoked or missing agent row is the same 401, drops the agent's tokens, and the log names
/// the agent and no token.
#[tokio::test]
async fn a_revoked_agent_is_401_and_loses_its_tokens() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    let t1 = token(&fx.u).await;
    let t2 = token(&fx.u).await;
    let unknown = ask_as(&fx, Some("x"), &n.doc(1)).await;
    set_agent_revoked(&fx, true).await;
    let r = ask_as(&fx, Some(&t1), &n.doc(1)).await;
    problem(&r, 401, "revoked");
    assert_eq!(bytes_of(&r), bytes_of(&unknown));
    set_agent_revoked(&fx, false).await;
    for (name, t) in [("t1", &t1), ("t2", &t2), ("first", &fx.token)] {
        let r = ask_as(&fx, Some(t), &n.doc(1)).await;
        assert_eq!(r.status, 401, "{name} is gone");
    }
    let text = log.text();
    assert!(text.contains(&fx.u.id), "the agent is named: {text}");
    for secret in [&t1, &t2, &fx.token] {
        assert!(!text.contains(secret.as_str()), "a token is in the log");
    }
    fx.finish().await;
}

/// R6: the agent's row cannot be read: 503 when the database is down and when the statement is
/// refused (the shared mapping, `db::failure`, takes a database error for unavailability).
#[tokio::test]
async fn an_unreadable_agent_row_is_503_or_500() {
    let mut fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    fx.u.w.pg.stop();
    problem(&ask(&fx, &n.doc(1)).await, 503, "database down");
    // A request that asks for a proof is refused the same way, unproved (ADR-504).
    let (_, r) = fx.nonced(&download(&n.doc(1)), &[]).await;
    problem(&r, 503, "database down, nonce");
    assert_unproved(&r, "database down");
    fx.finish().await;
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    fx.u.w
        .pg
        .execute("ALTER TABLE agents RENAME COLUMN revoked_at TO revoked_x")
        .await;
    problem(&ask(&fx, &n.doc(1)).await, 503, "statement fails");
    let (_, r) = fx.nonced(&download(&n.doc(1)), &[]).await;
    problem(&r, 503, "statement fails, nonce");
    assert_unproved(&r, "statement fails");
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The job, what it names and the rows (R7 to R12)

/// R7, R9, R10 and R11: every refusal of the lookup is a 404 with one body, and a request that
/// names an object the job names (for the generation it has) is not refused.
#[tokio::test]
async fn what_the_job_does_not_name_is_one_404() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 5).await;
    let doc = |digest: &str| body(&n.job, 5, "document", digest);
    let art = |digest: &str| body(&n.job, 5, "artifact", digest);
    let unrelated = hex(&[9; 32]);
    let mut refused: Vec<(&str, String)> = vec![
        // R7: no such job.
        ("unknown job", body(&v4(), 5, "document", &n.document)),
        // R9: another generation.
        ("older", n.doc(4)),
        ("newer", n.doc(6)),
        ("zero", n.doc(0)),
        ("largest", n.doc(i64::MAX as u64)),
        // R10: not named.
        ("document not listed", doc(&unrelated)),
        ("artifact not the artifact", art(&unrelated)),
        ("document as artifact", art(&n.document)),
        ("artifact as document", doc(&n.artifact)),
        ("plan as document", doc(&n.plan)),
        ("plan as artifact", art(&n.plan)),
    ];
    // R10: a digest of one of the job's own uploads.
    let object = fx.job().await;
    sqlx::query(
        "INSERT INTO job_objects (tenant_id, id, job_id, generation, kind, agent_id, storage_key, \
             upload_id, part_bytes, size_bytes, digest, state, finished_at) \
         SELECT tenant_id, gen_random_uuid(), id, 1, 'result', agent_id, $2, 'up', 5242880, 1, $3, \
             'finished', now() FROM jobs WHERE id = $1::uuid",
    )
    .bind(&object)
    .bind(v4())
    .bind(hex(&[7; 32]))
    .execute(&fx.u.w.pool)
    .await
    .unwrap();
    row_of(&fx, "ir_documents", &fx.u.id, &hex(&[7; 32]), &v4()).await;
    let in_job_objects = body(&object, 1, "document", &hex(&[7; 32]));
    refused.push(("a job_objects digest", in_job_objects));
    let reference = ask(&fx, &refused[0].1).await;
    problem(&reference, 404, "reference");
    for (what, text) in &refused {
        let r = ask(&fx, text).await;
        problem(&r, 404, what);
        assert_eq!(bytes_of(&r), bytes_of(&reference), "{what}");
    }
    // R10: a job that names no artifact.
    let plain = fx.job().await;
    let r = ask(&fx, &body(&plain, 1, "artifact", &n.artifact)).await;
    assert_eq!(bytes_of(&r), bytes_of(&reference));
    // R7: another agent's job and another tenant's agent.
    let (_, other) = fx.other_agent().await;
    fx.u.w.add_operator("bob").await;
    let outsider = Agent::new();
    let (_, r) = fx.u.w.redeem("bob", &outsider).await;
    let outsider_id = json(&r)["agentId"].as_str().unwrap().to_owned();
    let r = fx.u.exchange_as(&outsider.key, &outsider_id).await;
    let outsider_token = json(&r)["token"].as_str().unwrap().to_owned();
    for t in [&other, &outsider_token] {
        let r = ask_as(&fx, Some(t), &n.doc(5)).await;
        assert_eq!(bytes_of(&r), bytes_of(&reference));
    }
    // R11: named, but the tenant has no row; another tenant's row is not the tenant's.
    let lonely_doc = hex(&[11; 32]);
    let lonely_art = hex(&[12; 32]);
    let job = fx
        .job_naming(&fx.u.id, 1, vec![[11; 32]], Some([12; 32]), None)
        .await;
    row_of(&fx, "ir_documents", &outsider_id, &lonely_doc, &v4()).await;
    row_of(&fx, "artifacts", &outsider_id, &lonely_art, &v4()).await;
    for (kind, digest) in [("document", &lonely_doc), ("artifact", &lonely_art)] {
        let r = ask(&fx, &body(&job, 1, kind, digest)).await;
        assert_eq!(bytes_of(&r), bytes_of(&reference), "{kind}");
    }
    let text = log.text();
    assert!(text.contains("a named object has no row"), "{text}");
    for secret in [&lonely_doc, &lonely_art, &n.document, &n.job, &fx.token] {
        assert!(!text.contains(secret.as_str()), "a secret is in the log");
    }
    // Nothing reached the store.
    assert_eq!(fx.wrap.calls(), [0, 0, 0, 0]);
    fx.finish().await;
}

/// R8: a stored instruction that does not decode is a 500 and a warning.
#[tokio::test]
async fn a_stored_instruction_that_does_not_decode_is_500() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let job = v4();
    fx.insert_job(&job, &fx.u.id, b"not an envelope").await;
    let r = ask(&fx, &body(&job, 1, "document", &hex(&DOCUMENT))).await;
    problem(&r, 500, "undecodable");
    let text = log.text();
    assert!(
        text.contains("a stored job instruction does not decode"),
        "{text}"
    );
    assert!(!text.contains(&job));
    fx.finish().await;
}

/// R12: a stored key that is not an object key is a 500 and a warning without the key.
#[tokio::test]
async fn a_stored_key_that_does_not_parse_is_500() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let job = fx
        .job_naming(&fx.u.id, 1, vec![DOCUMENT], Some(ARTIFACT), None)
        .await;
    let bad_key = "x".repeat(36);
    row_of(&fx, "ir_documents", &fx.u.id, &hex(&DOCUMENT), &bad_key).await;
    row_of(
        &fx,
        "artifacts",
        &fx.u.id,
        &hex(&ARTIFACT),
        &bad_key.to_uppercase(),
    )
    .await;
    for kind_digest in [("document", hex(&DOCUMENT)), ("artifact", hex(&ARTIFACT))] {
        let r = ask(&fx, &body(&job, 1, kind_digest.0, &kind_digest.1)).await;
        problem(&r, 500, kind_digest.0);
    }
    let text = log.text();
    assert!(
        text.contains("a stored object key does not parse"),
        "{text}"
    );
    assert!(!text.contains(&bad_key));
    assert_eq!(fx.wrap.calls(), [0, 0, 0, 0]);
    fx.finish().await;
}

/// R19: the database down, or a statement it refuses, at the job or the object read is a 503; an
/// answer the code cannot decode (the column of another type) is a 500.
#[tokio::test]
async fn a_failing_job_or_object_read_is_503_or_500() {
    let cases: [(&[&str], u16); 4] = [
        (
            &["ALTER TABLE jobs RENAME COLUMN signed_job TO signed_x"],
            503,
        ),
        (
            &["ALTER TABLE ir_documents RENAME COLUMN storage_key TO key_x"],
            503,
        ),
        (
            &["ALTER TABLE jobs ALTER COLUMN signed_job TYPE text USING encode(signed_job, 'hex')"],
            500,
        ),
        (
            &[
                "ALTER TABLE ir_documents DROP CONSTRAINT ir_documents_storage_key_len",
                "ALTER TABLE ir_documents ALTER COLUMN storage_key TYPE bytea \
                     USING convert_to(storage_key, 'UTF8')",
            ],
            500,
        ),
    ];
    for (statements, status) in cases {
        let fx = Fx::new(With::Fake).await;
        let n = named(&fx, 1).await;
        for sql in statements {
            fx.u.w.pg.execute(sql).await;
        }
        problem(&ask(&fx, &n.doc(1)).await, status, statements[0]);
        fx.finish().await;
    }
    let mut fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    fx.u.w.pg.stop();
    problem(&ask(&fx, &n.doc(1)).await, 503, "database down");
    // A request that asks for a proof is refused the same way, unproved (ADR-504).
    let (_, r) = fx.nonced(&download(&n.doc(1)), &[]).await;
    problem(&r, 503, "database down, nonce");
    assert_unproved(&r, "database down");
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The store (R13)

/// R13: no store is a 503, a store that refuses to sign a 503, one with no randomness a 500.
#[tokio::test]
async fn a_missing_or_failing_store_is_503_or_500() {
    let fx = Fx::new(With::Nothing).await;
    let n = named(&fx, 1).await;
    problem(&ask(&fx, &n.doc(1)).await, 503, "no store");
    fx.finish().await;

    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    *fx.wrap.presign_fail.lock().unwrap() = Some(Fail::Io);
    problem(&ask(&fx, &n.doc(1)).await, 503, "store fails");
    *fx.wrap.presign_fail.lock().unwrap() = Some(Fail::Random);
    problem(&ask(&fx, &n.art(1)).await, 500, "no randomness");
    assert_eq!(fx.wrap.calls(), [0, 2, 0, 0]);
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The URL (R14 to R18)

/// R14: the answer, a new URL per request, no write, and with the URL a plain GET returns the
/// object byte for byte, for a document and for an artifact.
#[tokio::test]
async fn the_url_returns_the_object_byte_for_byte() {
    let fx = Fx::new(With::Real).await;
    let n = named(&fx, 2).await;
    // An object of two parts, so that the GET reads more than one chunk.
    let document = content(7, PART_BYTES as usize + 1234);
    let artifact = content(9, 4321);
    let (document_key, artifact_key) = (seed(&fx, &document).await, seed(&fx, &artifact).await);
    for (table, digest, key) in [
        ("ir_documents", &n.document, &document_key),
        ("artifacts", &n.artifact, &artifact_key),
    ] {
        let sql = format!("UPDATE {table} SET storage_key = $1 WHERE digest = $2");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key.as_str())
            .bind(digest)
            .execute(&fx.u.w.pool)
            .await
            .unwrap();
    }
    let writes = fx
        .sql("SELECT (SELECT count(*) FROM job_objects) || ',' || (SELECT count(*) FROM jobs)")
        .await;
    for (text, want) in [(n.doc(2), &document), (n.art(2), &artifact)] {
        let r = ask(&fx, &text).await;
        assert_eq!(r.status, 200, "{}", r.head);
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        let v = json(&r);
        assert_eq!(v["expiresInSecs"], GET_URL_EXPIRY.as_secs());
        assert_eq!(v.as_object().unwrap().len(), 2);
        let url = v["url"].as_str().unwrap().to_owned();
        assert!(url.starts_with("http://127.0.0.1:"), "the URL's host");
        let (status, bytes) = get(&url).await;
        assert_eq!(status, 200);
        assert!(bytes == *want, "the object differs from what was stored");
        // A second request is a new URL (the signature carries the time and nothing else is
        // random, so wait a second for the timestamp to move).
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_ne!(url_of(&ask(&fx, &text).await), url);
    }
    // The route wrote nothing and did no store call but the signing.
    assert_eq!(
        fx.sql("SELECT (SELECT count(*) FROM job_objects) || ',' || (SELECT count(*) FROM jobs)")
            .await,
        writes
    );
    let [begins, _, lists, deletes] = fx.wrap.calls();
    assert_eq!((begins, lists, deletes), (2, 0, 0), "the seeding only");
    fx.finish().await;
}

/// R15 and R16: the job's state plays no part, and the object missing from the store, though its
/// row exists, is still a URL (the GET then fails at the object server).
#[tokio::test]
async fn the_job_state_plays_no_part_and_the_route_does_not_look_in_the_store() {
    let fx = Fx::new(With::Real).await;
    let n = named(&fx, 1).await;
    let key = seed(&fx, b"short").await;
    sqlx::query("UPDATE ir_documents SET storage_key = $1 WHERE digest = $2")
        .bind(key.as_str())
        .bind(&n.document)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    for state in [
        "received",
        "running",
        "succeeded",
        "failed",
        "expired",
        "cancelled",
    ] {
        sqlx::query("UPDATE jobs SET state = $2 WHERE id = $1::uuid")
            .bind(&n.job)
            .bind(state)
            .execute(&fx.u.w.pool)
            .await
            .unwrap();
        assert_eq!(ask(&fx, &n.doc(1)).await.status, 200, "{state}");
    }
    // The object is deleted from the store: the row is still there, and so is the URL.
    fx.wrap.delete(&key).await.unwrap();
    let url = url_of(&ask(&fx, &n.doc(1)).await);
    assert_eq!(get(&url).await.0, 404, "the object server has nothing");
    // A row whose key never named an object (the artifact's row, with a made-up key).
    let url = url_of(&ask(&fx, &n.art(1)).await);
    assert_eq!(get(&url).await.0, 404);
    fx.finish().await;
}

/// R18: an agent revoked after its row was read keeps a URL that is already issued, which lives
/// for [`GET_URL_EXPIRY`] at most (accepted); the next request is refused.
#[tokio::test]
async fn a_url_issued_before_a_revocation_still_works_until_it_expires() {
    let fx = Fx::new(With::Real).await;
    let n = named(&fx, 1).await;
    let bytes = content(3, 777);
    let key = seed(&fx, &bytes).await;
    sqlx::query("UPDATE ir_documents SET storage_key = $1 WHERE digest = $2")
        .bind(key.as_str())
        .bind(&n.document)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let url = url_of(&ask(&fx, &n.doc(1)).await);
    set_agent_revoked(&fx, true).await;
    assert_eq!(ask(&fx, &n.doc(1)).await.status, 401);
    let (status, got) = get(&url).await;
    assert_eq!((status, got == bytes), (200, true));
    fx.finish().await;
}

/// The expiry probe: a URL signed for 5 seconds, a GET started in time and read slowly past the
/// expiry. The result is what the bundled object server does, which the route's 60 seconds
/// relies on only for the start of a transfer: a request is authorised when it arrives, and a
/// transfer that is under way is not cut when the signature runs out. A GET started after the
/// expiry is refused.
#[tokio::test]
async fn a_transfer_started_before_the_expiry_completes_after_it() {
    let fx = Fx::new(With::Real).await;
    // Larger than what the socket buffers hold, so that the object server is still sending when
    // the signature runs out.
    let bytes = content(5, 40 * 1024 * 1024);
    let key = seed(&fx, &bytes).await;
    let (port, credentials) = {
        let (_, object_server) = fx.real.as_ref().unwrap();
        (object_server.port(), object_server.credentials())
    };
    let bucket = Bucket::new(
        format!("http://127.0.0.1:{port}").parse().unwrap(),
        UrlStyle::Path,
        "nekoguruma".to_owned(),
        "us-east-1".to_owned(),
    )
    .unwrap();
    let signer = S3Credentials::new(credentials.access_key(), credentials.expose_secret());
    let url = bucket
        .get_object(Some(&signer), key.as_str())
        .sign(Duration::from_secs(5));
    let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    // Read the head and a little of the body, then stop reading until the signature has expired.
    let mut received = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    while !received.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the head arrives");
        received.extend_from_slice(&chunk[..n]);
    }
    let head_end = received.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    // The 200 is the proof that the transfer began while the signature was valid; how long the
    // head took is not asserted, since the signing time is cut to the second.
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // Stall until the signature has expired (the signing time is cut to the second, so how long
    // that takes varies): a new GET with the same URL is then refused.
    let mut late_status = 0;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        late_status = get(url.as_str()).await.0;
        if late_status != 200 {
            break;
        }
    }
    assert_eq!(late_status, 403, "the expired URL is refused");
    // The slow reader reads the rest.
    let mut body_len = received.len() - head_end;
    loop {
        let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut chunk))
            .await
            .expect("the transfer goes on")
            .unwrap_or(0);
        if n == 0 {
            break;
        }
        body_len += n;
    }
    eprintln!(
        "expiry probe: the transfer started before the expiry delivered {body_len} of {} bytes",
        bytes.len()
    );
    assert_eq!(
        body_len,
        bytes.len(),
        "a transfer under way is not cut by the expiry"
    );
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The warning gates, the agent's row and the token's life, and the migration's rules

/// The download route's warnings have a gate of their own: a caller that makes the route warn
/// (R11) does not hide the warning of an upload route's failure that comes next.
#[tokio::test]
async fn the_download_warnings_do_not_use_up_the_upload_routes_gate() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let job = fx.job_naming(&fx.u.id, 1, vec![[21; 32]], None, None).await;
    let r = ask(&fx, &body(&job, 1, "document", &hex(&[21; 32]))).await;
    problem(&r, 404, "no row");
    assert!(log.text().contains("a named object has no row"));
    // An upload route's database failure right after: begin's insert is refused.
    fx.u.w
        .pg
        .execute("ALTER TABLE job_objects ADD CONSTRAINT refuse CHECK (false) NOT VALID")
        .await;
    let upload_job = fx.job().await;
    let r = fx
        .begin(&crate::uploads::decl(
            &upload_job,
            1,
            "result",
            10,
            &hex(&[1; 32]),
        ))
        .await;
    problem(&r, 503, "insert refused");
    let text = log.text();
    assert!(
        text.contains("upload route: the database is unavailable"),
        "the upload route's warning is logged: {text}"
    );
    fx.finish().await;
}

/// R5: an agent whose row is gone is the same 401, and its tokens are dropped.
#[tokio::test]
async fn a_missing_agent_row_is_401_and_drops_the_tokens() {
    let fx = Fx::new(With::Fake).await;
    let agent = Agent::new();
    let id = fx.u.w.register(&agent).await;
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let r = fx.u.exchange_as(&agent.key, &id).await;
        tokens.push(json(&r)["token"].as_str().unwrap().to_owned());
    }
    let unknown = ask_as(&fx, Some("x"), &body(&v4(), 1, "document", &hex(&DOCUMENT))).await;
    for table in [
        "agent_allowed_operators",
        "agent_registration_codes",
        "agents",
    ] {
        let column = if table == "agents" { "id" } else { "agent_id" };
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {column} = $1::uuid"
        )))
        .bind(&id)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    }
    let text = body(&v4(), 1, "document", &hex(&DOCUMENT));
    let r = ask_as(&fx, Some(&tokens[0]), &text).await;
    problem(&r, 401, "missing row");
    assert_eq!(bytes_of(&r), bytes_of(&unknown));
    // The other token is gone from the session table itself, not only refused for the row.
    assert!(fx.u.sessions.authenticate(&tokens[0]).is_none());
    assert!(fx.u.sessions.authenticate(&tokens[1]).is_none());
    let r = ask_as(&fx, Some(&tokens[1]), &text).await;
    problem(&r, 401, "dropped");
    fx.finish().await;
}

/// R4: a token past its lifetime is the same 401.
#[tokio::test]
async fn an_expired_token_is_401() {
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    let unknown = ask_as(&fx, Some("x"), &n.doc(9)).await;
    // A generation the job does not have: a 404 shows the token was accepted.
    assert_eq!(ask(&fx, &n.doc(9)).await.status, 404, "live first");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(
        server::session::TOKEN_LIFETIME_SECS + 1,
    ))
    .await;
    tokio::time::resume();
    let r = ask(&fx, &n.doc(9)).await;
    problem(&r, 401, "expired");
    assert_eq!(bytes_of(&r), bytes_of(&unknown));
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    fx.finish().await;
}

fn sqlstate(error: &sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .unwrap_or_default()
}

/// Migration 0014: the digest is bare lowercase hex and the storage key is a unique 36-character
/// text, on both tables.
#[tokio::test]
async fn the_digest_tables_refuse_a_malformed_digest_or_key() {
    let fx = Fx::new(With::Fake).await;
    let tenant: String =
        sqlx::query_scalar("SELECT tenant_id::text FROM agents WHERE id = $1::uuid")
            .bind(&fx.u.id)
            .fetch_one(&fx.u.w.pool)
            .await
            .unwrap();
    for table in ["ir_documents", "artifacts"] {
        let insert = |digest: String, key: String| {
            let pool = fx.u.w.pool.clone();
            let tenant = tenant.clone();
            async move {
                let sql = if table == "ir_documents" {
                    "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, \
                         variant_name, source_format, storage_key) \
                     VALUES ($1::uuid, $2, 'e', 1, 'v', 'odx', $3)"
                } else {
                    "INSERT INTO artifacts (tenant_id, digest, size_bytes, storage_key) \
                     VALUES ($1::uuid, $2, 1, $3)"
                };
                sqlx::query(sql)
                    .bind(tenant)
                    .bind(digest)
                    .bind(key)
                    .execute(&pool)
                    .await
            }
        };
        let good = hex(&[5; 32]);
        for (what, digest, key) in [
            ("uppercase", "AB".repeat(32), v4()),
            ("prefixed", format!("sha256:{good}"), v4()),
            ("short digest", good[1..].to_owned(), v4()),
            ("65-character digest", format!("{good}a"), v4()),
            ("non-hex digest", "g".repeat(64), v4()),
            ("35-character key", good.clone(), v4()[1..].to_owned()),
            ("37-character key", good.clone(), format!("{}0", v4())),
        ] {
            let error = insert(digest, key)
                .await
                .expect_err(&format!("{table}: {what}"));
            assert_eq!(sqlstate(&error), "23514", "{table}: {what}");
        }
        let key = v4();
        insert(good, key.clone()).await.expect("a good row");
        // The same key under another digest.
        let error = insert(hex(&[6; 32]), key).await.unwrap_err();
        assert_eq!(sqlstate(&error), "23505", "{table}: duplicate key");
    }
    fx.finish().await;
}

/// The gates gate: a reason that warns twice in a row logs once, and every reason has a gate of
/// its own, so a NoRow warning, which a caller can make at will, hides none of the others.
#[tokio::test]
async fn the_download_gates_suppress_a_repeat_and_keep_the_reasons_apart() {
    let (log, _guard) = capture();
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    let lonely = fx.job_naming(&fx.u.id, 1, vec![[21; 32]], None, None).await;
    let no_row = body(&lonely, 1, "document", &hex(&[21; 32]));
    let count = |what: &str| log.text().matches(what).count();
    // The same reason twice: one line, and the second request is still answered.
    problem(&ask(&fx, &no_row).await, 404, "no row");
    problem(&ask(&fx, &no_row).await, 404, "no row again");
    assert_eq!(count("download route: a named object has no row"), 1);
    // A reason that follows is logged.
    *fx.wrap.presign_fail.lock().unwrap() = Some(Fail::Io);
    problem(&ask(&fx, &n.doc(1)).await, 503, "store fails");
    assert_eq!(count("download route: the object store failed"), 1);
    // So are an undecodable instruction and a key that does not parse.
    let broken = v4();
    fx.insert_job(&broken, &fx.u.id, b"not an envelope").await;
    problem(
        &ask(&fx, &body(&broken, 1, "document", &hex(&DOCUMENT))).await,
        500,
        "undecodable",
    );
    assert_eq!(
        count("download route: a stored job instruction does not decode"),
        1
    );
    let bad_key_job = fx.job_naming(&fx.u.id, 1, vec![[22; 32]], None, None).await;
    row_of(
        &fx,
        "ir_documents",
        &fx.u.id,
        &hex(&[22; 32]),
        &"x".repeat(36),
    )
    .await;
    problem(
        &ask(&fx, &body(&bad_key_job, 1, "document", &hex(&[22; 32]))).await,
        500,
        "bad key",
    );
    assert_eq!(
        count("download route: a stored object key does not parse"),
        1
    );
    // A repeat of the first reason is still suppressed after all that.
    problem(&ask(&fx, &no_row).await, 404, "no row, a third time");
    assert_eq!(count("download route: a named object has no row"), 1);
    // The database's two failures have gates apart: unavailable (a refused statement), then
    // internal (an answer that cannot be decoded).
    fx.u.w
        .pg
        .execute("ALTER TABLE ir_documents RENAME COLUMN storage_key TO key_x")
        .await;
    problem(&ask(&fx, &n.doc(1)).await, 503, "database refuses");
    assert_eq!(count("download route: the database is unavailable"), 1);
    // (The artifacts query has not run yet, so no cached plan hides the change.)
    fx.u.w
        .pg
        .execute("ALTER TABLE artifacts DROP CONSTRAINT artifacts_storage_key_len")
        .await;
    fx.u.w
        .pg
        .execute(
            "ALTER TABLE artifacts ALTER COLUMN storage_key TYPE bytea \
                 USING convert_to(storage_key, 'UTF8')",
        )
        .await;
    problem(&ask(&fx, &n.art(1)).await, 500, "decode fails");
    assert_eq!(count("download route: a statement failed"), 1);
    fx.finish().await;
}

// ---------------------------------------------------------------------------------------------
// The answer proof (ADR-504)

#[tokio::test]
async fn a_download_answer_without_a_nonce_carries_no_proof() {
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    for (what, text, status) in [
        ("no such job", body(&v4(), 1, "document", &n.document), 404),
        ("the store has no URL", n.doc(1), 503),
    ] {
        let r = ask(&fx, &text).await;
        assert_eq!(r.status, status, "{what}: {}", r.head);
        assert_unproved(&r, what);
    }
    fx.finish().await;
}

/// Nothing refused before the token and the agent's row is proved: the body, the shape, the guard,
/// the method and the token.
#[tokio::test]
async fn no_download_answer_before_the_token_carries_a_proof() {
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    let line = [nonce_line(&shared_crypto::random_32().unwrap())];
    let good = n.doc(1);
    let big = format!("{good}{}", " ".repeat(4096));
    for (what, text, status) in [("shape", "{}".to_owned(), 400), ("size", big, 413)] {
        for token in [None, Some(fx.token.as_str())] {
            let r = fx
                .send_with("POST", PATH, token, Some("application/json"), &line, &text)
                .await;
            assert_eq!(r.status, status, "{what}: {}", r.head);
            assert_unproved(&r, what);
        }
    }
    let call = download(&good);
    for token in [None, Some("x")] {
        let r = fx.call(&call, token, &line).await;
        problem(&r, 401, "token");
        assert_unproved(&r, "token");
    }
    let r = fx
        .send_with("GET", PATH, Some(&fx.token), None, &line, "")
        .await;
    assert_eq!(r.status, 405, "{}", r.head);
    assert_unproved(&r, "method");
    let host = format!(
        "POST {PATH} HTTP/1.1\r\nHost: example.invalid\r\n{}\r\nConnection: close\r\n\
         Content-Length: 0\r\n\r\n",
        line[0]
    );
    let r = crate::serving::exchange(fx.u.s.addr, &host).await;
    assert_eq!(r.status, 421, "{}", r.head);
    assert_unproved(&r, "guard");
    // The body that does not arrive.
    let slow = format!(
        "POST {PATH} HTTP/1.1\r\n{}\r\n{}\r\nContent-Type: application/json\r\n\
         Content-Length: 10\r\n\r\n",
        fx.u.s.host(),
        line[0]
    );
    let r = crate::serving::exchange(fx.u.s.addr, &slow).await;
    problem(&r, 408, "no body");
    assert_unproved(&r, "no body");
    // The agent's row: revoked.
    set_agent_revoked(&fx, true).await;
    let r = fx.call(&call, Some(&fx.token), &line).await;
    problem(&r, 401, "revoked");
    assert_unproved(&r, "revoked");
    fx.finish().await;
}

#[tokio::test]
async fn a_download_nonce_or_target_the_proof_cannot_take_is_400_before_the_token() {
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    let call = download(&n.doc(1));
    for (name, lines) in bad_nonce_lines() {
        for token in [None, Some(fx.token.as_str())] {
            let r = fx.call(&call, token, &lines).await;
            problem(&r, 400, name);
            assert!(r.body.contains("Invalid Ngr-Nonce header"), "{}", r.body);
            assert_unproved(&r, name);
        }
    }
    // The route's own shape error and length come first.
    let (_, lines) = bad_nonce_lines().swap_remove(0);
    let r = fx
        .send_with("POST", PATH, None, Some("application/json"), &lines, "{}")
        .await;
    problem(&r, 400, "shape");
    assert!(r.body.contains("not the expected JSON"), "{}", r.body);
    // A target the transcript cannot take: too long, or a byte over 0x7F in the query. The
    // parser may refuse the second before the route does; both are a 400.
    let line = [nonce_line(&shared_crypto::random_32().unwrap())];
    for suffix in [format!("?q={}", "a".repeat(2100)), "?q=\u{e9}".to_owned()] {
        let bad = Call {
            target: format!("{PATH}{suffix}"),
            ..download(&n.doc(1))
        };
        for token in [None, Some(fx.token.as_str())] {
            let r = fx.call(&bad, token, &line).await;
            assert_eq!(r.status, 400, "{}", r.head);
            assert_unproved(&r, "target");
        }
        // Without a nonce the same request is judged as before: the token, then the answer.
        assert_eq!(fx.call(&bad, None, &[]).await.status, 401);
        assert_eq!(fx.call(&bad, Some(&fx.token), &[]).await.status, 503);
    }
    fx.finish().await;
}

#[tokio::test]
async fn a_download_answer_is_proved_and_binds_the_target_as_sent() {
    let fx = Fx::new(With::Real).await;
    let n = named(&fx, 1).await;
    let key = seed(&fx, b"downloaded bytes").await;
    sqlx::query("UPDATE ir_documents SET storage_key = $1 WHERE digest = $2")
        .bind(key.as_str())
        .bind(&n.document)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    let call = download(&n.doc(1));
    // Without a nonce the same answer carries no proof.
    let plain = fx.call(&call, Some(&fx.token), &[]).await;
    assert_eq!(plain.status, 200, "{}", plain.head);
    assert_unproved(&plain, "no nonce");
    let r = fx.proved(&call, 200).await;
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(json(&r)["url"].is_string());
    // The target is the bytes the agent sent, query included.
    let queried = Call {
        target: format!("{PATH}?a=1"),
        ..download(&n.doc(1))
    };
    let (nonce, r) = fx.nonced(&queried, &[]).await;
    fx.assert_proved_for(nonce, "POST", &queried.target, &queried.body, 200, &r);
    let wrong = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fx.assert_proved_for(nonce, "POST", PATH, &queried.body, 200, &r);
    }));
    assert!(wrong.is_err(), "the proof ignores the query");
    fx.finish().await;
}

/// Every answer after the token and the row are judged is proved, whatever its status.
#[tokio::test]
async fn every_download_answer_after_the_token_and_row_is_proved() {
    let fx = Fx::new(With::Fake).await;
    let n = named(&fx, 1).await;
    fx.proved(&download(&body(&v4(), 1, "document", &n.document)), 404)
        .await;
    fx.proved(&download(&n.doc(2)), 404).await;
    fx.proved(&download(&body(&n.job, 1, "document", &hex(&[9; 32]))), 404)
        .await;
    // Named by the instruction, with no row.
    let unrowed = fx.job_naming(&fx.u.id, 1, vec![[7; 32]], None, None).await;
    fx.proved(
        &download(&body(&unrowed, 1, "document", &hex(&[7; 32]))),
        404,
    )
    .await;
    // An instruction that does not decode.
    let garbage = v4();
    fx.insert_job(&garbage, &fx.u.id, b"not an envelope").await;
    fx.proved(&download(&body(&garbage, 1, "document", &n.document)), 500)
        .await;
    // The store fails.
    fx.proved(&download(&n.doc(1)), 503).await;
    *fx.wrap.presign_fail.lock().unwrap() = Some(Fail::Random);
    fx.proved(&download(&n.art(1)), 500).await;
    fx.finish().await;

    // A stored key that does not parse, and no object store.
    let fx = Fx::new(With::Nothing).await;
    let n = named(&fx, 1).await;
    fx.proved(&download(&n.doc(1)), 503).await;
    sqlx::query("UPDATE ir_documents SET storage_key = repeat('x', 36) WHERE digest = $1")
        .bind(&n.document)
        .execute(&fx.u.w.pool)
        .await
        .unwrap();
    fx.proved(&download(&n.doc(1)), 500).await;
    fx.finish().await;
}

#[tokio::test]
async fn a_download_signing_failure_is_an_unproved_503() {
    let (log, _guard) = capture();
    let (fx, fail) = Fx::flaky(With::Fake).await;
    let n = named(&fx, 1).await;
    let call = download(&n.doc(1));
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, r) = fx.nonced(&call, &[]).await;
    problem(&r, 503, "signing fails");
    assert!(
        r.body.contains("The answer could not be proved"),
        "{}",
        r.body
    );
    assert_unproved(&r, "signing fails");
    // Without a nonce nothing is signed, so the failing signer changes nothing.
    assert_eq!(fx.call(&call, Some(&fx.token), &[]).await.status, 503);
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    fx.proved(&call, 503).await;
    let text = log.text();
    assert!(text.contains("could not be proved"), "{text}");
    assert!(!text.contains(PATH), "the log names the target: {text}");
    fx.finish().await;
}

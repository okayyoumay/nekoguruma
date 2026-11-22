//! The download URL route: `POST /api/v1/agent/downloads` (design 5.2, 13.3). An agent names a job,
//! the job's ownership generation, what it wants (`document` or `artifact`) and the SHA-256 of it,
//! and gets a presigned URL to GET the object from, valid for [`GET_URL_EXPIRY`].
//!
//! A request is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the body (at most [`MAX_BODY`] bytes, 413; within 5 seconds,
//! 408; broken, 400), the shape (400), the bearer token (401 with `WWW-Authenticate: Bearer`), the
//! agent's row (401 and the agent's tokens dropped when it is revoked or missing, 503, 500), the
//! job (404), its generation (404), what the job names (404), the digest's row (404), the stored
//! key (500) and the object store (503, 500). All the 404 answers are one body, so a caller learns
//! nothing of which step refused it.
//!
//! What a job names is its stored instruction, decoded as the upload routes decode it for the
//! generation: a document is named when its digest is among the instruction's document hashes, an
//! artifact when it is the instruction's artifact hash. The plan hash and the objects the agent
//! uploads (`job_objects`) are not downloads. The job's state plays no part: only the generation
//! must be the one the agent declares.
//!
//! The route only reads. A digest has a row in `ir_documents` or `artifacts` only for an object the
//! store read back, and the row holds the store's key. The route does no object-store I/O: the
//! store signs the URL, so an object the server holds no more still gets its URL and the GET fails
//! at the object server. A caller revoked after the agent's row was read keeps a URL that lives at
//! most [`GET_URL_EXPIRY`].
//!
//! Never logged: a token, a body, a digest, an object key or a URL (the SQLSTATE of a database
//! error only).

use std::fmt;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::json;
use shared_proto::{JobInstruction, is_canonical_uuid};
use sqlx::PgPool;

use crate::agent_caller::{self, Refusal};
use crate::control::unauthorized;
use crate::db;
use crate::problem::Problem;
use crate::register::{bounded_body_within, is_json};
use crate::storage::{GET_URL_EXPIRY, ObjectKey, StoreError};
use crate::uploads::{
    DownloadWarning, MAX_BODY, Number, StoredInstruction, Uploads, is_digest, json_answer,
    parse_sha256, stored_instruction,
};

/// The route's path.
pub const PATH: &str = "/api/v1/agent/downloads";

/// The route shares the state of the upload routes: the pool, the session table and the object
/// store. Its warnings have gates of their own, one per reason (`Uploads::warn_download`).
type Downloads = Uploads;

// ---------------------------------------------------------------------------------------------
// Answers

fn problem(status: StatusCode, title: &'static str) -> Response {
    Problem::new(status, title).into_response()
}

/// The one 404: whatever step refused, the body is the same.
fn not_found() -> Response {
    problem(StatusCode::NOT_FOUND, "No such object")
}

fn internal() -> Response {
    problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed")
}

fn no_store() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "The object store is unavailable",
    )
}

fn db_failure(d: &Downloads, error: &sqlx::Error) -> Response {
    match db::failure(error) {
        db::Failure::Unavailable(why) => {
            d.warn_download(
                DownloadWarning::Unavailable,
                "the database is unavailable",
                &why,
            );
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "The database is unavailable",
            )
        }
        db::Failure::Internal(why) => {
            d.warn_download(DownloadWarning::Internal, "a statement failed", &why);
            internal()
        }
    }
}

fn store_failure(d: &Downloads, error: &StoreError) -> Response {
    // The store's messages are fixed text: no key or URL.
    d.warn_download(
        DownloadWarning::Store,
        "the object store failed",
        &error.to_string(),
    );
    match error {
        StoreError::Random => internal(),
        _ => no_store(),
    }
}

// ---------------------------------------------------------------------------------------------
// The shape

/// The four properties of the body, typed; nothing is judged yet.
#[derive(Default)]
struct Raw {
    job_id: Option<String>,
    generation: Option<Number>,
    kind: Option<String>,
    sha256: Option<String>,
}

impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Raw, D::Error> {
        struct RawVisitor;

        impl<'de> Visitor<'de> for RawVisitor {
            type Value = Raw;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a download request")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Raw, A::Error> {
                fn put<T, E: de::Error>(slot: &mut Option<T>, value: T) -> Result<(), E> {
                    match slot.replace(value) {
                        None => Ok(()),
                        Some(_) => Err(E::custom("duplicate property")),
                    }
                }
                let mut raw = Raw::default();
                while let Some(name) = map.next_key::<String>()? {
                    match name.as_str() {
                        "jobId" => put(&mut raw.job_id, map.next_value()?)?,
                        "generation" => put(&mut raw.generation, map.next_value()?)?,
                        "kind" => put(&mut raw.kind, map.next_value()?)?,
                        "sha256" => put(&mut raw.sha256, map.next_value()?)?,
                        _ => return Err(de::Error::custom("unknown property")),
                    }
                }
                Ok(raw)
            }
        }

        deserializer.deserialize_map(RawVisitor)
    }
}

/// What an agent can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Document,
    Artifact,
}

impl Kind {
    /// The table that maps a tenant's digests of this kind to store keys.
    fn table(self) -> &'static str {
        match self {
            Kind::Document => "ir_documents",
            Kind::Artifact => "artifacts",
        }
    }
}

/// What the body asks for, judged for its shape.
#[derive(Debug, PartialEq, Eq)]
struct Wanted {
    job_id: String,
    generation: u64,
    kind: Kind,
    digest: String,
}

fn shape(headers: &HeaderMap, body: &[u8]) -> Option<Wanted> {
    if !is_json(headers) {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let raw = Raw::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    let job_id = raw.job_id.filter(|id| is_canonical_uuid(id))?;
    let generation = match raw.generation? {
        Number::Int(g) if i64::try_from(g).is_ok() => g,
        _ => return None,
    };
    let kind = match raw.kind?.as_str() {
        "document" => Kind::Document,
        "artifact" => Kind::Artifact,
        _ => return None,
    };
    let digest = raw.sha256.filter(|d| is_digest(d))?;
    Some(Wanted {
        job_id,
        generation,
        kind,
        digest,
    })
}

// ---------------------------------------------------------------------------------------------
// The database

enum Lookup {
    /// No job of that ID belongs to the agent.
    NoJob,
    /// The stored instruction does not decode.
    Undecodable,
    /// The job's generation is not the one declared.
    OtherGeneration,
    /// The instruction does not name the digest as that kind.
    NotNamed,
    /// The digest is named but the tenant has no row for it.
    NoRow,
    /// The key the object store issued for the object.
    Key(String),
}

/// The job's instruction, then the digest's row, in one transaction.
async fn lookup(
    pool: &PgPool,
    tenant: &str,
    agent: &str,
    wanted: &Wanted,
    hash: [u8; 32],
) -> Result<Lookup, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let instruction = match stored_instruction(&mut tx, tenant, &wanted.job_id, agent).await? {
        StoredInstruction::NoJob => return Ok(Lookup::NoJob),
        StoredInstruction::Undecodable => return Ok(Lookup::Undecodable),
        StoredInstruction::Instruction(instruction) => instruction,
    };
    if instruction.ownership_generation != wanted.generation {
        return Ok(Lookup::OtherGeneration);
    }
    if !names(&instruction, wanted.kind, &hash) {
        return Ok(Lookup::NotNamed);
    }
    // The table name is one of two constants.
    let key: Option<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT storage_key FROM {} WHERE tenant_id = $1::uuid AND digest = $2",
        wanted.kind.table()
    )))
    .bind(tenant)
    .bind(&wanted.digest)
    .fetch_optional(&mut *tx)
    .await?;
    Ok(key.map_or(Lookup::NoRow, |(key,)| Lookup::Key(key)))
}

/// Whether the instruction names `hash` as an object of `kind`.
fn names(instruction: &JobInstruction, kind: Kind, hash: &[u8; 32]) -> bool {
    match kind {
        Kind::Document => instruction.document_hashes.contains(hash),
        Kind::Artifact => instruction.artifact_hash.as_ref() == Some(hash),
    }
}

// ---------------------------------------------------------------------------------------------
// The route

/// `POST /api/v1/agent/downloads`.
pub async fn url(State(d): State<Downloads>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match bounded_body_within(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    // A shape that passed has a digest that parses.
    let Some((wanted, hash)) = shape(&parts.headers, &bytes)
        .and_then(|wanted| parse_sha256(&wanted.digest).map(|hash| (wanted, hash)))
    else {
        return problem(
            StatusCode::BAD_REQUEST,
            "The body is not the expected JSON object",
        );
    };
    let caller = match agent_caller::authenticate(d.pool(), d.sessions(), &parts.headers).await {
        Ok(caller) => caller,
        Err(Refusal::Unauthorized) => return unauthorized(),
        Err(Refusal::Database(error)) => return db_failure(&d, &error),
    };
    let key = match lookup(d.pool(), &caller.tenant, &caller.agent.0, &wanted, hash).await {
        Err(error) => return db_failure(&d, &error),
        Ok(Lookup::NoJob | Lookup::OtherGeneration | Lookup::NotNamed) => return not_found(),
        Ok(Lookup::Undecodable) => {
            d.warn_download(
                DownloadWarning::Undecodable,
                "a stored job instruction does not decode",
                "",
            );
            return internal();
        }
        Ok(Lookup::NoRow) => {
            // The instruction names an object the server does not hold: the row is missing.
            d.warn_download(DownloadWarning::NoRow, "a named object has no row", "");
            return not_found();
        }
        Ok(Lookup::Key(key)) => key,
    };
    let Ok(key) = ObjectKey::parse(&key) else {
        d.warn_download(
            DownloadWarning::BadKey,
            "a stored object key does not parse",
            "",
        );
        return internal();
    };
    let Some(store) = d.store() else {
        return no_store();
    };
    match store.presign_get(&key) {
        Ok(url) => json_answer(
            StatusCode::OK,
            json!({"url": url.expose(), "expiresInSecs": GET_URL_EXPIRY.as_secs()}),
        ),
        Err(error) => store_failure(&d, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, header};

    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h
    }

    const JOB: &str = "0192f3a4-5b6c-7d8e-8f90-123456789abc";

    fn digest() -> String {
        "ab".repeat(32)
    }

    fn shaped(body: &str) -> Option<Wanted> {
        shape(&headers(), body.as_bytes())
    }

    fn body(job: &str, generation: &str, kind: &str, sha: &str) -> String {
        format!(r#"{{"jobId":{job},"generation":{generation},"kind":{kind},"sha256":{sha}}}"#)
    }

    fn good() -> String {
        body(
            &format!("\"{JOB}\""),
            "3",
            "\"document\"",
            &format!("\"{}\"", digest()),
        )
    }

    #[test]
    fn a_complete_body_is_the_request() {
        let want = Wanted {
            job_id: JOB.to_owned(),
            generation: 3,
            kind: Kind::Document,
            digest: digest(),
        };
        assert_eq!(shaped(&good()), Some(want));
        let artifact = good().replace("document", "artifact");
        assert_eq!(shaped(&artifact).unwrap().kind, Kind::Artifact);
    }

    #[test]
    fn the_generation_is_an_integer_from_0_to_the_largest_i64() {
        let with =
            |g: &str| shaped(&good().replace("\"generation\":3", &format!("\"generation\":{g}")));
        assert_eq!(with("0").unwrap().generation, 0);
        assert_eq!(
            with("9223372036854775807").unwrap().generation,
            i64::MAX as u64
        );
        for bad in [
            "9223372036854775808",
            "18446744073709551616",
            "1e30",
            "-1",
            "1.5",
            "1.0",
            "1e3",
            "\"3\"",
            "null",
            "true",
            "[3]",
        ] {
            assert_eq!(with(bad), None, "{bad}");
        }
    }

    #[test]
    fn anything_but_the_four_exact_members_is_refused() {
        let job = format!("\"{JOB}\"");
        let sha = format!("\"{}\"", digest());
        let doc = "\"document\"";
        // Unknown, duplicate, missing and null members.
        assert_eq!(shaped(&good().replace('}', ",\"extra\":1}")), None);
        assert_eq!(
            shaped(&good().replace('}', ",\"kind\":\"document\"}")),
            None
        );
        assert_eq!(
            shaped(&format!(r#"{{"jobId":{job},"generation":3,"kind":{doc}}}"#)),
            None
        );
        assert_eq!(shaped(&body("null", "3", doc, &sha)), None);
        assert_eq!(shaped(&body(&job, "null", doc, &sha)), None);
        assert_eq!(shaped(&body(&job, "3", "null", &sha)), None);
        assert_eq!(shaped(&body(&job, "3", doc, "null")), None);
        assert_eq!(shaped("{}"), None);
        assert_eq!(shaped("[]"), None);
        assert_eq!(
            shaped(&format!("{} ", good())).map(|w| w.generation),
            Some(3)
        );
        assert_eq!(shaped(&format!("{}x", good())), None);
        // The wrong content type.
        assert!(shape(&HeaderMap::new(), good().as_bytes()).is_none());
    }

    #[test]
    fn the_job_the_kind_and_the_digest_are_exact() {
        let job = format!("\"{JOB}\"");
        let sha = format!("\"{}\"", digest());
        let doc = "\"document\"";
        assert!(shaped(&body(&job, "3", doc, &sha)).is_some());
        for bad_job in [
            "\"0192F3A4-5B6C-7D8E-8F90-123456789ABC\"",
            "\"0192f3a45b6c7d8e8f90123456789abc\"",
            "\"{0192f3a4-5b6c-7d8e-8f90-123456789abc}\"",
            "\"\"",
            "7",
        ] {
            assert_eq!(shaped(&body(bad_job, "3", doc, &sha)), None, "{bad_job}");
        }
        for bad_kind in ["\"Document\"", "\"result\"", "\"commLog\"", "\"\"", "1"] {
            assert_eq!(shaped(&body(&job, "3", bad_kind, &sha)), None, "{bad_kind}");
        }
        let upper = format!("\"{}\"", "AB".repeat(32));
        let short = format!("\"{}\"", "ab".repeat(31));
        let long = format!("\"{}\"", "ab".repeat(33));
        let prefixed = format!("\"sha256:{}\"", digest());
        for bad_sha in [upper, short, long, prefixed, "\"\"".to_owned()] {
            assert_eq!(shaped(&body(&job, "3", doc, &bad_sha)), None, "{bad_sha}");
        }
    }

    #[test]
    fn an_instruction_names_a_document_or_the_artifact_and_nothing_else() {
        let mut i = JobInstruction {
            version: shared_proto::INSTRUCTION_VERSION,
            job_id: shared_proto::JobId(JOB.to_owned()),
            ownership_generation: 3,
            agent_id: shared_proto::AgentId(JOB.to_owned()),
            operator_id: "op".to_owned(),
            kind: shared_proto::JobKind::Acquire,
            vci: shared_proto::VciId("v".to_owned()),
            vin: None,
            document_hashes: vec![[1; 32], [2; 32]],
            artifact_hash: Some([3; 32]),
            plan_hash: Some([4; 32]),
            issued_at: 1,
            start_deadline: 2,
        };
        assert!(names(&i, Kind::Document, &[1; 32]));
        assert!(names(&i, Kind::Document, &[2; 32]));
        assert!(!names(&i, Kind::Document, &[3; 32]));
        assert!(!names(&i, Kind::Document, &[4; 32]));
        assert!(names(&i, Kind::Artifact, &[3; 32]));
        assert!(!names(&i, Kind::Artifact, &[1; 32]));
        assert!(!names(&i, Kind::Artifact, &[4; 32]));
        i.artifact_hash = None;
        assert!(!names(&i, Kind::Artifact, &[3; 32]));
        assert!(!names(&i, Kind::Artifact, &[0; 32]));
    }
}

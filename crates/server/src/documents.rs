//! The IR document store: `PUT /api/v1/documents/{sha256}` (design 8.2.6, 14.3; ADR-500). An
//! operator who may start jobs puts the bytes of one IR document, named by their SHA-256, and the
//! server keeps it in the object store and in `ir_documents` for the tenant, so that a job can
//! name it (ADR-454) and the download route can sign it (ADR-497).
//!
//! A request is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the session layer (403 for the Origin, 401, 503, 500), the
//! name (64 lowercase hex digits, else 400), the Content-Type (exactly one
//! `application/octet-stream`, else 400), the declared length (over [`MAX_BYTES`], 413), the
//! caller's permission (the role `worker` with no scope, as job creation needs, else 403), the
//! concurrent stores (at most [`MAX_CONCURRENT`], else 503 with `Retry-After`), the body (413,
//! 408 after [`BODY_TIMEOUT`], 400), its hash against the name (400 `DIGEST_MISMATCH`), the
//! tenant's row (a stored digest answers 200 and nothing more happens), the document (400
//! `INVALID_IR_DOCUMENT` for what `diag_ir::declaration::load_ecu_document` refuses and for
//! metadata out of bounds), the object store (503), the store's begin, part and completion, which
//! reads the object back (503; the object is deleted when a step after `begin` fails), and the
//! row's insert with its audit row (201; 200 when a concurrent store won; 500 when the key is
//! held by another table).
//!
//! The permission is judged before the body is read, so a refused caller costs no 16 MiB read.
//! The body goes through the server, unlike the captured data and artifacts of 14.3 that go to
//! the store directly: a document is bounded at [`MAX_BYTES`] and the server must parse it
//! before it keeps it (ADR-500 item 2).
//!
//! No transaction is open while the object store is called: the existing-row check, and the
//! insert with its audit row, are transactions of their own. The key a row names is only ever
//! one this request's `begin` issued, so a failure after the read-back deletes the object, which
//! no row names yet. The one exception is a commit whose outcome is unknown: the row may exist,
//! so the object is kept (a later store of the same bytes finds the row, or stores them again).
//!
//! Never logged or put in a problem: the body, the digest, an object key or an upload ID.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::HttpBody;
use axum::extract::{Extension, Path, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use diag_ir::declaration::{EcuDocument, load_ecu_document};
use serde_json::json;
use sha2::{Digest, Sha256};
use shared_proto::MAX_DOCUMENT_BYTES;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::audit;
use crate::db;
use crate::health::may_log;
use crate::operator_session::SessionOperator;
use crate::problem::Problem;
use crate::register::{bounded_body_for, closing};
use crate::session::now;
use crate::storage::{MultipartStore, ObjectKey, StoreError};
use crate::uploads::{is_digest, json_answer, parse_sha256};

/// The route's path.
pub const PATH: &str = "/api/v1/documents/{sha256}";
/// The largest document, in bytes; exactly this many pass.
pub const MAX_BYTES: usize = MAX_DOCUMENT_BYTES as usize;
/// The most stores that run at once; one more is turned away with 503. Each holds up to
/// [`MAX_BYTES`] in memory.
pub const MAX_CONCURRENT: usize = 2;
/// How long the body may take to arrive. It is longer than the small routes' 5 seconds, since
/// 16 MiB is its largest size.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// The `Retry-After` of the 503 for a store beyond [`MAX_CONCURRENT`], in seconds.
pub const RETRY_AFTER_SECS: u64 = 5;
/// The longest metadata string (a variant name, part number, software version, source format or
/// source digest) taken from a document, in bytes.
pub const MAX_TEXT: usize = 256;
/// The most part numbers, and the most software versions, a document may list.
pub const MAX_LIST: usize = 1024;
/// A refusal reason, or a failure, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------------------------
// The document

/// What a stored document's row holds, taken from the document (ADR-500 items 7 and 9).
#[derive(Debug, PartialEq, Eq)]
struct Meta {
    schema_version: i32,
    variant_name: String,
    part_numbers: Vec<String>,
    sw_versions: Vec<String>,
    source_format: String,
    source_digest: String,
}

/// A string kept in a row: at most [`MAX_TEXT`] bytes and no control character (a NUL would be
/// a database error, and the others have no place in a name shown to an operator).
fn text(value: &str) -> Option<String> {
    (value.len() <= MAX_TEXT && !value.chars().any(char::is_control)).then(|| value.to_owned())
}

/// A list of `len` strings kept in a row: at most [`MAX_LIST`] of them, each as [`text`] says.
fn strings<'a>(len: usize, values: impl Iterator<Item = &'a str>) -> Option<Vec<String>> {
    if len > MAX_LIST {
        return None;
    }
    values.map(text).collect()
}

/// The metadata of a loaded document, or `None` when any of it is out of bounds: a variant with
/// no name, a string that is too long or holds a control character, more than [`MAX_LIST`] part
/// numbers or software versions.
fn meta(document: &EcuDocument<'_>) -> Option<Meta> {
    let variant = document.variant()?;
    let variant_name = text(variant.short_name().filter(|name| !name.is_empty())?)?;
    Some(Meta {
        schema_version: i32::try_from(document.schema_version()).ok()?,
        variant_name,
        part_numbers: variant
            .part_numbers()
            .map_or(Some(Vec::new()), |v| strings(v.len(), v.iter()))?,
        sw_versions: variant
            .sw_versions()
            .map_or(Some(Vec::new()), |v| strings(v.len(), v.iter()))?,
        source_format: text(document.source_format()?)?,
        source_digest: text(document.source_digest()?)?,
    })
}

/// The row for a document the loader accepts, or `None` for anything else.
fn judge(bytes: &[u8]) -> Option<Meta> {
    meta(&load_ecu_document(bytes).ok()?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// State

/// The route's state: the pool, the object store, the slots and the log gates of this router.
#[derive(Clone)]
pub struct Documents(Arc<Inner>);

struct Inner {
    pool: PgPool,
    store: Option<Arc<dyn MultipartStore>>,
    body_timeout: Duration,
    slots: Arc<Semaphore>,
    gates: [Mutex<Option<Instant>>; 6],
}

/// Why the route warns; each reason has its own log gate, so a flood of one cannot silence
/// another.
#[derive(Clone, Copy)]
enum Warning {
    /// The caller lacks the role (the reason of a 403).
    NotWorker = 0,
    /// The database is unavailable (503).
    Unavailable = 1,
    /// A statement failed in this code (500).
    Internal = 2,
    /// The object store failed.
    Store = 3,
    /// The key the store issued is held by another table (500).
    KeyHeld = 4,
    /// The object of a refused or lost store could not be deleted.
    Cleanup = 5,
}

impl Documents {
    pub fn new(pool: PgPool, backend: &crate::uploads::Backend) -> Documents {
        Documents(Arc::new(Inner {
            pool,
            store: backend.shared_store(),
            body_timeout: backend.document_body_timeout(),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            gates: std::array::from_fn(|_| Mutex::new(None)),
        }))
    }
}

impl Inner {
    fn warn(&self, reason: Warning, operator: &str, what: &str, why: &str) {
        if may_log(&self.gates[reason as usize], now(), LOG_EVERY) {
            tracing::warn!(operator_id = %operator, why, "document store: {what}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Answers

fn problem(status: StatusCode, title: &'static str) -> Response {
    Problem::new(status, title).into_response()
}

/// A refusal that ends the connection: the body may be unread.
fn closed(status: StatusCode, title: &'static str) -> Response {
    closing(status, title)
}

fn invalid_document() -> Response {
    Problem::with_code(
        StatusCode::BAD_REQUEST,
        "The body is not an IR document this server accepts",
        "INVALID_IR_DOCUMENT",
    )
    .into_response()
}

fn digest_mismatch() -> Response {
    Problem::with_code(
        StatusCode::BAD_REQUEST,
        "The body does not hash to the name in the path",
        "DIGEST_MISMATCH",
    )
    .into_response()
}

fn no_store() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "The object store is unavailable",
    )
}

fn internal() -> Response {
    problem(StatusCode::INTERNAL_SERVER_ERROR, "The request failed")
}

fn stored(status: StatusCode, name: &str) -> Response {
    json_answer(status, json!({ "sha256": name }))
}

fn db_failure(inner: &Inner, operator: &str, error: &sqlx::Error) -> Response {
    match db::failure(error) {
        db::Failure::Unavailable(why) => {
            inner.warn(
                Warning::Unavailable,
                operator,
                "the database is unavailable",
                &why,
            );
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "The database is unavailable",
            )
        }
        db::Failure::Internal(why) => {
            inner.warn(Warning::Internal, operator, "a statement failed", &why);
            internal()
        }
    }
}

fn store_failure(inner: &Inner, operator: &str, error: &StoreError) -> Response {
    // The store's messages are fixed text: no key, URL or upload ID.
    inner.warn(
        Warning::Store,
        operator,
        "the object store failed",
        &error.to_string(),
    );
    match error {
        StoreError::Random => internal(),
        _ => no_store(),
    }
}

/// Whether the headers carry exactly one Content-Type whose media type is
/// `application/octet-stream` (parameters allowed).
fn is_octet_stream(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|essence| {
            essence
                .trim()
                .eq_ignore_ascii_case("application/octet-stream")
        })
}

// ---------------------------------------------------------------------------------------------
// The database

/// Whether the caller holds the role `worker` with an empty scope, which creating a job needs
/// too (design 6.8).
async fn may_store(pool: &PgPool, tenant: &str, operator: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    let (worker,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM operator_roles r WHERE r.tenant_id = $1::uuid \
                        AND r.operator_id = $2::uuid AND r.role = 'worker' \
                        AND r.scope = '{}'::jsonb)",
    )
    .bind(tenant)
    .bind(operator)
    .fetch_one(&mut *tx)
    .await?;
    Ok(worker)
}

/// Whether the tenant holds a row for the digest.
async fn has_row(pool: &PgPool, tenant: &str, digest: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    db::statement_timeouts(&mut tx).await?;
    row_exists(&mut tx, tenant, digest).await
}

async fn row_exists(
    tx: &mut sqlx::PgConnection,
    tenant: &str,
    digest: &str,
) -> Result<bool, sqlx::Error> {
    let (found,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM ir_documents WHERE tenant_id = $1::uuid AND digest = $2)",
    )
    .bind(tenant)
    .bind(digest)
    .fetch_one(tx)
    .await?;
    Ok(found)
}

/// What the insert came to.
enum Recorded {
    /// This request's row is committed, with its audit row.
    Won,
    /// A concurrent store committed the digest's row first; nothing was written.
    Lost,
    /// The key is held by `artifacts` or `job_objects`; nothing was written.
    KeyHeld,
}

/// How the insert failed: before the commit, the key is in no row; at the commit, the row may
/// exist.
enum RecordFailure {
    Statement(sqlx::Error),
    Commit(sqlx::Error),
}

impl From<sqlx::Error> for RecordFailure {
    fn from(error: sqlx::Error) -> Self {
        RecordFailure::Statement(error)
    }
}

/// The row, then its audit row, in one transaction. The insert loses quietly to a row of the same
/// tenant and digest, and does not happen at all when another table already holds the key (the
/// unique index of `ir_documents` covers its own table only, ADR-500 item 13).
async fn record(
    pool: &PgPool,
    tenant: &str,
    operator: &str,
    digest: &str,
    size: u64,
    meta: &Meta,
    key: &ObjectKey,
) -> Result<Recorded, RecordFailure> {
    let mut tx = pool
        .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
        .await?;
    db::statement_timeouts(&mut tx).await?;
    let inserted = sqlx::query(
        "INSERT INTO ir_documents (tenant_id, digest, extension_id, schema_version, variant_name, \
             part_numbers, sw_versions, source_format, source_digest, storage_key) \
         SELECT $1::uuid, $2, NULL::text, $3, $4, $5, $6, $7, $8, $9 \
         WHERE NOT EXISTS (SELECT 1 FROM artifacts WHERE storage_key = $9) \
           AND NOT EXISTS (SELECT 1 FROM job_objects WHERE storage_key = $9) \
         ON CONFLICT (tenant_id, digest) DO NOTHING",
    )
    .bind(tenant)
    .bind(digest)
    .bind(meta.schema_version)
    .bind(&meta.variant_name)
    .bind(&meta.part_numbers)
    .bind(&meta.sw_versions)
    .bind(&meta.source_format)
    .bind(&meta.source_digest)
    .bind(key.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        // Either a concurrent store holds the digest (committed now: this statement waited for
        // it) or the guard held the key back. The transaction's end writes nothing.
        return Ok(if row_exists(&mut tx, tenant, digest).await? {
            Recorded::Lost
        } else {
            Recorded::KeyHeld
        });
    }
    audit::document_stored(&mut tx, tenant, operator, digest, size).await?;
    tx.commit().await.map_err(RecordFailure::Commit)?;
    Ok(Recorded::Won)
}

// ---------------------------------------------------------------------------------------------
// The route

/// `PUT /api/v1/documents/{sha256}`: 201 with `{"sha256": ...}` when this request stored the
/// document, 200 with the same body when the tenant already held it.
pub async fn store(
    State(docs): State<Documents>,
    Extension(operator): Extension<SessionOperator>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let inner = &*docs.0;
    let (parts, body) = request.into_parts();
    if !is_digest(&name) {
        return closed(
            StatusCode::BAD_REQUEST,
            "The name must be 64 lowercase hexadecimal digits",
        );
    }
    if !is_octet_stream(&parts.headers) {
        return closed(
            StatusCode::BAD_REQUEST,
            "Content-Type must be application/octet-stream",
        );
    }
    if body.size_hint().lower() > MAX_BYTES as u64 {
        return closed(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The request body is too large",
        );
    }
    let who = operator.operator_id.as_str();

    match may_store(&inner.pool, &operator.tenant_id, who).await {
        Ok(true) => {}
        Ok(false) => {
            inner.warn(
                Warning::NotWorker,
                who,
                "refused: the caller is not a worker with an empty scope",
                "",
            );
            return closed(StatusCode::FORBIDDEN, "Not permitted to store documents");
        }
        Err(error) => return db_failure(inner, who, &error),
    }

    let Ok(_slot) = Arc::clone(&inner.slots).try_acquire_owned() else {
        let mut response = closed(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many documents are being stored",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECS));
        return response;
    };

    let bytes = match bounded_body_for(body, MAX_BYTES, inner.body_timeout).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };

    // The hash and the parse of up to 16 MiB are CPU work: off the runtime's threads. The bytes
    // come back for the store.
    let Ok((bytes, hash)) = tokio::task::spawn_blocking(move || {
        let hash = hex(&Sha256::digest(&bytes));
        (bytes, hash)
    })
    .await
    else {
        return internal();
    };
    if hash != name {
        return digest_mismatch();
    }

    match has_row(&inner.pool, &operator.tenant_id, &name).await {
        Ok(true) => return stored(StatusCode::OK, &name),
        Ok(false) => {}
        Err(error) => return db_failure(inner, who, &error),
    }

    let Ok((bytes, meta)) = tokio::task::spawn_blocking(move || {
        let meta = judge(&bytes);
        (bytes, meta)
    })
    .await
    else {
        return internal();
    };
    let Some(meta) = meta else {
        return invalid_document();
    };
    let size = bytes.len() as u64;

    let Some(store) = inner.store.as_deref() else {
        return no_store();
    };
    let upload = match store.begin().await {
        Ok(upload) => upload,
        Err(error) => return store_failure(inner, who, &error),
    };
    // From here the object exists under a key no row names: a failure deletes it.
    if let Err(error) = store.put_part(&upload, 1, bytes).await {
        discard(inner, store, &upload.key, who).await;
        return store_failure(inner, who, &error);
    }
    // The name parsed as a digest above, and equals the body's hash.
    let Some(sha) = parse_sha256(&name) else {
        discard(inner, store, &upload.key, who).await;
        return internal();
    };
    if let Err(error) = store.complete(&upload, sha, size).await {
        // A mismatch has deleted the object already; the delete again is harmless.
        discard(inner, store, &upload.key, who).await;
        return store_failure(inner, who, &error);
    }

    match record(
        &inner.pool,
        &operator.tenant_id,
        who,
        &name,
        size,
        &meta,
        &upload.key,
    )
    .await
    {
        Ok(Recorded::Won) => {
            tracing::info!(operator_id = %who, "document stored");
            stored(StatusCode::CREATED, &name)
        }
        Ok(Recorded::Lost) => {
            // The winner's object is the row's; this request's is in no row.
            discard(inner, store, &upload.key, who).await;
            stored(StatusCode::OK, &name)
        }
        Ok(Recorded::KeyHeld) => {
            // The key may be another row's object: it is not deleted.
            inner.warn(
                Warning::KeyHeld,
                who,
                "the object store issued a key another table holds",
                "",
            );
            internal()
        }
        Err(RecordFailure::Statement(error)) => {
            discard(inner, store, &upload.key, who).await;
            db_failure(inner, who, &error)
        }
        Err(RecordFailure::Commit(error)) => {
            // Whether the row committed is unknown, so the object may be one a row names: it is
            // kept. If the row is not there, the object is one that nothing names, and a later
            // store of the same bytes makes its own.
            db_failure(inner, who, &error)
        }
    }
}

/// Deletes an object no row names, best effort: a failure is logged and the answer stays what
/// it was.
async fn discard(inner: &Inner, store: &dyn MultipartStore, key: &ObjectKey, operator: &str) {
    if let Err(error) = store.delete(key).await {
        inner.warn(
            Warning::Cleanup,
            operator,
            "an object nothing names could not be deleted",
            &error.to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diag_ir::IR_SCHEMA_VERSION;
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, encode_ecu_document};

    /// The bytes of a document with these header strings (`diag-ir`'s types are non-exhaustive,
    /// so they are filled in field by field).
    fn encoded(format: &str, digest: &str, variant: EcuVariantT) -> Vec<u8> {
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some(format.to_owned());
        document.source_digest = Some(digest.to_owned());
        document.variant = Some(Box::new(variant));
        encode_ecu_document(&document)
    }

    fn document(variant: EcuVariantT) -> Vec<u8> {
        encoded("odx", &"ab".repeat(32), variant)
    }

    fn named(name: &str) -> EcuVariantT {
        let mut variant = EcuVariantT::default();
        variant.short_name = Some(name.to_owned());
        variant
    }

    fn judged(variant: EcuVariantT) -> Option<Meta> {
        judge(&document(variant))
    }

    // R12
    #[test]
    fn metadata_is_bounded_and_free_of_control_characters() {
        let mut variant = named("Engine");
        variant.part_numbers = Some(vec!["P-1".to_owned(), "P-2".to_owned()]);
        variant.sw_versions = Some(vec!["1.0".to_owned()]);
        let meta = judged(variant).expect("a document within the bounds");
        assert_eq!(meta.variant_name, "Engine");
        assert_eq!(meta.part_numbers, ["P-1", "P-2"]);
        assert_eq!(meta.sw_versions, ["1.0"]);
        assert_eq!(meta.source_format, "odx");
        assert_eq!(meta.schema_version, IR_SCHEMA_VERSION as i32);

        // The names of a variant: present, not empty, within the bound, no control character.
        let longest = "n".repeat(MAX_TEXT);
        assert!(judged(named(&longest)).is_some());
        assert!(judged(named(&format!("{longest}n"))).is_none());
        assert!(judged(EcuVariantT::default()).is_none());
        assert!(judged(named("")).is_none());
        for bad in ["a\0b", "a\nb", "a\tb", "\u{7f}", "\u{85}"] {
            assert!(judged(named(bad)).is_none(), "{bad:?}");
        }
        // Text that is not ASCII is allowed, counted in bytes.
        assert!(judged(named("Motorsteuerung ä")).is_some());
        let wide = "ä".repeat(MAX_TEXT / 2);
        assert!(judged(named(&wide)).is_some());
        assert!(judged(named(&format!("{wide}ä"))).is_none());

        // The lists: the count and each string.
        let list = |n: usize| (0..n).map(|i| format!("v{i}")).collect::<Vec<_>>();
        let mut variant = named("Engine");
        variant.part_numbers = Some(list(MAX_LIST));
        variant.sw_versions = Some(list(MAX_LIST));
        assert!(judged(variant).is_some());
        let mut variant = named("Engine");
        variant.part_numbers = Some(list(MAX_LIST + 1));
        assert!(judged(variant).is_none());
        let mut variant = named("Engine");
        variant.sw_versions = Some(list(MAX_LIST + 1));
        assert!(judged(variant).is_none());
        for field in 0..2 {
            let mut variant = named("Engine");
            let bad = Some(vec!["fine".to_owned(), "x\0y".to_owned()]);
            if field == 0 {
                variant.part_numbers = bad;
            } else {
                variant.sw_versions = bad;
            }
            assert!(judged(variant).is_none(), "field {field}");
            let mut variant = named("Engine");
            let long = Some(vec!["v".repeat(MAX_TEXT + 1)]);
            if field == 0 {
                variant.part_numbers = long;
            } else {
                variant.sw_versions = long;
            }
            assert!(judged(variant).is_none(), "long field {field}");
        }

        // The header strings of the document.
        let bytes = encoded("od\0x", "", named("Engine"));
        assert!(judge(&bytes).is_none());
        let bytes = encoded("odx", &"d".repeat(MAX_TEXT + 1), named("Engine"));
        assert!(judge(&bytes).is_none());
    }

    #[test]
    fn the_content_type_is_exactly_one_octet_stream() {
        let with = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            }
            is_octet_stream(&headers)
        };
        assert!(with(&["application/octet-stream"]));
        assert!(with(&["Application/Octet-Stream; x=y"]));
        assert!(!with(&[]));
        assert!(!with(&["application/json"]));
        assert!(!with(&[
            "application/octet-stream",
            "application/octet-stream"
        ]));
    }
}

//! The object store on the real object server (ADR-492): the pinned RustFS as the server's child
//! (`NGR_TEST_RUSTFS_BIN`, as `object_server_rustfs.rs` has it), a bucket, parts PUT to presigned
//! URLs by a plain HTTP client, completion and its read-back, deletion and listing. The scripted
//! units are in `src/storage/s3/tests.rs`; the local store's are in `src/storage/local/tests.rs`.
//!
//! The tests that start the whole server also need PostgreSQL (the start order's earlier steps).

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use server::config::{Config, Database, Http, InstructionKeyConfig, Profile};
use server::object_server::ObjectServer;
use server::storage::{MultipartStore, ObjectKey, S3Store, S3Tuning, StoreError, Upload};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::object_server_rustfs::Real;
use crate::redeem::capture;
use crate::support::TestPostgres;

const MIB: usize = 1024 * 1024;

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

/// Deterministic content of `len` bytes that differs from part to part.
fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A plain HTTP PUT of `body` to a presigned URL: the status and the answer's `etag`.
async fn put(url: &str, body: Vec<u8>) -> (u16, Option<String>) {
    let (status, headers) = request("PUT", url, body).await;
    (status, headers)
}

async fn request(method: &str, url: &str, body: Vec<u8>) -> (u16, Option<String>) {
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
        .method(method)
        .uri(target)
        .header("host", format!("{}:{}", addr.0, addr.1))
        .header("content-length", body.len())
        .header("connection", "close")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_owned());
    let _ = response.into_body().collect().await;
    (status, etag)
}

/// Uploads `data` as part `number` of `upload`.
async fn put_part(store: &S3Store, upload: &Upload, number: u16, data: Vec<u8>) {
    let url = store.presign_part(upload, number).unwrap();
    let (status, etag) = put(url.expose(), data).await;
    assert_eq!(status, 200);
    assert!(etag.is_some_and(|tag| tag.starts_with('"')));
}

struct Running {
    real: Real,
    server: Option<ObjectServer>,
    store: S3Store,
}

impl Running {
    async fn start() -> Running {
        Running::start_with("nekoguruma").await
    }

    async fn start_with(bucket: &str) -> Running {
        let real = Real::new();
        let server = real.start().await;
        let store = open(&server, bucket).await;
        Running {
            real,
            server: Some(server),
            store,
        }
    }

    async fn stop(mut self) {
        self.server
            .take()
            .unwrap()
            .stop(std::future::pending())
            .await;
    }
}

async fn open(server: &ObjectServer, bucket: &str) -> S3Store {
    tokio::time::timeout(
        Duration::from_secs(60),
        S3Store::open(
            server.port(),
            server.credentials(),
            bucket,
            S3Tuning::default(),
        ),
    )
    .await
    .expect("open must not hang")
    .unwrap_or_else(|error| panic!("the store must open: {error}"))
}

async fn read_all(store: &S3Store, key: &ObjectKey) -> Result<Vec<u8>, StoreError> {
    let body = store.get(key).await?;
    let length = body.length;
    let mut reader = body.reader;
    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    assert_eq!(out.len() as u64, length);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------

/// Two parts, the listing, the completion and the read-back, and what the log of the object
/// server does not hold.
#[tokio::test]
async fn an_upload_in_two_parts_completes_and_reads_back() {
    let (log, _guard) = capture();
    let run = Running::start().await;
    let store = &run.store;
    let upload = store.begin().await.unwrap();
    let first = content(1, 5 * MIB);
    let second = content(2, 1234);
    put_part(store, &upload, 1, first.clone()).await;
    put_part(store, &upload, 2, second.clone()).await;
    let parts = store.list_parts(&upload).await.unwrap();
    assert_eq!(
        parts.iter().map(|p| (p.number, p.size)).collect::<Vec<_>>(),
        [(1, (5 * MIB) as u64), (2, 1234)]
    );
    let whole: Vec<u8> = [first, second].concat();
    store
        .complete(&upload, sha256(&whole), whole.len() as u64)
        .await
        .unwrap();
    assert_eq!(read_all(store, &upload.key).await.unwrap(), whole);
    // The upload is gone and the object is there.
    assert!(matches!(
        store.list_parts(&upload).await,
        Err(StoreError::AlreadyFinished)
    ));
    let secret = run
        .server
        .as_ref()
        .unwrap()
        .credentials()
        .expose_secret()
        .to_owned();
    run.stop().await;
    let text = log.text();
    assert!(
        !text.contains("X-Amz-Signature"),
        "a signature is in the log"
    );
    assert!(!text.contains(&secret));
}

/// A writer that lost its place lists what is held, re-sends a part (the later one wins) and
/// completes; a second completion of the finished upload verifies again and succeeds.
#[tokio::test]
async fn a_writer_resumes_resends_a_part_and_may_complete_twice() {
    let run = Running::start().await;
    let store = &run.store;
    let upload = store.begin().await.unwrap();
    put_part(store, &upload, 1, content(3, 5 * MIB)).await;
    assert_eq!(store.list_parts(&upload).await.unwrap().len(), 1);
    // Part 1 sent again with other content: the new bytes are the part.
    let first = content(4, 5 * MIB);
    put_part(store, &upload, 1, first.clone()).await;
    let last = content(5, 10);
    put_part(store, &upload, 2, last.clone()).await;
    let whole: Vec<u8> = [first, last].concat();
    let length = whole.len() as u64;
    store
        .complete(&upload, sha256(&whole), length)
        .await
        .unwrap();
    // The answer was lost, the caller calls again.
    store
        .complete(&upload, sha256(&whole), length)
        .await
        .unwrap();
    assert_eq!(read_all(store, &upload.key).await.unwrap(), whole);
    run.stop().await;
}

/// A digest that is not the object's deletes the object and the upload.
#[tokio::test]
async fn a_hash_mismatch_leaves_nothing_behind() {
    let run = Running::start().await;
    let store = &run.store;
    let upload = store.begin().await.unwrap();
    let data = content(6, 100);
    put_part(store, &upload, 1, data.clone()).await;
    let error = store
        .complete(&upload, sha256(b"not the content"), 100)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HashMismatch), "{error}");
    assert!(matches!(
        store.get(&upload.key).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.list_parts(&upload).await,
        Err(StoreError::NotFound)
    ));
    let listed = store.list_unfinished().await.unwrap();
    assert!(listed.uploads.iter().all(|u| u.key != upload.key));
    // The same after a finished object is found to be another one on a retry.
    let upload = store.begin().await.unwrap();
    put_part(store, &upload, 1, data.clone()).await;
    store.complete(&upload, sha256(&data), 100).await.unwrap();
    let error = store
        .complete(&upload, sha256(b"other"), 100)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HashMismatch), "{error}");
    assert!(matches!(
        store.get(&upload.key).await,
        Err(StoreError::NotFound)
    ));
    run.stop().await;
}

/// The store's own checks hold against the real server, and the upload stays usable after one.
#[tokio::test]
async fn the_stores_part_checks_refuse_before_the_object_server_would() {
    let run = Running::start().await;
    let store = &run.store;
    // No parts.
    let upload = store.begin().await.unwrap();
    assert!(matches!(
        store.complete(&upload, sha256(b""), 0).await,
        Err(StoreError::NoParts)
    ));
    // A gap.
    put_part(store, &upload, 2, content(7, 10)).await;
    assert!(matches!(
        store.complete(&upload, sha256(b""), 10).await,
        Err(StoreError::MissingPart { part: 1 })
    ));
    // Small and not last.
    put_part(store, &upload, 1, content(8, 1000)).await;
    assert!(matches!(
        store.complete(&upload, sha256(b""), 1010).await,
        Err(StoreError::PartTooSmall { part: 1 })
    ));
    // The upload is still there: replace part 1 by a full-size one and finish.
    let first = content(9, 5 * MIB);
    put_part(store, &upload, 1, first.clone()).await;
    // The length the caller names must be the sum.
    assert!(matches!(
        store.complete(&upload, sha256(b""), 5).await,
        Err(StoreError::LengthMismatch { stored }) if stored == (5 * MIB + 10) as u64
    ));
    let whole: Vec<u8> = [first, content(7, 10)].concat();
    store
        .complete(&upload, sha256(&whole), whole.len() as u64)
        .await
        .unwrap();
    run.stop().await;
}

/// One empty part is an empty object.
#[tokio::test]
async fn one_empty_part_makes_an_empty_object() {
    let run = Running::start().await;
    let store = &run.store;
    let upload = store.begin().await.unwrap();
    put_part(store, &upload, 1, Vec::new()).await;
    store.complete(&upload, sha256(b""), 0).await.unwrap();
    let body = store.get(&upload.key).await.unwrap();
    assert_eq!(body.length, 0);
    assert!(read_all(store, &upload.key).await.unwrap().is_empty());
    run.stop().await;
}

/// Delete in each state, and of nothing.
#[tokio::test]
async fn delete_removes_an_unfinished_upload_a_finished_object_and_nothing() {
    let run = Running::start().await;
    let store = &run.store;
    // Unfinished, with a part.
    let upload = store.begin().await.unwrap();
    put_part(store, &upload, 1, content(1, 10)).await;
    store.delete(&upload.key).await.unwrap();
    assert!(matches!(
        store.list_parts(&upload).await,
        Err(StoreError::NotFound)
    ));
    let listed = store.list_unfinished().await.unwrap();
    assert!(listed.uploads.iter().all(|u| u.key != upload.key));
    // Finished.
    let done = store.begin().await.unwrap();
    put_part(store, &done, 1, content(2, 10)).await;
    store
        .complete(&done, sha256(&content(2, 10)), 10)
        .await
        .unwrap();
    store.delete(&done.key).await.unwrap();
    assert!(matches!(
        store.get(&done.key).await,
        Err(StoreError::NotFound)
    ));
    // Twice, and a key that never was.
    store.delete(&done.key).await.unwrap();
    let never = ObjectKey::parse("5c0f3c2e-0a8e-4b7e-9d0c-3b5f6a1d2e4f").unwrap();
    store.delete(&never).await.unwrap();
    run.stop().await;
}

/// The unfinished uploads are listed with their keys and times, all of them.
#[tokio::test]
async fn unfinished_uploads_are_listed() {
    let run = Running::start().await;
    let store = &run.store;
    let mut begun = Vec::new();
    for _ in 0..3 {
        begun.push(store.begin().await.unwrap());
    }
    let listed = store.list_unfinished().await.unwrap();
    assert_eq!(listed.skipped, 0);
    for upload in &begun {
        let found = listed
            .uploads
            .iter()
            .find(|u| u.key == upload.key)
            .expect("listed");
        assert_eq!(found.id, upload.id);
        assert!(found.initiated > std::time::UNIX_EPOCH);
    }
    run.stop().await;
}

/// Presigned URLs bind what the signature covers; the object server refuses the rest.
#[tokio::test]
async fn an_edited_part_url_and_an_anonymous_read_are_refused() {
    let run = Running::start().await;
    let store = &run.store;
    let upload = store.begin().await.unwrap();
    let url = store.presign_part(&upload, 1).unwrap();
    let edited = url.expose().replace("partNumber=1", "partNumber=2");
    assert_ne!(edited, url.expose());
    let (status, _) = put(&edited, b"x".to_vec()).await;
    assert_eq!(status, 403);
    // The URL itself works.
    let (status, _) = put(url.expose(), b"x".to_vec()).await;
    assert_eq!(status, 200);
    // The object itself is not readable without a signature.
    let port = run.server.as_ref().unwrap().port();
    let bare = format!("http://127.0.0.1:{port}/nekoguruma/{}", upload.key);
    let (status, _) = request("GET", &bare, Vec::new()).await;
    assert_eq!(status, 403);
    run.stop().await;
}

/// The bucket is created at the first open, found at the next, and each name is a bucket of its
/// own.
#[tokio::test]
async fn the_bucket_is_created_once_and_names_are_separate_buckets() {
    let run = Running::start().await;
    let server = run.server.as_ref().unwrap();
    let again = open(server, "nekoguruma").await;
    let other = open(server, "another-bucket").await;
    let upload = run.store.begin().await.unwrap();
    put_part(&run.store, &upload, 1, content(1, 4)).await;
    // The second handle on the same bucket sees the upload; the other bucket does not.
    assert_eq!(again.list_parts(&upload).await.unwrap().len(), 1);
    assert!(matches!(
        other.list_parts(&upload).await,
        Err(StoreError::NotFound)
    ));
    run.stop().await;
}

/// A bucket that was created in an earlier run is found after a restart on the same data.
#[tokio::test]
async fn an_upload_survives_a_restart_of_the_object_server() {
    let run = Running::start().await;
    let upload = run.store.begin().await.unwrap();
    let data = content(3, 77);
    put_part(&run.store, &upload, 1, data.clone()).await;
    let Running {
        real,
        mut server,
        store,
    } = run;
    server.take().unwrap().stop(std::future::pending()).await;
    drop(store);
    let server = real.start().await;
    let store = open(&server, "nekoguruma").await;
    assert_eq!(store.list_parts(&upload).await.unwrap().len(), 1);
    store.complete(&upload, sha256(&data), 77).await.unwrap();
    assert_eq!(read_all(&store, &upload.key).await.unwrap(), data);
    server.stop(std::future::pending()).await;
}

// ---------------------------------------------------------------------------------------------
// The whole server

fn server_config(pg: &TestPostgres, real: &Real) -> Config {
    let mut database = Database::new(pg.url());
    database.connect_timeout_secs = 5;
    Config {
        profile: Profile::Local,
        http: Http {
            port: 0,
            shutdown_grace_secs: 2,
        },
        database,
        instruction_key: InstructionKeyConfig {
            path: pg.key_path(),
        },
        object_server: Some(real.config.clone()),
    }
}

/// `Server::start` opens the store on the real child; a restart finds the bucket and the object.
#[tokio::test]
async fn a_start_opens_the_store_and_a_restart_finds_what_it_held() {
    let pg = TestPostgres::start();
    let real = Real::new();
    let config = server_config(&pg, &real);
    let server = tokio::time::timeout(Duration::from_secs(90), server::Server::start(&config))
        .await
        .expect("the start must not hang")
        .expect("the server must start");
    assert_ne!(server.local_addr().port(), 0);
    let store = server.object_store().expect("start opens the store");
    let upload = store.begin().await.unwrap();
    let data = content(9, 50);
    let url = store.presign_part(&upload, 1).unwrap();
    assert_eq!(put(url.expose(), data.clone()).await.0, 200);
    store.complete(&upload, sha256(&data), 50).await.unwrap();
    server.run(async {}, std::future::pending()).await.unwrap();
    drop(store);
    let server = tokio::time::timeout(Duration::from_secs(90), server::Server::start(&config))
        .await
        .expect("the start must not hang")
        .expect("the server must start again");
    let store = server.object_store().unwrap();
    let body = store.get(&upload.key).await.unwrap();
    assert_eq!(body.length, 50);
    server.run(async {}, std::future::pending()).await.unwrap();
}

/// A server made by `bind`, or built with no `[object_server]`, has no store.
#[tokio::test]
async fn a_config_without_an_object_server_has_no_store() {
    let pg = TestPostgres::start();
    let real = Real::new();
    let mut config = server_config(&pg, &real);
    config.object_server = None;
    let server = server::Server::start(&config).await.unwrap();
    assert!(server.object_store().is_none());
    server.run(async {}, std::future::pending()).await.unwrap();
}

//! The object store on the real object server (ADR-492): the pinned RustFS as the server's child
//! (`NGR_TEST_RUSTFS_BIN`, as `object_server_rustfs.rs` has it), a bucket, parts PUT to presigned
//! URLs by an HTTP client that trusts the pin, completion and its read-back, deletion and listing. The scripted
//! units are in `src/storage/s3/tests.rs`; the local store's are in `src/storage/local/tests.rs`.
//!
//! The tests that start the whole server also need PostgreSQL (the start order's earlier steps).

use std::time::Duration;

use ngr_pinned_tls::CertPin;
use server::config::{Config, Database, Http, InstructionKeyConfig, Profile};
use server::object_server::ObjectServer;
use server::storage::{MultipartStore, ObjectKey, S3Store, S3Tuning, StoreError, Upload};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::object_server_rustfs::Real;
use crate::redeem::capture;
use crate::support::TestPostgres;

pub(crate) const MIB: usize = 1024 * 1024;

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

/// Deterministic content of `len` bytes that differs from part to part.
pub(crate) fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A PUT of `body` over TLS to a presigned URL, trusting the certificate `pin` names: the status
/// and the answer's `etag`.
pub(crate) async fn put(url: &str, pin: &CertPin, body: Vec<u8>) -> (u16, Option<String>) {
    request("PUT", url, pin, body).await
}

/// A PUT of `body` to the URL of a part-URL answer `r` (a 200), trusting the certificate its
/// `certificatePin` names.
pub(crate) async fn put_answer(r: &crate::serving::Reply, body: Vec<u8>) -> (u16, Option<String>) {
    assert_eq!(r.status, 200, "{}", r.head);
    let answer = crate::redeem::json(r);
    let pin = CertPin::from_hex(answer["certificatePin"].as_str().unwrap()).expect("a pin");
    put(answer["url"].as_str().unwrap(), &pin, body).await
}

async fn request(method: &str, url: &str, pin: &CertPin, body: Vec<u8>) -> (u16, Option<String>) {
    let (status, etag, _) = crate::support::tls_request(method, url, pin, body).await;
    (status, etag)
}

/// Uploads `data` as part `number` of `upload`.
async fn put_part(store: &S3Store, upload: &Upload, number: u16, data: Vec<u8>) {
    let url = store.presign_part(upload, number).unwrap();
    let (status, etag) = put(url.expose(), url.pin(), data).await;
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
        let mut real = Real::new();
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
            server.certificate_pin(),
            bucket,
            S3Tuning::default(),
        ),
    )
    .await
    .expect("open must not hang")
    .unwrap_or_else(|error| panic!("the store must open: {error}"))
}

/// A store on the same object server whose listings are one entry a page, so that every
/// continuation marker is signed and sent.
async fn paging_store(run: &Running) -> S3Store {
    let tuning = S3Tuning {
        page_size: 1,
        ..S3Tuning::default()
    };
    S3Store::open(
        run.server.as_ref().unwrap().port(),
        run.server.as_ref().unwrap().credentials(),
        run.server.as_ref().unwrap().certificate_pin(),
        "nekoguruma",
        tuning,
    )
    .await
    .unwrap()
}

pub(crate) async fn read_all(store: &S3Store, key: &ObjectKey) -> Result<Vec<u8>, StoreError> {
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

/// The store sends a part itself, as a document's route does (ADR-500): one small part that is
/// the whole object, and a first part of the minimum size with a small last one.
#[tokio::test]
async fn the_store_sends_a_part_itself_and_completes_it() {
    let run = Running::start().await;
    let store = &run.store;

    let small = content(7, 1234);
    let upload = store.begin().await.unwrap();
    store.put_part(&upload, 1, small.clone()).await.unwrap();
    // Sending a part again replaces it.
    store.put_part(&upload, 1, small.clone()).await.unwrap();
    store
        .complete(&upload, sha256(&small), small.len() as u64)
        .await
        .unwrap();
    assert_eq!(read_all(store, &upload.key).await.unwrap(), small);

    let first = content(8, 5 * MIB);
    let last = content(9, 77);
    let upload = store.begin().await.unwrap();
    store.put_part(&upload, 1, first.clone()).await.unwrap();
    store.put_part(&upload, 2, last.clone()).await.unwrap();
    let whole = [first, last].concat();
    store
        .complete(&upload, sha256(&whole), whole.len() as u64)
        .await
        .unwrap();
    assert_eq!(read_all(store, &upload.key).await.unwrap(), whole);

    // An upload that is gone takes no part.
    let gone = store.begin().await.unwrap();
    store.delete(&gone.key).await.unwrap();
    assert!(store.put_part(&gone, 1, vec![1]).await.is_err());
    run.stop().await;
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
        Err(StoreError::PartsLengthMismatch { stored }) if stored == (5 * MIB + 10) as u64
    ));
    // The refusal left the upload as it was: its parts are still listed.
    assert_eq!(store.list_parts(&upload).await.unwrap().len(), 2);
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
    let (status, _) = put(&edited, url.pin(), b"x".to_vec()).await;
    assert_eq!(status, 403);
    // The URL itself works.
    let (status, _) = put(url.expose(), url.pin(), b"x".to_vec()).await;
    assert_eq!(status, 200);
    // The object itself is not readable without a signature.
    let port = run.server.as_ref().unwrap().port();
    let bare = format!("https://127.0.0.1:{port}/nekoguruma/{}", upload.key);
    let (status, _) = request("GET", &bare, url.pin(), Vec::new()).await;
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
        mut real,
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

/// `Server::start` on `real`'s configuration, with a fresh object-server port when the port is
/// lost to another process; the port that served is kept in `real`.
async fn start_whole(pg: &TestPostgres, real: &mut Real) -> server::Server {
    let (result, port) = tokio::time::timeout(
        Duration::from_secs(90 * crate::support::PORT_ATTEMPTS as u64),
        crate::support::retry_on_lost_port(
            |port| {
                let mut config = server_config(pg, real);
                config.object_server.as_mut().unwrap().port = Some(port);
                async move { server::Server::start(&config).await }
            },
            |error, port| {
                matches!(error, server::StartError::ObjectServer(error)
                    if crate::support::object_port_lost(error, port))
            },
        ),
    )
    .await
    .expect("the start must not hang");
    real.config.port = Some(port);
    result.expect("the server must start")
}

/// `Server::start` opens the store on the real child; a restart finds the bucket and the object.
#[tokio::test]
async fn a_start_opens_the_store_and_a_restart_finds_what_it_held() {
    let pg = TestPostgres::start();
    let mut real = Real::new();
    let server = start_whole(&pg, &mut real).await;
    assert_ne!(server.local_addr().port(), 0);
    let store = server.object_store().expect("start opens the store");
    let upload = store.begin().await.unwrap();
    let data = content(9, 50);
    let url = store.presign_part(&upload, 1).unwrap();
    assert_eq!(put(url.expose(), url.pin(), data.clone()).await.0, 200);
    store.complete(&upload, sha256(&data), 50).await.unwrap();
    server.run(async {}, std::future::pending()).await.unwrap();
    drop(store);
    let server = start_whole(&pg, &mut real).await;
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

/// Listing parts one a page sends the part-number marker, which the object server checks
/// against the signature.
#[tokio::test]
async fn parts_are_listed_one_page_at_a_time_against_the_real_server() {
    let run = Running::start().await;
    let store = paging_store(&run).await;
    let upload = store.begin().await.unwrap();
    for number in 1..=3u16 {
        put_part(
            &store,
            &upload,
            number,
            content(number as u8, 10 * number as usize),
        )
        .await;
    }
    let parts = store.list_parts(&upload).await.unwrap();
    assert_eq!(
        parts.iter().map(|p| (p.number, p.size)).collect::<Vec<_>>(),
        [(1, 10), (2, 20), (3, 30)]
    );
    run.stop().await;
}

/// Three uploads, two of them of one key, listed one a page: the key and upload-ID markers are
/// signed and sent, and deleting the doubled key aborts both of its uploads.
#[tokio::test]
async fn unfinished_uploads_are_listed_one_page_at_a_time_against_the_real_server() {
    let run = Running::start().await;
    let store = paging_store(&run).await;
    let first = store.begin().await.unwrap();
    let other = store.begin().await.unwrap();
    // A second upload of the first key, started by hand.
    let server = run.server.as_ref().unwrap();
    let credentials = rusty_s3::Credentials::new(
        server.credentials().access_key(),
        server.credentials().expose_secret(),
    );
    let bucket = rusty_s3::Bucket::new(
        format!("https://127.0.0.1:{}", server.port())
            .parse()
            .unwrap(),
        rusty_s3::UrlStyle::Path,
        "nekoguruma",
        "us-east-1",
    )
    .unwrap();
    let url = rusty_s3::S3Action::sign(
        &bucket.create_multipart_upload(Some(&credentials), first.key.as_str()),
        Duration::from_secs(60),
    );
    assert_eq!(
        request("POST", url.as_str(), server.certificate_pin(), Vec::new())
            .await
            .0,
        200
    );
    let listed = store.list_unfinished().await.unwrap();
    assert_eq!(listed.skipped, 0);
    let of_first: Vec<_> = listed
        .uploads
        .iter()
        .filter(|u| u.key == first.key)
        .collect();
    assert_eq!(of_first.len(), 2);
    assert_ne!(of_first[0].id, of_first[1].id);
    assert!(listed.uploads.iter().any(|u| u.key == other.key));
    assert_eq!(listed.uploads.len(), 3);
    store.delete(&first.key).await.unwrap();
    let after = store.list_unfinished().await.unwrap();
    assert_eq!(after.uploads.len(), 1);
    assert_eq!(after.uploads[0].key, other.key);
    run.stop().await;
}

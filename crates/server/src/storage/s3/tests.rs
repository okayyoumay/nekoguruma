//! The S3 store's units (ADR-492), against a scripted loopback listener that stands in for the
//! object server: it records every request and answers as the test says. They need no database
//! and no RustFS, so every platform's CI runs them. The real object server is tested in
//! `tests/postgres/storage_s3.rs`.

use std::io::ErrorKind;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::*;

// ---------------------------------------------------------------------------------------------
// The scripted listener

/// One request the listener received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// The parameters of the query that are not the signature's, in the order sent.
    fn params(&self) -> Vec<&str> {
        self.target
            .split_once('?')
            .map(|(_, query)| {
                query
                    .split('&')
                    .filter(|p| !p.starts_with("X-Amz-"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The raw (percent-encoded) value of a query parameter.
    fn param(&self, name: &str) -> Option<&str> {
        self.target
            .split_once('?')?
            .1
            .split('&')
            .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
    }

    fn has_param(&self, name: &str) -> bool {
        self.target
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|p| p.split('=').next() == Some(name)))
    }

    /// Which S3 operation the request is.
    fn op(&self) -> &'static str {
        let bucket = self.path() == "/nekoguruma/" || self.path() == "/nekoguruma";
        match (self.method.as_str(), bucket) {
            ("HEAD", true) => "head-bucket",
            ("PUT", true) => "create-bucket",
            ("GET", true) if self.has_param("uploads") => "list-uploads",
            ("POST", false) if self.has_param("uploads") => "create-upload",
            ("POST", false) if self.has_param("uploadId") => "complete",
            ("GET", false) if self.has_param("uploadId") => "list-parts",
            ("PUT", false) if self.has_param("uploadId") => "upload-part",
            ("DELETE", false) if self.has_param("uploadId") => "abort",
            ("HEAD", false) => "head-object",
            ("GET", false) => "get-object",
            ("DELETE", false) => "delete-object",
            _ => "unknown",
        }
    }
}

/// What the listener does with a request.
enum Step {
    /// A complete answer.
    Reply { status: u16, body: Vec<u8> },
    /// An answer whose body is `length` bytes long, of which `send` is sent; then the connection is
    /// held open (`hold`) or closed.
    Stream {
        status: u16,
        length: usize,
        send: Vec<u8>,
        hold: bool,
    },
    /// Reads the request and never answers.
    Hang,
    /// Waits for a permit, then does the step.
    After(Arc<Semaphore>, Box<Step>),
}

fn reply(status: u16, body: impl Into<Vec<u8>>) -> Step {
    Step::Reply {
        status,
        body: body.into(),
    }
}

fn error_body(code: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
         <Message>SERVER-SAID-THIS</Message><Resource>RESOURCE-TEXT</Resource></Error>"
    )
}

type Responder = dyn Fn(&Seen, usize) -> Step + Send + Sync;

/// The listener: every request is recorded, then answered by `responder(request, index)`.
struct Fake {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    async fn start(responder: impl Fn(&Seen, usize) -> Step + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let responder: Arc<Responder> = Arc::new(responder);
        let recorded = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let responder = Arc::clone(&responder);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let _ = serve(stream, responder, recorded).await;
                });
            }
        });
        Fake { port, seen, task }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn ops(&self) -> Vec<&'static str> {
        self.seen().iter().map(Seen::op).collect()
    }
}

async fn serve(
    mut stream: TcpStream,
    responder: Arc<Responder>,
    recorded: Arc<Mutex<Vec<Seen>>>,
) -> std::io::Result<()> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        data.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let seen = Seen {
        method,
        target,
        headers,
        body: data[head_end..].to_vec(),
    };
    let index = {
        let mut all = recorded.lock().unwrap();
        all.push(seen.clone());
        all.len() - 1
    };
    let mut step = responder(&seen, index);
    loop {
        match step {
            Step::After(gate, next) => {
                gate.acquire().await.unwrap().forget();
                step = *next;
            }
            Step::Reply { status, body } => {
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await?;
                if seen.method != "HEAD" {
                    stream.write_all(&body).await?;
                }
                stream.shutdown().await?;
                return Ok(());
            }
            Step::Stream {
                status,
                length,
                send,
                hold,
            } => {
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
                );
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&send).await?;
                stream.flush().await?;
                if hold {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                return Ok(());
            }
            Step::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

/// The credentials of a store under test.
fn credentials() -> Credentials {
    let dir = tempfile::tempdir().unwrap();
    Credentials::load_or_create(dir.path()).unwrap()
}

/// Bounds that keep a test short: 10-byte parts, 1000-byte objects, short waits.
fn fast() -> Tuning {
    Tuning {
        ready_timeout: Duration::from_millis(600),
        retry_interval: Duration::from_millis(10),
        request_timeout: Duration::from_secs(5),
        read_idle_timeout: Duration::from_millis(300),
        url_expiry: Duration::from_secs(60),
        page_size: 1000,
        max_xml_bytes: 64 * 1024,
        max_error_bytes: 4 * 1024,
        min_part_bytes: 10,
        max_object_bytes: 1000,
    }
}

fn store_with(port: u16, tuning: Tuning) -> S3Store {
    store_with_credentials(port, &credentials(), tuning)
}

fn store_with_credentials(port: u16, creds: &Credentials, tuning: Tuning) -> S3Store {
    S3Store {
        inner: Arc::new(Inner::new(port, creds, "nekoguruma", tuning).unwrap()),
    }
}

fn store(fake: &Fake) -> S3Store {
    store_with(fake.port, fast())
}

fn upload() -> Upload {
    Upload {
        key: ObjectKey::generate().unwrap(),
        id: UploadId::parse("upload-id-1").unwrap(),
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

const NS: &str = "xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"";

fn parts_xml(parts: &[(u16, u64)], next: Option<u16>) -> String {
    let mut text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListPartsResult {NS}><Bucket>b</Bucket>\
         <Key>k</Key><UploadId>u</UploadId><MaxParts>1000</MaxParts>\
         <IsTruncated>{}</IsTruncated>",
        next.is_some()
    );
    if let Some(next) = next {
        text.push_str(&format!(
            "<NextPartNumberMarker>{next}</NextPartNumberMarker>"
        ));
    }
    for (number, size) in parts {
        text.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber>\
             <LastModified>2026-11-18T00:00:00.000Z</LastModified>\
             <ETag>\"etag-{number}\"</ETag><Size>{size}</Size></Part>"
        ));
    }
    text.push_str("</ListPartsResult>");
    text
}

fn complete_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUploadResult {NS}>\
         <Location>l</Location><Bucket>b</Bucket><Key>k</Key><ETag>\"done\"</ETag>\
         </CompleteMultipartUploadResult>"
    )
}

fn uploads_xml(entries: &[(&str, &str)], next: Option<(&str, &str)>) -> String {
    let mut text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListMultipartUploadsResult {NS}>\
         <Bucket>b</Bucket><KeyMarker></KeyMarker><UploadIdMarker></UploadIdMarker>\
         <MaxUploads>1000</MaxUploads><IsTruncated>{}</IsTruncated>",
        next.is_some()
    );
    if let Some((key, id)) = next {
        text.push_str(&format!(
            "<NextKeyMarker>{key}</NextKeyMarker><NextUploadIdMarker>{id}</NextUploadIdMarker>"
        ));
    }
    for (key, id) in entries {
        text.push_str(&format!(
            "<Upload><Key>{key}</Key><UploadId>{id}</UploadId>\
             <Initiator><ID>x</ID></Initiator><StorageClass>STANDARD</StorageClass>\
             <Initiated>2026-11-18T10:00:00.000Z</Initiated></Upload>"
        ));
    }
    text.push_str("</ListMultipartUploadsResult>");
    text
}

fn create_xml(id: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><InitiateMultipartUploadResult {NS}>\
         <Bucket>b</Bucket><Key>k</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
    )
}

fn empty_uploads() -> String {
    uploads_xml(&[], None)
}

async fn read_all(body: ObjectBody) -> std::io::Result<Vec<u8>> {
    let mut reader = body.reader;
    let mut out = Vec::new();
    reader.read_to_end(&mut out).await?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Open: the bucket

#[tokio::test]
async fn open_finds_an_existing_bucket_with_one_head() {
    let fake = Fake::start(|_, _| reply(200, "")).await;
    let _store = S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap();
    assert_eq!(fake.ops(), ["head-bucket"]);
}

#[tokio::test]
async fn open_creates_an_absent_bucket_with_an_empty_body_and_content_length_zero() {
    let fake = Fake::start(|seen, _| match seen.op() {
        "head-bucket" => reply(404, ""),
        _ => reply(200, ""),
    })
    .await;
    S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap();
    assert_eq!(fake.ops(), ["head-bucket", "create-bucket"]);
    let create = &fake.seen()[1];
    assert_eq!(create.header("content-length"), Some("0"));
    assert!(create.body.is_empty());
    assert_eq!(create.header("connection"), Some("close"));
    assert_eq!(
        create.header("host"),
        Some(format!("127.0.0.1:{}", fake.port).as_str())
    );
    assert!(create.target.contains("X-Amz-Signature="));
}

#[tokio::test]
async fn a_bucket_that_is_ours_already_counts_as_present_and_another_owner_is_refused() {
    let ours = Fake::start(|seen, _| match seen.op() {
        "head-bucket" => reply(404, ""),
        _ => reply(409, error_body("BucketAlreadyOwnedByYou")),
    })
    .await;
    S3Store::open(ours.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap();
    let theirs = Fake::start(|seen, _| match seen.op() {
        "head-bucket" => reply(404, ""),
        _ => reply(409, error_body("BucketAlreadyExists")),
    })
    .await;
    let error = S3Store::open(theirs.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Refused { .. }), "{error}");
    assert_eq!(theirs.ops(), ["head-bucket", "create-bucket"]);
}

#[tokio::test]
async fn a_refused_head_ends_the_open_at_once() {
    for status in [401, 403] {
        let fake = Fake::start(move |_, _| reply(status, error_body("AccessDenied"))).await;
        let error = S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::Refused { .. }), "{error}");
        assert_eq!(fake.seen().len(), 1);
    }
    // An unexpected 4xx is no reason to retry either.
    let fake = Fake::start(|_, _| reply(400, "")).await;
    let error = S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 400, .. }),
        "{error}"
    );
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test]
async fn a_server_that_is_not_up_yet_is_retried_until_it_is() {
    let fake = Fake::start(|_, index| {
        if index < 3 {
            reply(503, "")
        } else {
            reply(200, "")
        }
    })
    .await;
    S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap();
    assert_eq!(fake.ops().len(), 4);
}

#[tokio::test]
async fn a_server_that_stays_down_fails_when_the_budget_is_spent() {
    let fake = Fake::start(|_, _| reply(503, "")).await;
    let began = std::time::Instant::now();
    let error = S3Store::open(fake.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 503, .. }),
        "{error}"
    );
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(fake.seen().len() > 2, "retried");
    // A listener that never answers is cut by the budget too, and the error is an I/O one.
    let hang = Fake::start(|_, _| Step::Hang).await;
    let error = S3Store::open(hang.port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Io {
                kind: ErrorKind::TimedOut,
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_port_nothing_listens_on_fails_with_an_io_error() {
    // A socket that is bound and never listens refuses connects and keeps the port taken (the
    // refusal of a connect to a closed port takes seconds on Windows, so there the budget is
    // what ends this).
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .bind((std::net::Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let error = S3Store::open(port, &credentials(), "nekoguruma", fast())
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Io { .. }), "{error}");
    #[cfg(unix)]
    assert!(
        matches!(
            error,
            StoreError::Io {
                kind: ErrorKind::ConnectionRefused,
                ..
            }
        ),
        "{error}"
    );
    // The same port starts listening after a while: the refused connects were retried.
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listener = socket.listen(16).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await;
        stream
            .write_all(b"HTTP/1.1 200 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let mut tuning = fast();
    tuning.ready_timeout = Duration::from_secs(5);
    S3Store::open(port, &credentials(), "nekoguruma", tuning)
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn a_bucket_name_the_store_does_not_accept_is_refused_before_any_request() {
    let fake = Fake::start(|_, _| reply(200, "")).await;
    for bad in ["", "ab", "Upper", "-x-", "health", "a/b"] {
        let error = S3Store::open(fake.port, &credentials(), bad, fast())
            .await
            .unwrap_err();
        assert!(
            matches!(error, StoreError::Refused { .. }),
            "{bad}: {error}"
        );
    }
    assert!(fake.seen().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The HTTP rules

#[tokio::test]
async fn begin_sends_an_empty_post_with_content_length_zero_and_reads_the_id() {
    let fake = Fake::start(|_, _| reply(200, create_xml("abc123"))).await;
    let store = store(&fake);
    let started = store.begin().await.unwrap();
    assert_eq!(started.id.expose(), "abc123");
    let seen = &fake.seen()[0];
    assert_eq!(seen.op(), "create-upload");
    assert_eq!(seen.header("content-length"), Some("0"));
    assert!(seen.path().ends_with(started.key.as_str()));
    assert_eq!(seen.header("connection"), Some("close"));
}

#[tokio::test]
async fn every_request_uses_a_connection_of_its_own() {
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-uploads" => reply(200, empty_uploads()),
        _ => reply(204, ""),
    })
    .await;
    let store = store(&fake);
    let key = ObjectKey::generate().unwrap();
    store.delete(&key).await.unwrap();
    store.delete(&key).await.unwrap();
    assert_eq!(fake.seen().len(), 4);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let fake = Fake::start(|_, _| reply(307, "")).await;
    let store = store(&fake);
    let error = store.begin().await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 307, .. }),
        "{error}"
    );
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test]
async fn a_control_request_is_bounded_by_the_request_timeout() {
    let fake = Fake::start(|_, _| Step::Hang).await;
    let mut tuning = fast();
    tuning.request_timeout = Duration::from_millis(200);
    let store = store_with(fake.port, tuning);
    let began = std::time::Instant::now();
    let error = store.begin().await.unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Io {
                kind: ErrorKind::TimedOut,
                ..
            }
        ),
        "{error}"
    );
    assert!(began.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn an_xml_answer_over_the_bound_is_refused_and_an_error_body_over_its_bound_is_cut() {
    // A valid answer padded past the XML bound (64 KiB here) with a comment: it would parse
    // without the bound.
    let padded = create_xml("abc123").replace(
        "<Bucket>",
        &format!("<!-- {} --><Bucket>", "x".repeat(100 * 1024)),
    );
    assert!(CreateMultipartUpload::parse_response(&padded).is_ok());
    let fake = Fake::start(move |_, _| reply(200, padded.clone())).await;
    let error = store(&fake).begin().await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 200, .. }),
        "{error}"
    );
    // An error answer that promises 10 MB and holds the connection after 8 KiB: with the 4 KiB
    // error bound it is a quick status; without it the read would wait for the control timeout.
    let fake = Fake::start(|_, _| Step::Stream {
        status: 500,
        length: 10_000_000,
        send: vec![b'x'; 8 * 1024],
        hold: true,
    })
    .await;
    let mut tuning = fast();
    tuning.request_timeout = Duration::from_secs(2);
    let began = std::time::Instant::now();
    let error = store_with(fake.port, tuning)
        .delete(&ObjectKey::generate().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 500, .. }),
        "{error}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn a_garbled_answer_is_a_server_error_not_a_panic() {
    for body in ["", "not xml", "<a/>", "<InitiateMultipartUploadResult/>"] {
        let fake = Fake::start(move |_, _| reply(200, body)).await;
        let store = store(&fake);
        let error = store.begin().await.unwrap_err();
        assert!(
            matches!(error, StoreError::Server { status: 200, .. }),
            "{body}: {error}"
        );
    }
    // An upload ID that is not printable ASCII is not taken.
    let fake = Fake::start(|_, _| reply(200, create_xml("has space"))).await;
    assert!(store(&fake).begin().await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Nothing secret is formatted

#[tokio::test]
async fn errors_hold_no_url_key_upload_id_or_server_text() {
    let fake = Fake::start(|_, _| {
        reply(
            500,
            error_body("SERVER-CODE").replace(
                "SERVER-SAID-THIS",
                "SERVER-SAID-THIS upload-id-1 X-Amz-Signature=abc",
            ),
        )
    })
    .await;
    let creds = credentials();
    let store = store_with_credentials(fake.port, &creds, fast());
    let up = upload();
    let secret = creds.expose_secret().to_owned();
    let mut errors = vec![store.begin().await.unwrap_err()];
    errors.push(store.list_parts(&up).await.unwrap_err());
    errors.push(store.complete(&up, [0; 32], 1).await.unwrap_err());
    errors.push(store.get(&up.key).await.unwrap_err());
    errors.push(store.delete(&up.key).await.unwrap_err());
    errors.push(store.list_unfinished().await.unwrap_err());
    for error in errors {
        let shown = format!("{error} / {error:?}");
        for forbidden in [
            "SERVER-SAID-THIS",
            "SERVER-CODE",
            "RESOURCE-TEXT",
            "X-Amz",
            "upload-id-1",
            up.key.as_str(),
            "127.0.0.1",
            "http",
            secret.as_str(),
        ] {
            assert!(!shown.contains(forbidden), "{forbidden} in {shown}");
        }
    }
}

#[test]
fn debug_of_a_part_url_an_upload_id_and_the_store_shows_nothing() {
    let store = store_with(1, fast());
    let up = upload();
    let url = store.presign_part(&up, 1).unwrap();
    for shown in [
        format!("{url:?}"),
        format!("{:?}", up.id),
        format!("{up:?}"),
        format!("{store:?}"),
        format!("{:?}", fast()),
    ] {
        assert!(!shown.contains("X-Amz"), "{shown}");
        assert!(!shown.contains("upload-id-1"), "{shown}");
        assert!(!shown.contains("Signature"), "{shown}");
    }
    assert_eq!(format!("{url:?}"), "PartUrl(..)");
}

// ---------------------------------------------------------------------------------------------
// Part URLs and upload IDs

#[test]
fn a_part_url_is_signed_for_five_minutes_and_binds_the_part_without_io() {
    // Nothing listens on this port: presigning does no I/O.
    let store = store_with(1, fast());
    let up = upload();
    let url = store.presign_part(&up, 7).unwrap();
    let text = url.expose();
    assert!(text.starts_with("http://127.0.0.1:1/nekoguruma/"), "{text}");
    assert!(text.contains(up.key.as_str()));
    assert!(text.contains("X-Amz-Expires=300"));
    assert!(text.contains("partNumber=7"));
    assert!(text.contains("uploadId=upload-id-1"));
    assert!(text.contains("X-Amz-Signature="));
    // The part number is covered by the signature: another part has another one.
    let other = store.presign_part(&up, 8).unwrap();
    let signature = |u: &PartUrl| {
        u.expose()
            .split("X-Amz-Signature=")
            .nth(1)
            .unwrap()
            .to_owned()
    };
    assert_ne!(signature(&url), signature(&other));
}

#[test]
fn part_numbers_outside_1_to_10000_are_refused() {
    let store = store_with(1, fast());
    let up = upload();
    for bad in [0u16, 10_001, u16::MAX] {
        assert!(matches!(
            store.presign_part(&up, bad),
            Err(StoreError::InvalidPartNumber)
        ));
    }
    for good in [1u16, 2, 9_999, 10_000] {
        assert!(store.presign_part(&up, good).is_ok(), "{good}");
    }
}

#[test]
fn an_upload_id_is_one_to_1024_printable_ascii_characters() {
    assert!(UploadId::parse("a").is_ok());
    assert!(UploadId::parse(&"x".repeat(1024)).is_ok());
    for bad in ["", "a b", "tab\t", "nul\0", "é", "\n"] {
        assert!(
            matches!(UploadId::parse(bad), Err(StoreError::InvalidUploadId)),
            "{bad:?}"
        );
    }
    assert!(UploadId::parse(&"x".repeat(1025)).is_err());
}

// ---------------------------------------------------------------------------------------------
// Listing the parts

#[tokio::test]
async fn parts_are_listed_page_by_page_without_their_tags() {
    let fake = Fake::start(|seen, _| match seen.param("part-number-marker") {
        None => reply(200, parts_xml(&[(1, 10), (2, 10)], Some(2))),
        Some("2") => reply(200, parts_xml(&[(3, 7)], None)),
        Some(_) => reply(500, ""),
    })
    .await;
    let mut tuning = fast();
    tuning.page_size = 2;
    let store = store_with(fake.port, tuning);
    let parts = store.list_parts(&upload()).await.unwrap();
    assert_eq!(
        parts,
        [
            Part {
                number: 1,
                size: 10
            },
            Part {
                number: 2,
                size: 10
            },
            Part { number: 3, size: 7 }
        ]
    );
    assert_eq!(fake.seen()[0].param("max-parts"), Some("2"));
}

#[tokio::test]
async fn a_listing_that_goes_backwards_repeats_or_overflows_is_parts_changed() {
    let cases: Vec<(&str, String)> = vec![
        ("descending", parts_xml(&[(2, 10), (1, 10)], None)),
        ("duplicate", parts_xml(&[(1, 10), (1, 10)], None)),
        ("number 0", parts_xml(&[(0, 10)], None)),
        ("number 10001", parts_xml(&[(10_001, 10)], None)),
        ("marker does not move", parts_xml(&[], Some(1))),
    ];
    for (name, xml) in cases {
        let fake = Fake::start(move |_, _| reply(200, xml.clone())).await;
        let error = store(&fake).list_parts(&upload()).await.unwrap_err();
        assert!(matches!(error, StoreError::PartsChanged), "{name}: {error}");
    }
}

#[tokio::test]
async fn a_gone_upload_is_already_finished_when_the_object_exists_else_not_found() {
    for (head, expected_finished) in [(200u16, true), (404, false)] {
        let fake = Fake::start(move |seen, _| match seen.op() {
            "list-parts" => reply(404, error_body("NoSuchUpload")),
            _ => reply(head, ""),
        })
        .await;
        let error = store(&fake).list_parts(&upload()).await.unwrap_err();
        assert_eq!(
            matches!(error, StoreError::AlreadyFinished),
            expected_finished,
            "{error}"
        );
        assert_eq!(
            matches!(error, StoreError::NotFound),
            !expected_finished,
            "{error}"
        );
        assert_eq!(fake.ops(), ["list-parts", "head-object"]);
    }
    // A 404 that is not a missing upload is not read as one.
    let fake = Fake::start(|_, _| reply(404, error_body("NoSuchBucket"))).await;
    let error = store(&fake).list_parts(&upload()).await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 404, .. }),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Listing the unfinished uploads

#[tokio::test]
async fn unfinished_uploads_are_listed_with_their_markers_and_foreign_ones_are_counted() {
    let k1 = ObjectKey::generate().unwrap();
    let k2 = ObjectKey::generate().unwrap();
    let (k1t, k2t) = (k1.to_string(), k2.to_string());
    let fake = Fake::start({
        let (k1t, k2t) = (k1t.clone(), k2t.clone());
        move |seen, _| match seen.param("key-marker") {
            None => reply(
                200,
                uploads_xml(
                    &[(&k1t, "id-1"), ("someone-elses-key", "id-x")],
                    Some((&k1t, "id-1")),
                ),
            ),
            Some(marker) if marker == k1t => {
                assert_eq!(seen.param("upload-id-marker"), Some("id-1"));
                reply(200, uploads_xml(&[(&k2t, "id-2"), (&k2t, "bad id")], None))
            }
            Some(_) => reply(500, ""),
        }
    })
    .await;
    let listed = store(&fake).list_unfinished().await.unwrap();
    assert_eq!(listed.skipped, 2);
    let keys: Vec<&ObjectKey> = listed.uploads.iter().map(|u| &u.key).collect();
    assert_eq!(keys, [&k1, &k2]);
    assert_eq!(listed.uploads[0].id.expose(), "id-1");
    assert!(listed.uploads[0].initiated > std::time::UNIX_EPOCH);
    // The signed query is sorted by name: the object server verifies it that way.
    let first = &fake.seen()[0];
    assert_eq!(first.params(), ["max-uploads=1000", "uploads="]);
    let second = &fake.seen()[1];
    assert_eq!(
        second.params(),
        [
            format!("key-marker={k1t}"),
            "max-uploads=1000".to_owned(),
            "upload-id-marker=id-1".to_owned(),
            "uploads=".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_listing_that_does_not_advance_or_never_ends_is_a_server_error() {
    let key = ObjectKey::generate().unwrap().to_string();
    let k = key.clone();
    let stuck = Fake::start(move |_, _| reply(200, uploads_xml(&[], Some((&k, "id"))))).await;
    let error = store(&stuck).list_unfinished().await.unwrap_err();
    assert!(matches!(error, StoreError::Server { .. }), "{error}");
    // A truncated page with no marker cannot be continued.
    let no_marker = Fake::start(|_, _| {
        reply(
            200,
            uploads_xml(&[], Some(("", ""))).replace("<NextKeyMarker></NextKeyMarker>", ""),
        )
    })
    .await;
    assert!(store(&no_marker).list_unfinished().await.is_err());
}

// ---------------------------------------------------------------------------------------------
// Complete

/// A fake that holds `parts` for the upload and the object `content` once completed.
fn object_fake(
    parts: Vec<(u16, u64)>,
    content: Vec<u8>,
) -> impl Fn(&Seen, usize) -> Step + Send + Sync + 'static {
    move |seen, _| match seen.op() {
        "list-parts" => reply(200, parts_xml(&parts, None)),
        "complete" => reply(200, complete_xml()),
        "get-object" => reply(200, content.clone()),
        "list-uploads" => reply(200, empty_uploads()),
        _ => reply(204, ""),
    }
}

#[tokio::test]
async fn complete_checks_lists_completes_and_reads_back() {
    let content = b"0123456789abc".to_vec();
    let fake = Fake::start(object_fake(vec![(1, 10), (2, 3)], content.clone())).await;
    store(&fake)
        .complete(&upload(), digest(&content), 13)
        .await
        .unwrap();
    assert_eq!(fake.ops(), ["list-parts", "complete", "get-object"]);
    // The completion names the parts in order with the tags the listing gave, numbered by position.
    let body = String::from_utf8(fake.seen()[1].body.clone()).unwrap();
    let first = body.find("etag-1").expect("tag 1");
    let second = body.find("etag-2").expect("tag 2");
    assert!(first < second, "{body}");
    assert_eq!(
        fake.seen()[1].header("content-length"),
        Some(body.len().to_string().as_str())
    );
}

#[tokio::test]
async fn complete_refuses_what_the_store_checks_itself_and_never_sends_the_completion() {
    // (name, parts the object server lists, length asked for, expected)
    type Check = fn(&StoreError) -> bool;
    type Case = (&'static str, Vec<(u16, u64)>, u64, Check);
    let cases: Vec<Case> = vec![
        ("no parts", vec![], 0, |e| matches!(e, StoreError::NoParts)),
        ("starts at 2", vec![(2, 10)], 10, |e| {
            matches!(e, StoreError::MissingPart { part: 1 })
        }),
        ("gap", vec![(1, 10), (3, 10)], 20, |e| {
            matches!(e, StoreError::MissingPart { part: 2 })
        }),
        ("small and not last", vec![(1, 9), (2, 10)], 19, |e| {
            matches!(e, StoreError::PartTooSmall { part: 1 })
        }),
        ("sum is not the length", vec![(1, 10), (2, 10)], 21, |e| {
            matches!(e, StoreError::PartsLengthMismatch { stored: 20 })
        }),
        (
            "over the object limit",
            vec![(1, 600), (2, 600)],
            1200,
            |e| matches!(e, StoreError::ObjectTooLarge),
        ),
    ];
    for (name, parts, length, expected) in cases {
        let fake = Fake::start(object_fake(parts, Vec::new())).await;
        let error = store(&fake)
            .complete(&upload(), [0; 32], length)
            .await
            .unwrap_err();
        assert!(expected(&error), "{name}: {error}");
        assert_eq!(fake.ops(), ["list-parts"], "{name}");
    }
}

#[tokio::test]
async fn the_last_part_may_be_small_and_one_empty_part_is_an_empty_object() {
    let fake = Fake::start(object_fake(vec![(1, 10), (2, 1)], vec![7; 11])).await;
    store(&fake)
        .complete(&upload(), digest(&[7; 11]), 11)
        .await
        .unwrap();
    let fake = Fake::start(object_fake(vec![(1, 0)], Vec::new())).await;
    store(&fake)
        .complete(&upload(), digest(b""), 0)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_hash_mismatch_deletes_the_object_and_is_answered_once_it_is_gone() {
    let content = b"0123456789abc".to_vec();
    let fake = Fake::start(object_fake(vec![(1, 10), (2, 3)], content)).await;
    let up = upload();
    let error = store(&fake)
        .complete(&up, digest(b"something else"), 13)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HashMismatch), "{error}");
    assert_eq!(
        fake.ops(),
        [
            "list-parts",
            "complete",
            "get-object",
            "list-uploads",
            "delete-object"
        ]
    );
}

#[tokio::test]
async fn a_failed_delete_after_a_mismatch_is_answered_instead_of_the_mismatch() {
    let content = b"0123456789abc".to_vec();
    let inner = object_fake(vec![(1, 10), (2, 3)], content);
    let fake = Fake::start(move |seen, index| match seen.op() {
        "delete-object" => reply(500, ""),
        _ => inner(seen, index),
    })
    .await;
    let error = store(&fake)
        .complete(&upload(), digest(b"something else"), 13)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 500, .. }),
        "the object is not gone, so it is not a mismatch: {error}"
    );
}

#[tokio::test]
async fn an_object_of_another_length_is_a_length_mismatch_and_is_deleted() {
    // The listing adds up to 13, the object server reads back 12.
    let fake = Fake::start(object_fake(vec![(1, 10), (2, 3)], b"0123456789ab".to_vec())).await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 13)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::LengthMismatch { stored: 12 }),
        "{error}"
    );
    assert!(fake.ops().contains(&"delete-object"));
    // A body longer than the length is cut at the length and mismatches too.
    let fake = Fake::start(object_fake(vec![(1, 3)], b"abcdef".to_vec())).await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 3)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::LengthMismatch { .. }),
        "{error}"
    );
    assert!(fake.ops().contains(&"delete-object"));
}

#[tokio::test]
async fn a_retry_after_a_completed_upload_reads_the_object_back_again() {
    let content = b"hello".to_vec();
    let object = content.clone();
    let fake = Fake::start(move |seen, _| match seen.op() {
        "list-parts" => reply(404, error_body("NoSuchUpload")),
        "head-object" => reply(200, ""),
        "get-object" => reply(200, object.clone()),
        "list-uploads" => reply(200, empty_uploads()),
        _ => reply(204, ""),
    })
    .await;
    store(&fake)
        .complete(&upload(), digest(&content), 5)
        .await
        .unwrap();
    assert_eq!(fake.ops(), ["list-parts", "head-object", "get-object"]);
    // The same retry with another digest finds a different object: it is deleted.
    let error = store(&fake)
        .complete(&upload(), digest(b"other"), 5)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::HashMismatch), "{error}");
}

#[tokio::test]
async fn a_completion_of_an_upload_that_never_was_is_not_found() {
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-parts" => reply(404, error_body("NoSuchUpload")),
        _ => reply(404, ""),
    })
    .await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 5)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::NotFound), "{error}");
}

#[tokio::test]
async fn a_completion_that_loses_the_upload_to_a_race_reads_the_object_if_it_is_there() {
    let content = b"hi".to_vec();
    let object = content.clone();
    let fake = Fake::start(move |seen, _| match seen.op() {
        "list-parts" => reply(200, parts_xml(&[(1, 2)], None)),
        "complete" => reply(404, error_body("NoSuchUpload")),
        "head-object" => reply(200, ""),
        _ => reply(200, object.clone()),
    })
    .await;
    store(&fake)
        .complete(&upload(), digest(&content), 2)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_object_servers_own_complaints_about_the_parts_are_parts_changed() {
    for (status, body) in [
        (400u16, error_body("InvalidPart")),
        (400, error_body("InvalidPartOrder")),
        (200, error_body("InvalidPart")),
    ] {
        let fake = Fake::start(move |seen, _| match seen.op() {
            "list-parts" => reply(200, parts_xml(&[(1, 2)], None)),
            _ => reply(status, body.clone()),
        })
        .await;
        let error = store(&fake)
            .complete(&upload(), [0; 32], 2)
            .await
            .unwrap_err();
        assert!(
            matches!(error, StoreError::PartsChanged),
            "{status}: {error}"
        );
    }
    // An error the store does not know is a status, and a 200 with no result is one too.
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-parts" => reply(200, parts_xml(&[(1, 2)], None)),
        _ => reply(200, error_body("InternalError")),
    })
    .await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 2)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 200, .. }),
        "{error}"
    );
}

#[tokio::test]
async fn a_read_back_that_is_cut_is_an_io_error_and_deletes_nothing() {
    let inner = object_fake(vec![(1, 10)], Vec::new());
    let fake = Fake::start(move |seen, index| match seen.op() {
        "get-object" => Step::Stream {
            status: 200,
            length: 10,
            send: b"abc".to_vec(),
            hold: false,
        },
        _ => inner(seen, index),
    })
    .await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 10)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Io { .. }), "{error}");
    assert!(!fake.ops().contains(&"delete-object"));
    assert!(!fake.ops().contains(&"list-uploads"));
}

#[tokio::test]
async fn a_read_back_that_stalls_is_cut_by_the_idle_timeout() {
    let inner = object_fake(vec![(1, 10)], Vec::new());
    let fake = Fake::start(move |seen, index| match seen.op() {
        "get-object" => Step::Stream {
            status: 200,
            length: 10,
            send: b"abc".to_vec(),
            hold: true,
        },
        _ => inner(seen, index),
    })
    .await;
    let began = std::time::Instant::now();
    let error = store(&fake)
        .complete(&upload(), [0; 32], 10)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Io {
                kind: ErrorKind::TimedOut,
                ..
            }
        ),
        "{error}"
    );
    assert!(began.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn a_completion_over_ten_thousand_parts_is_never_built() {
    // The listing guard stops at 10000 parts, before rusty-s3's body would panic past u16::MAX.
    let many: Vec<(u16, u64)> = (1..=10_000u16).map(|n| (n, 10)).collect();
    let fake = Fake::start(move |_, _| reply(200, parts_xml(&many, Some(10_000)))).await;
    let mut tuning = fast();
    tuning.max_xml_bytes = 4 * 1024 * 1024;
    let error = store_with(fake.port, tuning)
        .complete(&upload(), [0; 32], 1)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::PartsChanged), "{error}");
}

// ---------------------------------------------------------------------------------------------
// The key lock

#[tokio::test]
async fn two_completions_of_one_key_run_one_after_the_other() {
    let gate = Arc::new(Semaphore::new(0));
    let g = Arc::clone(&gate);
    let content = b"hello".to_vec();
    let inner = object_fake(vec![(1, 5)], content.clone());
    let fake = Arc::new(
        Fake::start(move |seen, index| match seen.op() {
            "list-parts" => Step::After(Arc::clone(&g), Box::new(inner(seen, index))),
            _ => inner(seen, index),
        })
        .await,
    );
    let store = Arc::new(store(&fake));
    let up = Arc::new(upload());
    let run = |store: Arc<S3Store>, up: Arc<Upload>, sha: [u8; 32]| {
        tokio::spawn(async move { store.complete(&up, sha, 5).await })
    };
    let first = run(Arc::clone(&store), Arc::clone(&up), digest(&content));
    let second = run(Arc::clone(&store), Arc::clone(&up), digest(&content));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let listings = fake.ops().iter().filter(|op| **op == "list-parts").count();
    assert_eq!(listings, 1, "the second completion must wait for the first");
    gate.add_permits(2);
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(
        fake.ops().iter().filter(|op| **op == "list-parts").count(),
        2
    );
}

#[tokio::test]
async fn a_delete_waits_for_a_completion_of_the_same_key_but_not_of_another() {
    let gate = Arc::new(Semaphore::new(0));
    let g = Arc::clone(&gate);
    let content = b"hello".to_vec();
    let inner = object_fake(vec![(1, 5)], content.clone());
    let fake = Arc::new(
        Fake::start(move |seen, index| match seen.op() {
            "list-parts" => Step::After(Arc::clone(&g), Box::new(inner(seen, index))),
            _ => inner(seen, index),
        })
        .await,
    );
    let store = Arc::new(store(&fake));
    let up = Arc::new(upload());
    let completing = {
        let (store, up) = (Arc::clone(&store), Arc::clone(&up));
        let sha = digest(&content);
        tokio::spawn(async move { store.complete(&up, sha, 5).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let same = {
        let (store, up) = (Arc::clone(&store), Arc::clone(&up));
        tokio::spawn(async move { store.delete(&up.key).await })
    };
    // Another key is not held up.
    store.delete(&ObjectKey::generate().unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let deletes_of_same = fake
        .seen()
        .iter()
        .filter(|s| s.op() == "delete-object" && s.path().ends_with(up.key.as_str()))
        .count();
    assert_eq!(deletes_of_same, 0, "the delete of the held key must wait");
    gate.add_permits(1);
    completing.await.unwrap().unwrap();
    same.await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------------------------
// Get

#[tokio::test]
async fn get_streams_the_object_and_names_its_length() {
    let fake = Fake::start(|_, _| reply(200, b"some bytes".to_vec())).await;
    let key = ObjectKey::generate().unwrap();
    let body = store(&fake).get(&key).await.unwrap();
    assert_eq!(body.length, 10);
    assert_eq!(read_all(body).await.unwrap(), b"some bytes");
    assert_eq!(fake.ops(), ["get-object"]);
}

#[tokio::test]
async fn get_maps_the_status() {
    let key = ObjectKey::generate().unwrap();
    for (status, expected) in [
        (404u16, "not found"),
        (403, "refused"),
        (500, "server"),
        (206, "server"),
    ] {
        let fake = Fake::start(move |_, _| reply(status, error_body("NoSuchKey"))).await;
        let error = store(&fake).get(&key).await.unwrap_err();
        let kind = match error {
            StoreError::NotFound => "not found",
            StoreError::Refused { .. } => "refused",
            StoreError::Server { .. } => "server",
            ref other => panic!("{other}"),
        };
        assert_eq!(kind, expected, "{status}");
    }
}

#[tokio::test]
async fn a_get_body_that_stalls_fails_with_a_timeout_and_one_that_is_cut_fails_too() {
    let key = ObjectKey::generate().unwrap();
    let stalled = Fake::start(|_, _| Step::Stream {
        status: 200,
        length: 10,
        send: b"abc".to_vec(),
        hold: true,
    })
    .await;
    let body = store(&stalled).get(&key).await.unwrap();
    let error = read_all(body).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let cut = Fake::start(|_, _| Step::Stream {
        status: 200,
        length: 10,
        send: b"abc".to_vec(),
        hold: false,
    })
    .await;
    let body = store(&cut).get(&key).await.unwrap();
    assert!(read_all(body).await.is_err());
}

#[tokio::test]
async fn a_slow_get_body_that_keeps_progressing_has_no_total_bound() {
    // Three chunks 600 ms apart with a 1 s idle timeout: 1.2 s in all, which passes.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await;
        stream
            .write_all(b"HTTP/1.1 200 X\r\ncontent-length: 6\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        for chunk in [b"ab", b"cd", b"ef"] {
            stream.write_all(chunk).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
    });
    let mut tuning = fast();
    tuning.read_idle_timeout = Duration::from_secs(1);
    let store = store_with(port, tuning);
    let body = store.get(&ObjectKey::generate().unwrap()).await.unwrap();
    assert_eq!(read_all(body).await.unwrap(), b"abcdef");
    task.abort();
}

#[tokio::test]
async fn a_get_answer_with_no_length_is_refused() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await;
        // No length: the body ends with the connection.
        stream
            .write_all(b"HTTP/1.1 200 X\r\nconnection: close\r\n\r\nabc")
            .await
            .unwrap();
    });
    let error = store_with(port, fast())
        .get(&ObjectKey::generate().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 200, .. }),
        "{error}"
    );
    task.abort();
}

// ---------------------------------------------------------------------------------------------
// Delete

#[tokio::test]
async fn delete_lists_aborts_the_exact_key_then_lists_again_then_deletes_the_object() {
    let key = ObjectKey::generate().unwrap();
    let other = ObjectKey::generate().unwrap();
    let (kt, ot) = (key.to_string(), other.to_string());
    let fake = Fake::start({
        let (kt, ot) = (kt.clone(), ot.clone());
        move |seen, index| match seen.op() {
            // The first listing holds two uploads of the key and one of another key, the second
            // nothing.
            "list-uploads" if index == 0 => reply(
                200,
                uploads_xml(&[(&kt, "id-a"), (&kt, "id-b"), (&ot, "id-c")], None),
            ),
            "list-uploads" => reply(200, empty_uploads()),
            _ => reply(204, ""),
        }
    })
    .await;
    store(&fake).delete(&key).await.unwrap();
    assert_eq!(
        fake.ops(),
        [
            "list-uploads",
            "abort",
            "abort",
            "list-uploads",
            "delete-object"
        ]
    );
    let seen = fake.seen();
    assert_eq!(seen[0].param("prefix"), Some(kt.as_str()));
    let aborted: Vec<&str> = seen
        .iter()
        .filter(|s| s.op() == "abort")
        .filter_map(|s| s.param("uploadId"))
        .collect();
    assert_eq!(aborted, ["id-a", "id-b"]);
    assert!(
        seen.iter()
            .filter(|s| s.op() == "abort")
            .all(|s| s.path().ends_with(&kt))
    );
    assert!(seen[4].path().ends_with(&kt));
}

#[tokio::test]
async fn delete_gives_up_listing_after_three_rounds_and_still_deletes_the_object() {
    let key = ObjectKey::generate().unwrap();
    let kt = key.to_string();
    let fake = Fake::start({
        let kt = kt.clone();
        move |seen, _| match seen.op() {
            "list-uploads" => reply(200, uploads_xml(&[(&kt, "id-a")], None)),
            _ => reply(204, ""),
        }
    })
    .await;
    store(&fake).delete(&key).await.unwrap();
    assert_eq!(
        fake.ops(),
        [
            "list-uploads",
            "abort",
            "list-uploads",
            "abort",
            "list-uploads",
            "abort",
            "delete-object"
        ]
    );
}

#[tokio::test]
async fn deleting_nothing_succeeds_and_a_gone_upload_or_object_is_no_error() {
    let key = ObjectKey::generate().unwrap();
    let kt = key.to_string();
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-uploads" => reply(200, empty_uploads()),
        _ => reply(204, ""),
    })
    .await;
    store(&fake).delete(&key).await.unwrap();
    assert_eq!(fake.ops(), ["list-uploads", "delete-object"]);
    // The upload is aborted by someone else between the listing and the abort; the object is 404.
    let fake = Fake::start({
        let kt = kt.clone();
        move |seen, index| match seen.op() {
            "list-uploads" if index == 0 => reply(200, uploads_xml(&[(&kt, "id-a")], None)),
            "list-uploads" => reply(200, empty_uploads()),
            "abort" => reply(404, error_body("NoSuchUpload")),
            _ => reply(404, error_body("NoSuchKey")),
        }
    })
    .await;
    store(&fake).delete(&key).await.unwrap();
}

#[tokio::test]
async fn a_failed_abort_keeps_the_object_and_is_answered() {
    let key = ObjectKey::generate().unwrap();
    let kt = key.to_string();
    let fake = Fake::start(move |seen, _| match seen.op() {
        "list-uploads" => reply(200, uploads_xml(&[(&kt, "id-a")], None)),
        _ => reply(500, ""),
    })
    .await;
    let error = store(&fake).delete(&key).await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 500, .. }),
        "{error}"
    );
    assert!(!fake.ops().contains(&"delete-object"));
}

#[tokio::test]
async fn a_404_is_gone_only_with_the_code_that_says_so() {
    let key = ObjectKey::generate().unwrap();
    let kt = key.to_string();
    // get
    let fake = Fake::start(|_, _| reply(404, error_body("NoSuchBucket"))).await;
    let error = store(&fake).get(&key).await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 404, .. }),
        "{error}"
    );
    // delete of the object
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-uploads" => reply(200, empty_uploads()),
        _ => reply(404, error_body("NoSuchBucket")),
    })
    .await;
    let error = store(&fake).delete(&key).await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 404, .. }),
        "{error}"
    );
    // abort
    let fake = Fake::start(move |seen, _| match seen.op() {
        "list-uploads" => reply(200, uploads_xml(&[(&kt, "id-a")], None)),
        _ => reply(404, error_body("NoSuchBucket")),
    })
    .await;
    let error = store(&fake).delete(&key).await.unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 404, .. }),
        "{error}"
    );
    assert!(!fake.ops().contains(&"delete-object"));
    // complete
    let fake = Fake::start(|seen, _| match seen.op() {
        "list-parts" => reply(200, parts_xml(&[(1, 2)], None)),
        _ => reply(404, error_body("NoSuchBucket")),
    })
    .await;
    let error = store(&fake)
        .complete(&upload(), [0; 32], 2)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Server { status: 404, .. }),
        "{error}"
    );
}

#[tokio::test]
async fn an_upload_of_the_key_is_aborted_whatever_its_start_time_reads_as() {
    let key = ObjectKey::generate().unwrap();
    let kt = key.to_string();
    let listing =
        uploads_xml(&[(&kt, "id-a")], None).replace("2026-11-18T10:00:00.000Z", "not a time");
    let fake = Fake::start({
        let listing = listing.clone();
        move |seen, index| match seen.op() {
            "list-uploads" if index == 0 => reply(200, listing.clone()),
            "list-uploads" => reply(200, empty_uploads()),
            _ => reply(204, ""),
        }
    })
    .await;
    store(&fake).delete(&key).await.unwrap();
    assert_eq!(
        fake.ops(),
        ["list-uploads", "abort", "list-uploads", "delete-object"]
    );
    // The unfinished listing counts it as skipped: its time is unknown.
    let fake = Fake::start(move |_, _| reply(200, listing.clone())).await;
    let listed = store(&fake).list_unfinished().await.unwrap();
    assert_eq!((listed.uploads.len(), listed.skipped), (0, 1));
}

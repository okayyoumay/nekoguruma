//! The fetcher against a scripted server over the in-memory dial: rows A1 to A19 and A21 of the
//! download leg's case table (ADR-497 items 6 to 8). The route's own rows (A5 to A7 as the route
//! judges them) are the tests of `route`, the cache's (A2, A15, A17, A19 as the directory sees
//! them) those of `store`, and the client's streaming those of `uploader::http`.
//!
//! The server answers the exchange, the download route and the object port. Tests with a body in
//! flight and a stall run on the real clock with a short no-progress limit: the writer's blocking
//! thread holds off a paused clock's auto-advance (as the uploader's notes say).

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use shared_crypto::{PublicKey, SigningKey, instruction as envelope};
use shared_proto::{AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, VciId, Vin};
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::time::Instant;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::control::dial::Dialled;
use crate::control::exchange::{CHALLENGE_PATH, SESSION_PATH};
use crate::instruction;
use crate::uploader::fake::{FakeDial, Seen, answer, json, read_request};

const API: u16 = 8080;
const OBJ: u16 = 9000;
const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const JOB: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

/// Serializes the operations of the fetchers of all tests, which the log test also holds for its
/// whole body. A call site that is first reached on a thread without a subscriber while the log
/// test's scoped subscriber is being set up can stay disabled, and the "no secret in the log"
/// checks would then pass on an empty log. A test thread that holds the gate does not take it
/// again.
pub(super) fn gate() -> Gate {
    static GATE: Mutex<()> = Mutex::new(());
    thread_local!(static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if HELD.get() {
        return Gate(None);
    }
    let guard = GATE.lock().unwrap_or_else(|e| e.into_inner());
    HELD.set(true);
    Gate(Some((guard, || HELD.set(false))))
}

pub(super) struct Gate(Option<(std::sync::MutexGuard<'static, ()>, fn())>);

impl Drop for Gate {
    fn drop(&mut self) {
        if let Some((_, release)) = &self.0 {
            release();
        }
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Bytes that are not text and cross several chunks.
fn bytes(n: usize, seed: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 31 + seed) % 251) as u8).collect()
}

fn token_text(n: u32) -> String {
    STANDARD.encode([n as u8; 32])
}

/// A 200 answer with a body of any bytes.
fn ok(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn download_url() -> String {
    format!(
        r#"{{"url":"http://127.0.0.1:{OBJ}/bucket/key?X-Amz-Signature=sig","expiresInSecs":60}}"#
    )
}

/// What the object port does with a GET.
#[derive(Clone)]
enum Obj {
    /// Writes these bytes (a whole answer) and closes.
    Raw(Vec<u8>),
    /// Writes these bytes and then neither writes nor closes.
    Hold(Vec<u8>),
    /// Writes the head and then the body in pieces with a gap between them.
    Slow {
        body: Vec<u8>,
        pieces: usize,
        gap: Duration,
    },
}

#[derive(Default)]
struct Script {
    challenge: VecDeque<Vec<u8>>,
    session: VecDeque<Vec<u8>>,
    downloads: VecDeque<Vec<u8>>,
    objects: VecDeque<Obj>,
    object_default: Option<Obj>,
    /// Every request: the port, the request line and the `Authorization` header.
    log: Vec<(u16, String, Option<String>)>,
    sessions: u32,
}

impl Script {
    fn count(&self, port: u16, line: &str) -> usize {
        self.log
            .iter()
            .filter(|(p, l, _)| *p == port && l.contains(line))
            .count()
    }

    fn react(&mut self, port: u16, seen: &Seen) -> Obj {
        let line = seen.request_line().to_owned();
        self.log
            .push((port, line.clone(), seen.header("authorization")));
        let raw = |bytes: Vec<u8>| Obj::Raw(bytes);
        if port == OBJ {
            return self
                .objects
                .pop_front()
                .or_else(|| self.object_default.clone())
                .unwrap_or_else(|| raw(answer(404, "", "")));
        }
        if line.contains(CHALLENGE_PATH) {
            return raw(self.challenge.pop_front().unwrap_or_else(|| {
                json(
                    200,
                    &format!(r#"{{"challenge":"{}"}}"#, STANDARD.encode([7u8; 32])),
                )
            }));
        }
        if line.contains(SESSION_PATH) {
            return raw(self.session.pop_front().unwrap_or_else(|| {
                self.sessions += 1;
                json(
                    200,
                    &format!(
                        r#"{{"token":"{}","expiresInSecs":300}}"#,
                        token_text(self.sessions)
                    ),
                )
            }));
        }
        raw(self
            .downloads
            .pop_front()
            .unwrap_or_else(|| json(200, &download_url())))
    }
}

fn serve(script: Arc<Mutex<Script>>) -> impl Fn(u16, DuplexStream) + Send + Sync + 'static {
    move |port, mut server| {
        let script = script.clone();
        tokio::spawn(async move {
            let Some(seen) = read_request(&mut server).await else {
                return;
            };
            let reply = script.lock().unwrap().react(port, &seen);
            match reply {
                Obj::Raw(bytes) => {
                    let _ = server.write_all(&bytes).await;
                    let _ = server.shutdown().await;
                }
                Obj::Hold(bytes) => {
                    let _ = server.write_all(&bytes).await;
                    std::future::pending::<()>().await;
                }
                Obj::Slow { body, pieces, gap } => {
                    let head = ok(&body);
                    let head_len = head.len() - body.len();
                    let _ = server.write_all(&head[..head_len]).await;
                    let step = body.len().div_ceil(pieces);
                    for piece in body.chunks(step) {
                        tokio::time::sleep(gap).await;
                        let _ = server.write_all(piece).await;
                    }
                    let _ = server.shutdown().await;
                }
            }
        });
    }
}

/// The in-memory dial, with the object port able to refuse.
struct Dials {
    inner: Arc<FakeDial>,
    object_down: AtomicBool,
}

impl PortDial for Dials {
    fn dial_port(&self, port: u16) -> Dialled<'_> {
        if port == OBJ && self.object_down.load(Ordering::Relaxed) {
            self.inner.dialled.lock().unwrap().push(port);
            return Box::pin(async { Err(io::ErrorKind::ConnectionRefused.into()) });
        }
        self.inner.dial_port(port)
    }
}

struct Rig {
    temp: tempfile::TempDir,
    script: Arc<Mutex<Script>>,
    dials: Arc<Dials>,
    fetcher: Fetcher,
    verified: VerifiedInstruction,
    /// The documents, in the order of the instruction's (ascending) hashes.
    docs: Vec<Vec<u8>>,
}

impl Rig {
    fn new(docs: Vec<Vec<u8>>, artifact: Option<Vec<u8>>) -> Rig {
        Rig::build(docs, artifact, None)
    }

    /// A rig whose bounds are `bound` bytes when given.
    fn build(docs: Vec<Vec<u8>>, artifact: Option<Vec<u8>>, bound: Option<u64>) -> Rig {
        let mut docs = docs;
        docs.sort_by_key(|d| digest(d));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("objects");
        fs::create_dir(&root).unwrap();
        let script = Arc::new(Mutex::new(Script::default()));
        let dials = Arc::new(Dials {
            inner: FakeDial::new(64 * 1024, serve(script.clone())),
            object_down: AtomicBool::new(false),
        });
        let agent_key = Arc::new(SigningKey::generate().unwrap());
        let record = Record {
            agent_id: AGENT_ID.to_owned(),
            owner_id: "owner".to_owned(),
            preset: "personal".to_owned(),
            allow_remote: false,
            server_origin: format!("http://127.0.0.1:{API}"),
            instruction_key_pem: String::new(),
            instruction_key_id: String::new(),
            agent_key_fingerprint: agent_key.public_key().fingerprint_hex(),
        };
        let seams = Seams {
            client: Client::with_dial(dials.clone()),
            dial: dials.clone(),
            signer: agent_key.clone(),
            random: shared_crypto::random_32,
        };
        let mut fetcher = Fetcher::with_seams(
            &record,
            &agent_key.public_key(),
            ObjectStore::open_in(&root).unwrap(),
            seams,
        )
        .unwrap();
        if let Some(bound) = bound {
            fetcher = fetcher.with_limits(bound, bound);
        }
        let (verified, _) = sign(
            docs.iter().map(|d| digest(d)).collect(),
            artifact.as_deref().map(digest),
        );
        Rig {
            temp,
            script,
            dials,
            fetcher,
            verified,
            docs,
        }
    }

    fn one(doc: &[u8]) -> Rig {
        Rig::new(vec![doc.to_vec()], None)
    }

    fn tmp_dir(&self) -> std::path::PathBuf {
        self.temp.path().join("objects").join("tmp")
    }

    fn tmp_files(&self) -> usize {
        fs::read_dir(self.tmp_dir()).unwrap().count()
    }

    /// Waits (at most 10 seconds, on the real clock) until `tmp/` is empty: a dropped download's
    /// file is removed by the guard, which on Windows retries while the writer thread still holds
    /// it.
    fn tmp_empty_soon(&self) -> bool {
        for _ in 0..500 {
            if self.tmp_files() == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn cached(&self, kind: Kind, bytes: &[u8]) -> std::path::PathBuf {
        self.temp
            .path()
            .join("objects")
            .join(kind.dir())
            .join(hex(&digest(bytes)))
    }

    fn serve(&self, object: Obj) {
        self.script.lock().unwrap().object_default = Some(object);
    }

    fn serve_body(&self, body: &[u8]) {
        self.serve(Obj::Raw(ok(body)));
    }

    fn count(&self, port: u16, line: &str) -> usize {
        self.script.lock().unwrap().count(port, line)
    }

    fn gets(&self) -> usize {
        self.count(OBJ, "GET ")
    }

    fn downloads(&self) -> usize {
        self.count(API, "/downloads")
    }

    fn sessions(&self) -> usize {
        self.count(API, SESSION_PATH)
    }

    fn dialled(&self, port: u16) -> usize {
        self.dials
            .inner
            .dialled
            .lock()
            .unwrap()
            .iter()
            .filter(|p| **p == port)
            .count()
    }

    async fn doc(&self, at: usize) -> Result<Fetched, FetchError> {
        let d = digest(&self.docs[at]);
        self.fetcher.fetch(&self.verified, Kind::Document, &d).await
    }

    async fn doc_retrying(&self, at: usize, attempts: u32) -> Result<Fetched, FetchError> {
        let d = digest(&self.docs[at]);
        self.fetcher
            .fetch_retrying(&self.verified, Kind::Document, &d, attempts)
            .await
    }
}

fn sign(documents: Vec<[u8; 32]>, artifact: Option<[u8; 32]>) -> (VerifiedInstruction, PublicKey) {
    let key = SigningKey::generate().unwrap();
    let body = JobInstruction {
        version: INSTRUCTION_VERSION,
        job_id: JobId(JOB.to_owned()),
        ownership_generation: 3,
        agent_id: AgentId(AGENT_ID.to_owned()),
        operator_id: "operator".to_owned(),
        kind: JobKind::RunSequence,
        vci: VciId("vci".to_owned()),
        vin: Some(Vin("VIN".to_owned())),
        document_hashes: documents,
        artifact_hash: artifact,
        plan_hash: None,
        issued_at: 1,
        start_deadline: 2,
    };
    let blob = envelope::seal(&key, &body.encode().unwrap()).unwrap();
    let verified = instruction::open(&blob, &key.public_key(), AGENT_ID).unwrap();
    (verified, key.public_key())
}

fn retry_why(error: FetchError) -> &'static str {
    match error {
        FetchError::Retry { why, .. } => why,
        other => panic!("not a retry: {other:?}"),
    }
}

// ---------------------------------------------------------------- A1, A15

/// A15 and the whole path: the exchange, the route with the bearer, the GET with none, and the
/// object cached byte-identical.
#[tokio::test(start_paused = true)]
async fn a_download_is_cached_byte_identical_with_the_bearer_on_the_route_only() {
    let rig = Rig::one(&bytes(200_000, 1));
    rig.serve_body(&rig.docs[0]);
    let got = rig.doc(0).await.unwrap();
    assert_eq!((got.kind, got.length), (Kind::Document, 200_000));
    assert_eq!(got.path, rig.cached(Kind::Document, &rig.docs[0]));
    assert_eq!(fs::read(&got.path).unwrap(), rig.docs[0]);
    assert_eq!(rig.tmp_files(), 0);
    let script = rig.script.lock().unwrap();
    let lines: Vec<&str> = script.log.iter().map(|(_, l, _)| l.as_str()).collect();
    assert!(lines[0].contains(CHALLENGE_PATH) && lines[1].contains(SESSION_PATH));
    assert!(lines[2].starts_with("POST /api/v1/agent/downloads "));
    assert_eq!(lines[3], "GET /bucket/key?X-Amz-Signature=sig HTTP/1.1");
    assert_eq!(
        script.log[2].2.as_deref(),
        Some(&*format!("Bearer {}", token_text(1)))
    );
    assert_eq!(script.log[3].2, None, "the URL is the bearer of the GET");
    assert_eq!(script.log.len(), 4);
}

/// A1: a cached object that hashes to its name is used with no request and no exchange.
#[tokio::test(start_paused = true)]
async fn a_cached_object_needs_no_request() {
    let rig = Rig::one(b"document one");
    rig.serve_body(&rig.docs[0]);
    rig.doc(0).await.unwrap();
    let before = rig.script.lock().unwrap().log.len();
    let dials = rig.dialled(API) + rig.dialled(OBJ);
    let again = rig.doc(0).await.unwrap();
    assert_eq!(again.length, 12);
    assert_eq!(rig.script.lock().unwrap().log.len(), before);
    assert_eq!(rig.dialled(API) + rig.dialled(OBJ), dials);
    // A cached object serves a restarted fetcher too: the cache is on disk.
    let rig2 = Rig::one(b"document one");
    assert_eq!(rig2.gets(), 0);
}

/// Only what the instruction names is fetched, under the kind it names.
#[tokio::test(start_paused = true)]
async fn only_a_named_digest_of_the_right_kind_is_fetched() {
    let rig = Rig::new(vec![b"doc".to_vec()], Some(b"art".to_vec()));
    rig.serve_body(b"doc");
    let f = &rig.fetcher;
    let v = &rig.verified;
    assert_eq!(
        f.fetch(v, Kind::Document, &digest(b"other")).await,
        Err(FetchError::NotNamed)
    );
    assert_eq!(
        f.fetch(v, Kind::Artifact, &digest(b"doc")).await,
        Err(FetchError::NotNamed)
    );
    assert_eq!(
        f.fetch(v, Kind::Document, &digest(b"art")).await,
        Err(FetchError::NotNamed)
    );
    let (none, _) = sign(vec![digest(b"doc")], None);
    assert_eq!(
        f.fetch(&none, Kind::Artifact, &digest(b"art")).await,
        Err(FetchError::NotNamed)
    );
    assert!(
        rig.script.lock().unwrap().log.is_empty(),
        "nothing was sent"
    );
}

/// Every document, then the artifact, on one token; the token is gone at the end.
#[tokio::test(start_paused = true)]
async fn fetch_all_takes_the_documents_then_the_artifact_on_one_token() {
    let rig = Rig::new(
        vec![b"doc a".to_vec(), b"doc b".to_vec()],
        Some(b"the artifact".to_vec()),
    );
    {
        let mut script = rig.script.lock().unwrap();
        for body in [&rig.docs[0], &rig.docs[1], &b"the artifact".to_vec()] {
            script.objects.push_back(Obj::Raw(ok(body)));
        }
    }
    let all = rig.fetcher.fetch_all(&rig.verified, 1).await.unwrap();
    let kinds: Vec<_> = all.iter().map(|f| f.kind).collect();
    assert_eq!(kinds, [Kind::Document, Kind::Document, Kind::Artifact]);
    assert_eq!(fs::read(&all[2].path).unwrap(), b"the artifact");
    assert_eq!((rig.sessions(), rig.downloads(), rig.gets()), (1, 3, 3));
    // No token is kept: with the cache emptied the next fetch exchanges again.
    fs::remove_file(rig.cached(Kind::Document, b"doc a")).unwrap();
    rig.script
        .lock()
        .unwrap()
        .objects
        .push_back(Obj::Raw(ok(b"doc a")));
    let a = digest(b"doc a");
    rig.fetcher
        .fetch(&rig.verified, Kind::Document, &a)
        .await
        .unwrap();
    assert_eq!(rig.sessions(), 2);
}

/// `read_document` returns the bytes only when they hash to the instruction's hash, and fetches
/// again a file that went bad.
#[tokio::test(start_paused = true)]
async fn read_document_returns_only_matching_bytes() {
    let rig = Rig::one(b"document bytes");
    rig.serve_body(b"document bytes");
    let d = digest(b"document bytes");
    let got = rig
        .fetcher
        .read_document(&rig.verified, &d, 1)
        .await
        .unwrap();
    assert_eq!(got, b"document bytes");
    fs::write(rig.cached(Kind::Document, b"document bytes"), b"tampered!").unwrap();
    let got = rig
        .fetcher
        .read_document(&rig.verified, &d, 1)
        .await
        .unwrap();
    assert_eq!(got, b"document bytes");
    assert_eq!(rig.gets(), 2);
}

// ---------------------------------------------------------------- A2

/// A2: a cached file that no longer matches is deleted and fetched again; a name that is not a
/// regular file is a local failure and nothing is deleted or sent.
#[tokio::test(start_paused = true)]
async fn a_bad_cached_file_is_deleted_and_fetched_again() {
    let rig = Rig::one(b"document one");
    rig.serve_body(b"document one");
    let path = rig.doc(0).await.unwrap().path;
    fs::write(&path, b"document ONE").unwrap();
    let again = rig.doc(0).await.unwrap();
    assert_eq!(fs::read(&again.path).unwrap(), b"document one");
    assert_eq!(rig.gets(), 2);

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let before = rig.script.lock().unwrap().log.len();
    assert_eq!(
        rig.doc(0).await,
        Err(FetchError::Local { why: "not-regular" })
    );
    assert!(path.is_dir());
    assert_eq!(rig.script.lock().unwrap().log.len(), before);
}

// ---------------------------------------------------------------- A3, A4

/// A3: a failed exchange is a retry with the exchange's wait; nothing else is sent.
#[tokio::test(start_paused = true)]
async fn a_failed_exchange_is_a_retry_with_its_wait() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.challenge.push_back(json(500, "{}"));
        script
            .challenge
            .push_back(answer(429, "Retry-After: 9\r\n", "{}"));
        script.session.push_back(json(403, "{}"));
    }
    assert_eq!(
        rig.doc(0).await,
        Err(FetchError::Retry {
            why: "exchange",
            status: None,
            retry_after_secs: None
        })
    );
    assert_eq!(
        rig.doc(0).await,
        Err(FetchError::Retry {
            why: "exchange",
            status: None,
            retry_after_secs: Some(9)
        })
    );
    assert_eq!(rig.doc(0).await, Err(FetchError::ExchangeRefused));
    assert_eq!((rig.downloads(), rig.gets()), (0, 0));
    // The next one gets through.
    rig.doc(0).await.unwrap();
}

/// A3: the bounded loop waits the back-off between attempts: 1 second, then 2.
#[tokio::test(start_paused = true)]
async fn the_bounded_loop_waits_the_back_off() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.challenge.push_back(json(500, "{}"));
        script.challenge.push_back(json(500, "{}"));
    }
    let started = Instant::now();
    rig.doc_retrying(0, 3).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert_eq!(rig.sessions(), 1);

    // The bound: the last error is returned after the last attempt, with no wait after it.
    let rig = Rig::one(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        for _ in 0..5 {
            script.challenge.push_back(json(500, "{}"));
        }
    }
    let started = Instant::now();
    let error = rig.doc_retrying(0, 3).await.unwrap_err();
    assert_eq!(retry_why(error), "exchange");
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert_eq!(rig.count(API, CHALLENGE_PATH), 3);
}

/// A4: a 401 drops the token, exchanges again and repeats once; a second 401 is a retry.
#[tokio::test(start_paused = true)]
async fn a_401_gets_one_new_exchange_and_one_repeat() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    rig.script
        .lock()
        .unwrap()
        .downloads
        .push_back(json(401, "{}"));
    rig.doc(0).await.unwrap();
    assert_eq!((rig.sessions(), rig.downloads()), (2, 2));
    {
        let script = rig.script.lock().unwrap();
        let bearers: Vec<_> = script
            .log
            .iter()
            .filter(|(_, l, _)| l.contains("/downloads"))
            .map(|(_, _, a)| a.clone().unwrap())
            .collect();
        assert_eq!(
            bearers,
            [
                format!("Bearer {}", token_text(1)),
                format!("Bearer {}", token_text(2))
            ]
        );
    }

    let rig = Rig::one(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.downloads.push_back(json(401, "{}"));
        script.downloads.push_back(json(401, "{}"));
    }
    assert_eq!(
        rig.doc(0).await,
        Err(FetchError::Retry {
            why: "unauthorized",
            status: Some(401),
            retry_after_secs: None
        })
    );
    assert_eq!((rig.sessions(), rig.downloads(), rig.gets()), (2, 2, 0));
}

/// A revoked agent: the route answers 401 and the exchange is then refused.
#[tokio::test(start_paused = true)]
async fn a_401_then_a_refused_exchange_ends_in_a_retry_of_the_exchange() {
    let rig = Rig::one(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.downloads.push_back(json(401, "{}"));
        script.session.push_back(json(
            200,
            &format!(r#"{{"token":"{}","expiresInSecs":300}}"#, token_text(1)),
        ));
        script.session.push_back(json(403, "{}"));
    }
    assert_eq!(rig.doc(0).await, Err(FetchError::ExchangeRefused));
    assert_eq!(rig.gets(), 0);
}

// ---------------------------------------------------------------- A5, A6, A7

/// A5: a refusal ends the fetch, and the loop does not repeat it.
#[tokio::test(start_paused = true)]
async fn a_refusal_ends_the_fetch_and_is_not_retried() {
    for status in [400u16, 404, 413, 500, 403] {
        let rig = Rig::one(b"d");
        rig.script
            .lock()
            .unwrap()
            .downloads
            .push_back(json(status, "{}"));
        assert_eq!(
            rig.doc_retrying(0, 5).await,
            Err(FetchError::Refused {
                why: "refused",
                status: Some(status)
            }),
            "{status}"
        );
        assert_eq!((rig.downloads(), rig.gets()), (1, 0));
    }
    let rig = Rig::one(b"d");
    rig.script.lock().unwrap().downloads.push_back(json(
        200,
        r#"{"url":"http://127.0.0.1:9000/x","expiresInSecs":60,"more":1}"#,
    ));
    assert!(matches!(
        rig.doc(0).await,
        Err(FetchError::Refused { why: "answer", .. })
    ));
}

/// A6: an unavailable route is retried with the back-off and a `Retry-After`.
#[tokio::test(start_paused = true)]
async fn an_unavailable_route_is_retried_after_the_back_off() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.downloads.push_back(json(503, "{}"));
        script
            .downloads
            .push_back(answer(429, "Retry-After: 10\r\n", "{}"));
    }
    let started = Instant::now();
    rig.doc_retrying(0, 3).await.unwrap();
    // 1 second, then the longer of 2 and the 10 the server named.
    assert_eq!(started.elapsed(), Duration::from_secs(11));
    assert_eq!(rig.downloads(), 3);
}

/// A7: a URL that breaks the host rule is a retry and nothing is sent to it.
#[tokio::test(start_paused = true)]
async fn a_url_off_the_host_rule_is_a_retry_and_is_never_dialled() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    rig.script.lock().unwrap().downloads.push_back(json(
        200,
        r#"{"url":"http://127.0.0.2:9000/x","expiresInSecs":60}"#,
    ));
    assert_eq!(
        retry_why(rig.doc(0).await.unwrap_err()),
        "download-url-host"
    );
    assert_eq!(rig.dialled(OBJ), 0);
    assert_eq!(rig.tmp_files(), 0);
}

// ---------------------------------------------------------------- A8 to A13

/// A8: a refused connect to the object port is a retry.
#[tokio::test(start_paused = true)]
async fn a_refused_object_connect_is_a_retry() {
    let rig = Rig::one(b"d");
    rig.dials.object_down.store(true, Ordering::Relaxed);
    assert_eq!(retry_why(rig.doc(0).await.unwrap_err()), "not-sent");
    assert_eq!(rig.tmp_files(), 0);
    rig.dials.object_down.store(false, Ordering::Relaxed);
    rig.serve_body(b"d");
    rig.doc(0).await.unwrap();
}

/// A9 and A10: a redirect is not followed, and no status but 200 has its body read (the server
/// declares a body it never sends, so a read would stall); each is a retry with a new URL.
#[tokio::test(start_paused = true)]
async fn a_status_but_200_is_a_retry_and_its_body_is_never_read() {
    for (status, why) in [
        (301u16, "get-redirect"),
        (302, "get-redirect"),
        (307, "get-redirect"),
        (403, "get-status"),
        (404, "get-status"),
        (500, "get-status"),
        (503, "get-status"),
        (206, "get-status"),
    ] {
        let rig = Rig::one(b"d");
        rig.serve(Obj::Hold(
            format!("HTTP/1.1 {status} X\r\nLocation: http://127.0.0.1:9999/x\r\nContent-Length: 10\r\n\r\n")
                .into_bytes(),
        ));
        let started = Instant::now();
        let error = rig.doc(0).await.unwrap_err();
        assert_eq!(
            error,
            FetchError::Retry {
                why,
                status: Some(status),
                retry_after_secs: None
            },
            "{status}"
        );
        assert_eq!(started.elapsed(), Duration::ZERO, "no body was waited for");
        assert_eq!(rig.dialled(9999), 0, "the redirect is not followed");
        assert_eq!(rig.tmp_files(), 0);
    }
    // A new URL is asked for by the next attempt.
    let rig = Rig::one(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.objects.push_back(Obj::Raw(answer(403, "", "")));
        script.objects.push_back(Obj::Raw(ok(b"d")));
    }
    rig.doc_retrying(0, 2).await.unwrap();
    assert_eq!(rig.downloads(), 2);
}

/// A11: a 200 without exactly one canonical length, or over the kind's bound, is refused with no
/// body byte read.
#[tokio::test(start_paused = true)]
async fn a_200_without_a_usable_length_is_refused_before_any_body_byte() {
    let heads = [
        "HTTP/1.1 200 X\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 X\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
        "HTTP/1.1 200 X\r\nContent-Length: 01\r\n\r\n",
        "HTTP/1.1 200 X\r\nContent-Length: 9\r\n\r\n",
        "HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\n\r\n",
        "HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n",
    ];
    for head in heads {
        let rig = Rig::build(vec![b"d".to_vec()], None, Some(8));
        rig.serve(Obj::Hold(format!("{head}d").into_bytes()));
        let started = Instant::now();
        let error = rig.doc(0).await.unwrap_err();
        assert!(
            matches!(
                error,
                FetchError::Refused {
                    why: "get-length",
                    ..
                }
            ),
            "{head:?}: {error:?}"
        );
        assert_eq!(started.elapsed(), Duration::ZERO, "{head:?}");
        assert_eq!(rig.tmp_files(), 0);
        assert_eq!(rig.doc_retrying(0, 4).await.unwrap_err().class(), "refused");
    }
}

/// A11: a length hyper itself does not parse is no answer (a retry) and no body byte is read.
#[tokio::test(start_paused = true)]
async fn a_malformed_length_is_no_answer_and_no_body_is_read() {
    let rig = Rig::one(b"d");
    rig.serve(Obj::Hold(
        b"HTTP/1.1 200 X\r\nContent-Length: +1\r\n\r\nd".to_vec(),
    ));
    let started = Instant::now();
    assert_eq!(retry_why(rig.doc(0).await.unwrap_err()), "transport");
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(rig.tmp_files(), 0);
}

/// A12: a body cut short is a retry and leaves nothing; one longer than declared is never taken
/// beyond the declared length, so the digest does not match.
#[tokio::test(start_paused = true)]
async fn a_cut_body_leaves_nothing_and_a_longer_one_is_not_accepted() {
    let rig = Rig::one(&bytes(100, 5));
    let mut cut = ok(&rig.docs[0]);
    cut.truncate(cut.len() - 40);
    rig.serve(Obj::Raw(cut));
    assert_eq!(retry_why(rig.doc(0).await.unwrap_err()), "transport");
    assert_eq!(rig.tmp_files(), 0);
    assert!(!rig.cached(Kind::Document, &rig.docs[0]).exists());

    // Declares 3 and sends 100: only 3 bytes are ever taken.
    let mut long = b"HTTP/1.1 200 X\r\nContent-Length: 3\r\nConnection: close\r\n\r\n".to_vec();
    long.extend_from_slice(&rig.docs[0]);
    rig.serve(Obj::Raw(long));
    assert_eq!(rig.doc(0).await, Err(FetchError::Mismatch));
    assert_eq!(rig.tmp_files(), 0);
    assert!(!rig.cached(Kind::Document, &rig.docs[0]).exists());
}

/// A13: a body with no byte for the no-progress limit is dropped and its file deleted; a slow
/// body that keeps moving is not (no total time).
#[tokio::test]
async fn a_stalled_body_is_dropped_and_a_slow_one_is_not() {
    let mut rig = Rig::one(&bytes(1000, 2));
    rig.fetcher.client = rig
        .fetcher
        .client
        .clone()
        .with_no_progress(Duration::from_secs(1));
    let mut partial = ok(&rig.docs[0]);
    partial.truncate(partial.len() - 500);
    rig.serve(Obj::Hold(partial));
    assert_eq!(retry_why(rig.doc(0).await.unwrap_err()), "no-progress");
    assert!(rig.tmp_empty_soon());

    // Five pieces, 400 ms apart: two seconds in all, over the one-second limit, but never silent
    // for more than 400 ms, a margin a loaded runner does not close.
    rig.serve(Obj::Slow {
        body: rig.docs[0].clone(),
        pieces: 5,
        gap: Duration::from_millis(400),
    });
    let started = std::time::Instant::now();
    rig.doc(0).await.unwrap();
    assert!(started.elapsed() > Duration::from_secs(1));
    assert_eq!(
        fs::read(rig.cached(Kind::Document, &rig.docs[0])).unwrap(),
        rig.docs[0]
    );
}

// ---------------------------------------------------------------- A14

/// A14: a full download whose digest differs is deleted, one more attempt is allowed, and a
/// second mismatch in a row fails the object until a restart, with no more requests.
#[tokio::test(start_paused = true)]
async fn a_second_mismatch_in_a_row_fails_the_object_until_restart() {
    let rig = Rig::one(b"the real bytes");
    rig.serve_body(b"not the bytes!");
    assert_eq!(rig.doc(0).await, Err(FetchError::Mismatch));
    assert_eq!(rig.tmp_files(), 0);
    assert!(!rig.cached(Kind::Document, &rig.docs[0]).exists());
    assert_eq!(rig.doc(0).await, Err(FetchError::MismatchTwice));
    let gets = rig.gets();
    assert_eq!(gets, 2);
    assert_eq!(rig.doc(0).await, Err(FetchError::MismatchTwice));
    assert_eq!(rig.doc_retrying(0, 5).await, Err(FetchError::MismatchTwice));
    assert_eq!(rig.gets(), gets, "no request after the second mismatch");
    // A restart (a new fetcher) starts over.
    let again = Rig::one(b"the real bytes");
    again.serve_body(b"the real bytes");
    again.doc(0).await.unwrap();
}

/// A14: the loop stops after two mismatches, and a good download clears the count.
#[tokio::test(start_paused = true)]
async fn the_loop_makes_two_attempts_on_a_mismatch_and_a_match_clears_the_count() {
    let rig = Rig::one(b"the real bytes");
    rig.serve_body(b"not the bytes!");
    assert_eq!(rig.doc_retrying(0, 5).await, Err(FetchError::MismatchTwice));
    assert_eq!(rig.gets(), 2);

    let rig = Rig::one(b"the real bytes");
    {
        let mut script = rig.script.lock().unwrap();
        script.objects.push_back(Obj::Raw(ok(b"not the bytes!")));
        script.objects.push_back(Obj::Raw(ok(b"the real bytes")));
    }
    rig.doc_retrying(0, 3).await.unwrap();
    // Cache emptied, one more mismatch: the first of a new run, not a second.
    fs::remove_file(rig.cached(Kind::Document, b"the real bytes")).unwrap();
    rig.serve_body(b"not the bytes!");
    assert_eq!(rig.doc(0).await, Err(FetchError::Mismatch));
}

// ---------------------------------------------------------------- A16

/// A16: a temporary file that cannot be made is a local failure, which the loop does not retry.
#[tokio::test(start_paused = true)]
async fn a_local_failure_is_not_retried() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    fs::remove_dir_all(rig.tmp_dir()).unwrap();
    fs::write(rig.tmp_dir(), b"in the way").unwrap();
    assert_eq!(
        rig.doc_retrying(0, 5).await,
        Err(FetchError::Local { why: "io" })
    );
    assert_eq!((rig.downloads(), rig.gets()), (1, 0));
    assert!(!rig.cached(Kind::Document, b"d").exists());
}

// ---------------------------------------------------------------- A18, A19

/// A18: a stop drops the future; the temporary file is deleted and nothing is reported. The stop
/// comes once the GET has reached the server, so the download is under way.
#[tokio::test]
async fn a_stop_deletes_the_temporary_file() {
    let rig = Rig::one(&bytes(1000, 3));
    let mut partial = ok(&rig.docs[0]);
    partial.truncate(partial.len() - 500);
    rig.serve(Obj::Hold(partial));
    let stopped = async {
        for _ in 0..500 {
            if rig.gets() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    tokio::select! {
        biased;
        () = stopped => {}
        got = rig.doc(0) => panic!("the fetch ended by itself: {got:?}"),
    }
    assert!(rig.tmp_empty_soon());
    assert!(!rig.cached(Kind::Document, &rig.docs[0]).exists());
}

/// A19: one fetch at a time, so a second request for the digest finds the cache.
#[tokio::test(start_paused = true)]
async fn two_requests_for_one_digest_make_one_download() {
    let rig = Rig::one(&bytes(50_000, 4));
    rig.serve_body(&rig.docs[0]);
    let (a, b) = tokio::join!(rig.doc(0), rig.doc(0));
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(rig.gets(), 1);
}

/// A write that fails (a full disk) is a local failure, which the loop does not retry, and
/// leaves nothing.
#[tokio::test(start_paused = true)]
async fn a_failing_write_is_a_local_failure() {
    let rig = Rig::one(&bytes(100_000, 6));
    rig.serve_body(&rig.docs[0]);
    rig.fetcher.store.fail_writes_after(10_000);
    assert_eq!(
        rig.doc_retrying(0, 5).await,
        Err(FetchError::Local { why: "write" })
    );
    assert_eq!(rig.gets(), 1);
    assert!(rig.tmp_empty_soon());
    assert!(!rig.cached(Kind::Document, &rig.docs[0]).exists());
}

/// A refused exchange takes the back-off to its cap, as the uploader's does, and it stays there:
/// a refusal and then an ordinary failure wait 60 seconds and 60 seconds again.
#[tokio::test(start_paused = true)]
async fn a_refused_exchange_caps_the_back_off_and_it_stays_there() {
    let rig = Rig::one(b"d");
    rig.serve_body(b"d");
    {
        let mut script = rig.script.lock().unwrap();
        script.challenge.push_back(json(
            200,
            &format!(r#"{{"challenge":"{}"}}"#, STANDARD.encode([7u8; 32])),
        ));
        script.session.push_back(json(403, "{}"));
        script.challenge.push_back(json(500, "{}"));
    }
    let started = Instant::now();
    rig.doc_retrying(0, 3).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(120));
}

/// A `Retry-After` of the object server is taken for a 429 and a 503 only, as for the routes.
#[tokio::test(start_paused = true)]
async fn the_get_s_retry_after_is_kept_for_429_and_503_only() {
    for (status, want) in [
        (429u16, Some(7)),
        (503, Some(7)),
        (500, None),
        (404, None),
        (302, None),
    ] {
        let rig = Rig::one(b"d");
        rig.serve(Obj::Raw(answer(status, "Retry-After: 7\r\n", "")));
        let error = rig.doc(0).await.unwrap_err();
        assert_eq!(
            error,
            FetchError::Retry {
                why: if status == 302 {
                    "get-redirect"
                } else {
                    "get-status"
                },
                status: Some(status),
                retry_after_secs: want
            },
            "{status}"
        );
    }
}

/// `read_document`: the bytes of a fresh download, of a cached file with no request, and a file
/// that went bad is fetched again; the same call holds the lock, so it is not interleaved with
/// another fetch of the digest.
#[tokio::test(start_paused = true)]
async fn read_document_returns_the_bytes_of_one_verified_load() {
    let rig = Rig::one(&bytes(70_000, 9));
    rig.serve_body(&rig.docs[0]);
    let d = digest(&rig.docs[0]);
    let read = || rig.fetcher.read_document(&rig.verified, &d, 1);
    assert_eq!(read().await.unwrap(), rig.docs[0]);
    let requests = rig.script.lock().unwrap().log.len();
    assert_eq!(read().await.unwrap(), rig.docs[0]);
    assert_eq!(
        rig.script.lock().unwrap().log.len(),
        requests,
        "a cache hit"
    );
    fs::write(rig.cached(Kind::Document, &rig.docs[0]), b"bad").unwrap();
    assert_eq!(read().await.unwrap(), rig.docs[0]);
    assert_eq!(rig.gets(), 2);
    let (a, b) = tokio::join!(read(), rig.doc(0));
    assert_eq!(a.unwrap(), rig.docs[0]);
    b.unwrap();
    assert_eq!(rig.gets(), 2);
    // A digest the instruction does not name is not read.
    let other = digest(b"other");
    assert_eq!(
        rig.fetcher.read_document(&rig.verified, &other, 1).await,
        Err(FetchError::NotNamed)
    );
}

// ---------------------------------------------------------------- A21

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Log {
    type Writer = Log;

    fn make_writer(&'a self) -> Log {
        self.clone()
    }
}

/// A21: whatever happens, the log has kinds, classes and statuses and no URL, token, digest,
/// signature or body.
#[tokio::test(start_paused = true)]
async fn the_log_holds_no_url_token_digest_or_body() {
    // No other test's fetcher runs while this subscriber is set up and used.
    let _gate = gate();
    let log = Log::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    // Another test's thread may have cached a call site as disabled before this subscriber
    // existed.
    tracing::callsite::rebuild_interest_cache();
    let secret = b"SECRET-DOCUMENT-BODY";
    let rig = Rig::one(secret);
    {
        let mut script = rig.script.lock().unwrap();
        script.challenge.push_back(json(500, "{}"));
        script
            .downloads
            .push_back(json(503, r#"{"title":"SECRET-PROBLEM-BODY"}"#));
        script
            .downloads
            .push_back(json(404, r#"{"title":"SECRET-PROBLEM-BODY"}"#));
    }
    let _ = rig.doc_retrying(0, 5).await;
    // A mismatch, a cached file gone bad and a refusal of the URL.
    rig.serve_body(b"SECRET-OTHER-BYTES-!!");
    let _ = rig.doc_retrying(0, 5).await;
    let fine = Rig::one(secret);
    fine.serve_body(secret);
    let path = fine.doc(0).await.unwrap().path;
    fs::write(&path, b"changed").unwrap();
    fine.script.lock().unwrap().downloads.push_back(json(
        200,
        r#"{"url":"http://127.0.0.2:9000/SECRET-URL-PATH","expiresInSecs":60}"#,
    ));
    let _ = fine.doc(0).await;
    let text = log.0.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&text).into_owned();
    assert!(text.contains("document"), "{text}");
    assert!(text.contains("retry") || text.contains("refused"), "{text}");
    let hash = hex(&digest(secret));
    for needle in [
        "SECRET",
        "X-Amz",
        "127.0.0.",
        "bucket",
        hash.as_str(),
        &token_text(1),
        &token_text(2),
        &STANDARD.encode([7u8; 32]),
    ] {
        assert!(!text.contains(needle), "{needle} is in the log:\n{text}");
    }
}

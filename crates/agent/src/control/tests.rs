//! The control channel session library's cases (ADR-471's case table; the row's letter and number
//! begin each test's name). The loop runs against an in-process fake server over
//! `tokio::io::duplex` on a paused clock, through the private dial seam. Two smoke tests run over
//! real loopback in real time and never pause.
//!
//! Keys, tokens, challenges and nonces are made at run time, and no assertion prints one: the
//! checks compare with `assert!` and a fixed message. Nothing here sleeps for a fixed real time;
//! a paused clock moves only when every task is waiting, so what a test waits for is an event
//! (a connection, a frame, a phase) within a bounded virtual time.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use shared_crypto::proof::{Audience, ProofTranscript, verify_proof};
use shared_crypto::registration::RegistrationCode;
use shared_crypto::{
    PublicKey, RandomError, SignError, SignErrorKind, Signature, Signer, SigningKey,
};
use shared_proto::{
    AgentId, AgentMessage, CLOSE_CODE_TOO_BIG, CONTROL_SUBPROTOCOL, INSTRUCTION_VERSION, JobId,
    JobInstruction, JobKind, MAX_CONTROL_MESSAGE_LEN, ServerMessage, VciId, Vin,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing_subscriber::fmt::MakeWriter;

use super::backoff::{self, Backoff};
use super::channel::{self, HEARTBEAT};
use super::dial::{Dial, Dialled, Stream};
use super::exchange::{CHALLENGE_PATH, SESSION_PATH};
use super::*;
use crate::identity::load_or_create;
use crate::identity::store::{MemoryStore, SecretName};
use crate::instruction::VerifiedInstruction;
use crate::registration::{Request as RegisterRequest, register_in};
use crate::server_http::Timeouts;

/// The longest a test waits for anything, in (paused) time: a test that waits this long has failed.
const DEADLINE: Duration = Duration::from_secs(3600);
/// How far a measured instant may lie past the scheduled one: the clock ticks in milliseconds and
/// each step of a chain may round up once.
const TOL: Duration = Duration::from_millis(50);
const ORIGIN: &str = "http://127.0.0.1:8080";
const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const JOB_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f";

// ---------------------------------------------------------------------------------------------
// The log

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
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

impl Log {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn lines(&self, level: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains(level))
            .map(str::to_owned)
            .collect()
    }

    /// The warnings whose line holds `word`.
    fn warns(&self, word: &str) -> usize {
        self.lines("WARN")
            .iter()
            .filter(|line| line.contains(word))
            .count()
    }

    fn contains(&self, word: &str) -> bool {
        self.text().contains(word)
    }
}

// ---------------------------------------------------------------------------------------------
// The dial

/// What a test can do to the client's end of every connection.
#[derive(Default)]
struct Flags {
    fail_read: AtomicBool,
    fail_write: AtomicBool,
    stall_write: AtomicBool,
}

/// The client's end of a duplex pipe, with faults a test switches on.
struct Flaky {
    inner: DuplexStream,
    flags: Arc<Flags>,
}

impl AsyncRead for Flaky {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.flags.fail_read.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Flaky {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.flags.fail_write.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if self.flags.stall_write.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.flags.fail_write.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if self.flags.stall_write.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// One dial the loop made: when, and the server's end (none when the dial was refused or hangs).
type Dialed = (Instant, Option<DuplexStream>);

struct FakeDial {
    conns: mpsc::UnboundedSender<Dialed>,
    refuse: AtomicBool,
    hang: AtomicBool,
    flags: Arc<Flags>,
}

impl Dial for FakeDial {
    fn dial(&self) -> Dialled<'_> {
        Box::pin(async move {
            let at = Instant::now();
            if self.hang.load(Ordering::SeqCst) {
                let _ = self.conns.send((at, None));
                std::future::pending::<()>().await;
            }
            if self.refuse.load(Ordering::SeqCst) {
                let _ = self.conns.send((at, None));
                return Err(io::ErrorKind::ConnectionRefused.into());
            }
            let (client, server) = tokio::io::duplex(1 << 20);
            let _ = self.conns.send((at, Some(server)));
            Ok(Box::new(Flaky {
                inner: client,
                flags: self.flags.clone(),
            }) as Stream)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The state directory, the record and the key

struct Fixture {
    dir: tempfile::TempDir,
    key: Arc<SigningKey>,
    /// The key whose public half the record names as the instruction key.
    instruction: Arc<SigningKey>,
}

fn record_for(public: &PublicKey, origin: &str) -> Record {
    record_with(
        public,
        origin,
        &SigningKey::generate().unwrap().public_key(),
    )
}

fn record_with(public: &PublicKey, origin: &str, instruction: &PublicKey) -> Record {
    Record {
        agent_id: AGENT_ID.to_owned(),
        owner_id: "owner-1".to_owned(),
        preset: "personal".to_owned(),
        allow_remote: false,
        server_origin: origin.to_owned(),
        instruction_key_pem: instruction.to_spki_pem(),
        instruction_key_id: instruction.fingerprint_hex(),
        agent_key_fingerprint: public.fingerprint_hex(),
    }
}

impl Fixture {
    fn with_origin(origin: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(DIR_NAME)).unwrap();
        std::fs::create_dir(dir.path().join(record::DIR_NAME)).unwrap();
        let key = Arc::new(SigningKey::generate().unwrap());
        let instruction = Arc::new(SigningKey::generate().unwrap());
        let fixture = Fixture {
            dir,
            key,
            instruction,
        };
        fixture.write(&record_with(
            &fixture.key.public_key(),
            origin,
            &fixture.instruction.public_key(),
        ));
        fixture
    }

    fn new() -> Fixture {
        Fixture::with_origin(ORIGIN)
    }

    fn control_dir(&self) -> PathBuf {
        self.dir.path().join(DIR_NAME)
    }

    fn registration_dir(&self) -> PathBuf {
        self.dir.path().join(record::DIR_NAME)
    }

    fn record_path(&self) -> PathBuf {
        self.registration_dir().join(record::RECORD_FILE)
    }

    fn connection_lock(&self) -> PathBuf {
        self.control_dir().join(LOCK_FILE)
    }

    fn registration_lock(&self) -> PathBuf {
        self.registration_dir().join(record::LOCK_FILE)
    }

    fn write(&self, record: &Record) {
        std::fs::write(
            self.record_path(),
            serde_json::to_vec_pretty(record).unwrap(),
        )
        .unwrap();
    }

    fn open(&self) -> Result<Opened, ControlError> {
        open_in(&self.control_dir(), &self.registration_dir())
    }

    /// Whether both lock files can be taken now (and are released again at once).
    fn locks_are_free(&self) -> bool {
        let connection = WriterLock::at(&self.connection_lock());
        let registration = WriterLock::at(&self.registration_lock());
        connection.is_ok() && registration.is_ok()
    }
}

/// An agent key of the kind the store gives, holding `signing`.
fn agent_key_for(signing: &SigningKey) -> AgentKey {
    let store = MemoryStore::new();
    let name = SecretName::agent_key(OperatingMode::User);
    store.insert_raw(&name, &signing.to_pkcs8_der());
    let lock_dir = tempfile::tempdir().unwrap();
    load_or_create(
        &store,
        &name,
        &|| Ok(lock_dir.path().to_owned()),
        Duration::from_millis(100),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------------
// The environment: the loop under test and the fake server's side

fn failing_random() -> Result<[u8; 32], RandomError> {
    Err(RandomError)
}

fn panicking_random() -> Result<[u8; 32], RandomError> {
    panic!("a scripted panic in the task");
}

/// A signer that cannot sign.
struct BrokenSigner(Arc<SigningKey>);

impl Signer for BrokenSigner {
    fn public_key(&self) -> PublicKey {
        self.0.public_key()
    }

    fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
        Err(SignError::new(SignErrorKind::Failed))
    }
}

struct Opts {
    random: fn() -> Result<[u8; 32], RandomError>,
    broken_signer: bool,
    origin: &'static str,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            random: shared_crypto::random_32,
            broken_signer: false,
            origin: ORIGIN,
        }
    }
}

/// An intake that keeps what it was given: the job ID and the envelope.
#[derive(Default)]
struct RecordingIntake(Mutex<Vec<(String, Vec<u8>)>>);

impl Intake for RecordingIntake {
    fn take(&self, instruction: VerifiedInstruction) {
        self.0.lock().unwrap().push((
            instruction.instruction().job_id.0.clone(),
            instruction.envelope().to_vec(),
        ));
    }
}

struct Env {
    fx: Fixture,
    intake: Arc<RecordingIntake>,
    handle: Handle,
    conns: mpsc::UnboundedReceiver<Dialed>,
    dial: Arc<FakeDial>,
    log: Log,
    started: Instant,
    // Dropped last: the log stays captured until the loop has ended.
    _subscriber: tracing::subscriber::DefaultGuard,
}

type Ws = WebSocketStream<DuplexStream>;

/// What the upgrade request carried.
#[derive(Default, Clone)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What one exchange gave: when it began, the token, and the secrets the log must not hold.
struct Exchanged {
    at: Instant,
    token: String,
    secrets: Vec<String>,
}

impl Env {
    fn new() -> Env {
        Env::with(Opts::default())
    }

    fn with(opts: Opts) -> Env {
        let log = Log::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let fx = Fixture::with_origin(opts.origin);
        let (tx, rx) = mpsc::unbounded_channel();
        let dial = Arc::new(FakeDial {
            conns: tx,
            refuse: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            flags: Arc::new(Flags::default()),
        });
        let signer: Arc<dyn Signer + Send + Sync> = if opts.broken_signer {
            Arc::new(BrokenSigner(fx.key.clone()))
        } else {
            fx.key.clone()
        };
        let intake = Arc::new(RecordingIntake::default());
        let seams = Seams {
            dial: dial.clone(),
            signer,
            random: opts.random,
            intake: intake.clone(),
        };
        let started = Instant::now();
        let handle = fx
            .open()
            .unwrap()
            .start_with(&fx.key.public_key(), seams)
            .unwrap();
        Env {
            fx,
            intake,
            handle,
            conns: rx,
            dial,
            log,
            started,
            _subscriber: guard,
        }
    }

    /// The commands answered `commandAck` and `commandRejected` so far.
    fn counts(&self) -> (u64, u64) {
        let status = self.status();
        (status.commands_acknowledged, status.commands_rejected)
    }

    fn status(&self) -> Status {
        self.handle.status().borrow().clone()
    }

    async fn wait_phase(&self, phase: Phase) {
        let mut status = self.handle.status();
        let seen = tokio::time::timeout(DEADLINE, status.wait_for(|s| s.phase == phase)).await;
        assert!(matches!(seen, Ok(Ok(_))), "the phase was not reached");
    }

    /// Waits until `count` commands are answered, whichever way.
    async fn wait_commands(&self, count: u64) {
        let mut status = self.handle.status();
        let seen = tokio::time::timeout(
            DEADLINE,
            status.wait_for(|s| s.commands_acknowledged + s.commands_rejected >= count),
        )
        .await;
        assert!(matches!(seen, Ok(Ok(_))), "the commands were not answered");
    }

    /// The next dial of the loop, whatever became of it.
    async fn dialed(&mut self) -> Dialed {
        tokio::time::timeout(DEADLINE, self.conns.recv())
            .await
            .expect("the agent did not dial in time")
            .expect("the dial seam is gone")
    }

    /// The next dial, which must have reached the fake server.
    async fn conn(&mut self) -> (Instant, DuplexStream) {
        let (at, stream) = self.dialed().await;
        (at, stream.expect("the dial was refused"))
    }

    /// The next dial and the request on it.
    async fn request(&mut self) -> (Instant, DuplexStream, Req) {
        let (at, mut stream) = self.conn().await;
        let req = read_req(&mut stream).await;
        (at, stream, req)
    }

    /// An attempt's first request: the challenge. Answers it with `status`.
    async fn fail_challenge(
        &mut self,
        status: u16,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Instant {
        let (at, mut stream, req) = self.request().await;
        assert!(
            req.is_post(CHALLENGE_PATH),
            "an attempt starts with a challenge"
        );
        reply(&mut stream, status, headers, body).await;
        at
    }

    /// An attempt's challenge, answered with a good one.
    async fn challenge(&mut self) -> (Instant, [u8; 32]) {
        let (at, mut stream, req) = self.request().await;
        assert!(
            req.is_post(CHALLENGE_PATH),
            "an attempt starts with a challenge"
        );
        assert!(req.body.is_empty(), "the challenge request has no body");
        let challenge = shared_crypto::random_32().unwrap();
        json_reply(&mut stream, 200, &challenge_body(&challenge)).await;
        (at, challenge)
    }

    /// The session request for `challenge`, checked: the agent ID, the challenge and a proof that
    /// verifies. Returns the stream to answer on and the body.
    async fn session(&mut self, challenge: [u8; 32]) -> (Instant, DuplexStream, serde_json::Value) {
        let (at, stream, req) = self.request().await;
        assert!(
            req.is_post(SESSION_PATH),
            "the session request follows the challenge"
        );
        let body = req.json();
        assert!(body["agentId"] == AGENT_ID, "the agent ID is the record's");
        assert!(
            body["challenge"] == STANDARD.encode(challenge),
            "the challenge is the one given"
        );
        let nonce: [u8; 32] = STANDARD
            .decode(body["nonce"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let proof =
            Signature::from_bytes(&STANDARD.decode(body["proof"].as_str().unwrap()).unwrap())
                .unwrap();
        let audience = Audience::parse_canonical(ORIGIN).unwrap();
        let transcript = ProofTranscript::new(&audience, AGENT_ID, challenge, nonce).unwrap();
        assert!(
            verify_proof(&self.fx.key.public_key(), &transcript, &proof).is_ok(),
            "the proof verifies"
        );
        assert_eq!(
            body.as_object().unwrap().len(),
            4,
            "the body has four properties"
        );
        (at, stream, body)
    }

    /// Challenge and session request, up to the point where the session route answers.
    async fn until_session(&mut self) -> (Instant, DuplexStream, serde_json::Value) {
        let (_, challenge) = self.challenge().await;
        self.session(challenge).await
    }

    /// A whole exchange that ends in a token.
    async fn exchange(&mut self) -> Exchanged {
        let (at, challenge) = self.challenge().await;
        let (_, mut stream, body) = self.session(challenge).await;
        let token = token_text();
        json_reply(&mut stream, 200, &token_body(&token)).await;
        let secrets = ["challenge", "nonce", "proof"]
            .iter()
            .map(|name| body[name].as_str().unwrap().to_owned())
            .chain([token.clone()])
            .collect();
        Exchanged { at, token, secrets }
    }

    /// The upgrade: the fake server accepts the WebSocket handshake and picks the subprotocol.
    async fn upgrade(&mut self) -> (Instant, Ws, Seen) {
        let (at, stream) = self.conn().await;
        let seen = Arc::new(Mutex::new(Seen::default()));
        let ws = accept_ws(stream, seen.clone()).await;
        let seen = seen.lock().unwrap().clone();
        (at, ws, seen)
    }

    /// An exchange and an upgrade: the 101 is given, `hello` is not.
    async fn upgraded(&mut self) -> (Instant, Ws) {
        let exchanged = self.exchange().await;
        let (_, ws, seen) = self.upgrade().await;
        let bearer = format!("Bearer {}", exchanged.token);
        assert!(
            seen.header("authorization") == Some(bearer.as_str()),
            "the bearer token"
        );
        (exchanged.at, ws)
    }

    /// A connection with `hello` read: the instant of `hello` and the server's socket.
    async fn connected(&mut self, heartbeat_secs: u32) -> (Instant, Ws) {
        let (_, mut ws) = self.upgraded().await;
        send(&mut ws, &hello(heartbeat_secs)).await;
        let at = Instant::now();
        self.wait_phase(Phase::Connected).await;
        (at, ws)
    }
}

// ---------------------------------------------------------------------------------------------
// The fake server: HTTP

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn is_post(&self, path: &str) -> bool {
        self.method == "POST" && self.path == path
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the body is JSON")
    }
}

async fn read_req<S: AsyncRead + Unpin>(stream: &mut S) -> Req {
    tokio::time::timeout(DEADLINE, async {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream
                .read_exact(&mut byte)
                .await
                .expect("the request head");
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).into_owned();
        let mut lines = text.split("\r\n");
        let first = lines.next().unwrap();
        let mut parts = first.split(' ');
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream
            .read_exact(&mut body)
            .await
            .expect("the request body");
        Req {
            method,
            path,
            headers,
            body,
        }
    })
    .await
    .expect("the agent sent no request in time")
}

async fn reply<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut head = format!("HTTP/1.1 {status} Status\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();
    let _ = stream.shutdown().await;
}

async fn json_reply<S: AsyncWrite + Unpin>(stream: &mut S, status: u16, body: &str) {
    reply(
        stream,
        status,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
    .await;
}

fn challenge_body(challenge: &[u8; 32]) -> String {
    format!(r#"{{"challenge":"{}"}}"#, STANDARD.encode(challenge))
}

/// A fresh 32-byte value in padded base64: a token, or a challenge's text.
fn token_text() -> String {
    STANDARD.encode(shared_crypto::random_32().unwrap())
}

fn token_body(token: &str) -> String {
    format!(r#"{{"token":"{token}","expiresInSecs":3600}}"#)
}

// ---------------------------------------------------------------------------------------------
// The fake server: WebSocket

async fn accept_ws<S>(stream: S, seen: Arc<Mutex<Seen>>) -> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    #[expect(
        clippy::result_large_err,
        reason = "the handshake callback's error type is the stack's"
    )]
    let callback =
        move |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
            *seen.lock().unwrap() = Seen {
                path: request.uri().path().to_owned(),
                headers: request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_ascii_lowercase(),
                            value.to_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect(),
            };
            response.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(CONTROL_SUBPROTOCOL),
            );
            Ok(response)
        };
    tokio::time::timeout(
        DEADLINE,
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, None),
    )
    .await
    .expect("the handshake did not finish in time")
    .expect("the handshake")
}

async fn send(ws: &mut Ws, text: &str) {
    ws.send(Message::Text(text.into())).await.unwrap();
}

fn hello(secs: u32) -> String {
    format!(r#"{{"type":"hello","heartbeatSecs":{secs}}}"#)
}

fn job_command(id: &str) -> String {
    format!(r#"{{"type":"jobCommand","commandId":"{id}","signedJob":"c2lnbmVk"}}"#)
}

/// How a signed command is spoiled.
#[derive(Clone, Copy)]
enum Tamper {
    None,
    /// Cut to a length no envelope has.
    Truncate,
    /// Sealed by a key the agent does not trust.
    ForeignKey,
    /// A payload byte changed after signing.
    FlipPayload,
    /// An unknown envelope format byte.
    Format,
    /// The body names another agent.
    OtherAgent,
}

/// A `jobCommand` text and the envelope inside it.
struct Signed {
    text: String,
    envelope: Vec<u8>,
}

/// A job command the record's instruction key signed for `agent`, spoiled as `tamper` says.
fn signed_command(fx: &Fixture, id: &str, agent: &str, tamper: Tamper) -> Signed {
    use shared_crypto::instruction as envelope;
    let body = JobInstruction {
        version: INSTRUCTION_VERSION,
        job_id: JobId(JOB_ID.to_owned()),
        ownership_generation: 7,
        agent_id: AgentId(match tamper {
            Tamper::OtherAgent => "other-agent".to_owned(),
            _ => agent.to_owned(),
        }),
        operator_id: "operator-1".to_owned(),
        kind: JobKind::RunSequence,
        vci: VciId("vci-a".to_owned()),
        vin: Some(Vin("VIN-UNDER-TEST".to_owned())),
        document_hashes: vec![],
        artifact_hash: None,
        plan_hash: None,
        issued_at: 1_000,
        start_deadline: 2_000,
    };
    let payload = body.encode().unwrap();
    let foreign;
    let key = match tamper {
        Tamper::ForeignKey => {
            foreign = SigningKey::generate().unwrap();
            &foreign
        }
        _ => &*fx.instruction,
    };
    let mut bytes = envelope::seal(key, &payload).unwrap();
    match tamper {
        Tamper::Truncate => bytes.truncate(5),
        Tamper::FlipPayload => *bytes.last_mut().unwrap() ^= 1,
        Tamper::Format => bytes[0] = bytes[0].wrapping_add(1),
        Tamper::None | Tamper::ForeignKey | Tamper::OtherAgent => {}
    }
    let text = format!(
        r#"{{"type":"jobCommand","commandId":"{id}","signedJob":"{}"}}"#,
        STANDARD.encode(&bytes)
    );
    Signed {
        text,
        envelope: bytes,
    }
}

/// The next message from the agent, which must be an answer to a job command.
async fn answer(ws: &mut Ws) -> (Instant, serde_json::Value) {
    let (at, message) = got(ws).await;
    let Got::Text(text) = message else {
        panic!("an answer is next, got {message:?}");
    };
    // It is a message of the agent's protocol, too.
    AgentMessage::decode(&text).unwrap();
    (at, serde_json::from_str(&text).unwrap())
}

/// What the agent sent, as the fake server reads it.
#[derive(Debug, PartialEq, Eq)]
enum Got {
    Text(String),
    Binary,
    Ping,
    Pong,
    Close(Option<u16>),
    Eof,
}

async fn got(ws: &mut Ws) -> (Instant, Got) {
    let next = tokio::time::timeout(DEADLINE, ws.next())
        .await
        .expect("the agent sent nothing in time");
    let got = match next {
        None | Some(Err(_)) => Got::Eof,
        Some(Ok(Message::Text(text))) => Got::Text(text.as_str().to_owned()),
        Some(Ok(Message::Binary(_))) => Got::Binary,
        Some(Ok(Message::Ping(_))) => Got::Ping,
        Some(Ok(Message::Pong(_))) => Got::Pong,
        Some(Ok(Message::Close(frame))) => Got::Close(frame.map(|frame| u16::from(frame.code))),
        Some(Ok(Message::Frame(_))) => unreachable!("the stack never returns a raw frame"),
    };
    (Instant::now(), got)
}

/// The next message must be the agent's heartbeat.
async fn heartbeat(ws: &mut Ws) -> Instant {
    let (at, message) = got(ws).await;
    assert_eq!(
        message,
        Got::Text(HEARTBEAT.to_owned()),
        "a heartbeat is next"
    );
    at
}

/// The next message must be a Close with `code`; `reply` answers it, as a server's stack does.
async fn close_of(ws: &mut Ws, code: u16, reply: bool) -> Instant {
    let (at, message) = got(ws).await;
    assert_eq!(message, Got::Close(Some(code)), "a close is next");
    if reply {
        let _ = ws.flush().await;
    }
    at
}

/// Reads until the agent's Close, skipping its heartbeats; returns its code and when it came.
async fn close_after_heartbeats(ws: &mut Ws, reply: bool) -> (Instant, Option<u16>) {
    loop {
        let (at, message) = got(ws).await;
        match message {
            Got::Text(text) if text == HEARTBEAT => {}
            Got::Close(code) => {
                if reply {
                    let _ = ws.flush().await;
                }
                return (at, code);
            }
            other => panic!("unexpected from the agent: {other:?}"),
        }
    }
}

fn close_frame(code: Option<u16>) -> Message {
    Message::Close(code.map(|code| CloseFrame {
        code: CloseCode::from(code),
        // The agent must never log this text.
        reason: "reason-text-that-must-not-be-logged".into(),
    }))
}

/// Writes bytes the stack would not write: they go to the pipe as they are.
async fn raw(ws: &mut Ws, bytes: &[u8]) {
    ws.flush().await.unwrap();
    ws.get_mut().write_all(bytes).await.unwrap();
    ws.get_mut().flush().await.unwrap();
}

/// An unmasked frame with the given first byte (FIN, reserved bits, opcode).
fn frame(first: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
    match payload.len() {
        len if len < 126 => out.push(len as u8),
        len if len <= 0xFFFF => {
            out.push(126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            out.push(127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// A server frame that breaks a rule of the protocol, and the code the agent closes with.
#[derive(Clone, Copy, Debug)]
enum Fault {
    TooBig,
    BadUtf8,
    Masked,
    ReservedBits,
    BadOpcode,
}

const FAULTS: [Fault; 5] = [
    Fault::TooBig,
    Fault::BadUtf8,
    Fault::Masked,
    Fault::ReservedBits,
    Fault::BadOpcode,
];

async fn inject(ws: &mut Ws, fault: Fault) -> u16 {
    match fault {
        Fault::TooBig => {
            let text = "x".repeat(MAX_CONTROL_MESSAGE_LEN + 1);
            ws.send(Message::Text(text.into())).await.unwrap();
            1009
        }
        Fault::BadUtf8 => {
            raw(ws, &frame(0x81, &[0xff, 0xfe, 0xfd])).await;
            1002
        }
        Fault::Masked => {
            let mut bytes = vec![0x81, 0x80 | 2, 1, 2, 3, 4];
            bytes.extend_from_slice(&[b'{' ^ 1, b'}' ^ 2]);
            raw(ws, &bytes).await;
            1002
        }
        Fault::ReservedBits => {
            raw(ws, &frame(0xC1, b"{}")).await;
            1002
        }
        Fault::BadOpcode => {
            raw(ws, &frame(0x83, b"x")).await;
            1002
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Time

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `at` is `want` seconds after `base`, give or take the clock's rounding.
fn assert_after(base: Instant, at: Instant, want: u64, what: &str) {
    let got = at.saturating_duration_since(base);
    assert!(
        got >= secs(want) && got < secs(want) + TOL,
        "{what}: wanted {want} s after the base, got {got:?}"
    );
}

/// The whole seconds between consecutive instants.
fn gaps(times: &[Instant]) -> Vec<u64> {
    times
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_secs_f64().round() as u64)
        .collect()
}

/// The delays the waits use after each failed attempt of an unbroken run of failures that take no
/// time: 1, 2, 4 and so on up to 60 seconds.
fn ord_delays(count: usize) -> Vec<u64> {
    (0..count).map(|k| (1u64 << k.min(6)).min(60)).collect()
}

/// Sleeps until `at`: a paused clock moves to it when everything else waits.
async fn until(at: Instant) {
    tokio::time::sleep_until(at).await;
}

// ---------------------------------------------------------------------------------------------
// A. Start

#[test]
fn a1_a_held_connection_lock_is_already_running_and_the_registration_lock_is_not_tried() {
    let fx = Fixture::new();
    let _connection = WriterLock::at(&fx.connection_lock()).unwrap();
    // Held too: were it tried, the answer would be RegistrationRunning.
    let _registration = WriterLock::at(&fx.registration_lock()).unwrap();
    assert!(matches!(fx.open(), Err(ControlError::AlreadyRunning)));
}

#[test]
fn a2_lock_io_failure_is_lock() {
    let fx = Fixture::new();
    std::fs::remove_dir(fx.control_dir()).unwrap();
    let error = fx.open().unwrap_err();
    assert!(matches!(error, ControlError::Lock(_)));
    assert!(
        !error.to_string().contains(fx.dir.path().to_str().unwrap()),
        "the text names no path"
    );
    // The registration lock's directory too.
    let fx = Fixture::new();
    std::fs::remove_dir_all(fx.registration_dir()).unwrap();
    assert!(matches!(fx.open(), Err(ControlError::Lock(_))));
    // The connection lock taken before it is released.
    assert!(WriterLock::at(&fx.connection_lock()).is_ok());
}

#[test]
fn a3_a_held_registration_lock_is_registration_running_and_the_connection_lock_is_released() {
    let fx = Fixture::new();
    let _registration = WriterLock::at(&fx.registration_lock()).unwrap();
    assert!(matches!(fx.open(), Err(ControlError::RegistrationRunning)));
    assert!(
        WriterLock::at(&fx.connection_lock()).is_ok(),
        "the connection lock was released"
    );
}

#[test]
fn a4_no_record_is_not_registered() {
    let fx = Fixture::new();
    std::fs::remove_file(fx.record_path()).unwrap();
    assert!(matches!(fx.open(), Err(ControlError::NotRegistered)));
    assert!(fx.locks_are_free());
}

#[test]
fn a5_a_record_that_cannot_be_used_is_record_and_names_nothing() {
    let fx = Fixture::new();
    let good = serde_json::to_value(record_for(&fx.key.public_key(), ORIGIN)).unwrap();
    let with = |change: &dyn Fn(&mut serde_json::Map<String, serde_json::Value>)| {
        let mut object = good.as_object().unwrap().clone();
        change(&mut object);
        serde_json::to_vec(&object).unwrap()
    };
    let path = fx.record_path();
    let reset = || {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&path);
    };
    let check = |name: &str| {
        let error = fx.open().unwrap_err();
        assert!(matches!(error, ControlError::Record(_)), "{name}");
        let text = format!("{error} {error:?}");
        assert!(
            !text.contains(fx.dir.path().to_str().unwrap()),
            "{name}: the error names the path"
        );
        assert!(fx.locks_are_free(), "{name}: the locks are released");
    };
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("not JSON", b"{ not json".to_vec()),
        ("over 16 KiB", vec![b' '; 16 * 1024 + 1]),
        (
            "an unknown field",
            with(&|o| {
                o.insert("extra".into(), true.into());
            }),
        ),
        (
            "a missing field",
            with(&|o| {
                o.remove("owner_id");
            }),
        ),
        (
            "a bad field",
            with(&|o| {
                o.insert("agent_id".into(), "not-a-uuid".into());
            }),
        ),
    ];
    for (name, bytes) in &files {
        reset();
        std::fs::write(&path, bytes).unwrap();
        check(name);
    }
    reset();
    std::fs::create_dir(&path).unwrap();
    check("a directory");
    #[cfg(unix)]
    {
        reset();
        let target = fx.dir.path().join("elsewhere.json");
        std::fs::write(&target, with(&|_| {})).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        check("a symbolic link");
    }
}

#[test]
fn a6_only_the_loopback_origin_in_canonical_form_is_accepted() {
    let fx = Fixture::new();
    let public = fx.key.public_key();
    for bad in [
        "http://localhost:8080",
        "https://127.0.0.1:8080",
        "http://127.0.0.1:80",
        "http://127.0.0.1:8080/",
        "http://127.0.0.1:8080/x",
        "http://127.0.0.2:8080",
        "http://[::1]:8080",
        "HTTP://127.0.0.1:8080",
        "http://127.0.0.1:08080",
        "http://127.0.0.1:99999",
        "http://user@127.0.0.1:8080",
        "http://127.0.0.1:",
        "",
    ] {
        fx.write(&record_for(&public, bad));
        assert!(
            matches!(
                fx.open(),
                Err(ControlError::Record(RecordError::Field("server_origin")))
            ),
            "{bad:?}"
        );
    }
    for good in ["http://127.0.0.1", "http://127.0.0.1:8080"] {
        fx.write(&record_for(&public, good));
        assert!(fx.open().is_ok(), "{good}");
    }
}

#[tokio::test(start_paused = true)]
async fn a7_a_key_with_another_fingerprint_is_a_key_mismatch() {
    let fx = Fixture::new();
    let other = SigningKey::generate().unwrap().public_key();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let seams = Seams {
        dial: Arc::new(FakeDial {
            conns: tx,
            refuse: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            flags: Arc::new(Flags::default()),
        }),
        signer: fx.key.clone(),
        random: shared_crypto::random_32,
        intake: Arc::new(LogIntake),
    };
    let error = fx.open().unwrap().start_with(&other, seams).unwrap_err();
    assert!(matches!(error, ControlError::KeyMismatch));
    assert!(fx.locks_are_free(), "both locks are released");
    assert!(rx.try_recv().is_err(), "nothing was dialled");
}

#[tokio::test(start_paused = true)]
async fn a9_when_everything_passes_the_first_exchange_starts_at_once_with_delay_1() {
    let mut env = Env::new();
    let (at, _, req) = env.request().await;
    assert_after(env.started, at, 0, "the first dial");
    assert!(req.is_post(CHALLENGE_PATH));
    let status = env.status();
    assert_eq!(status.next_delay, secs(1));
    assert_eq!(status.phase, Phase::Exchange);
    assert_eq!(
        (status.commands_acknowledged, status.commands_rejected),
        (0, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn a10_register_during_a_session_exits_3_and_names_the_session() {
    let env = Env::new();
    let key = agent_key_for(&SigningKey::generate().unwrap());
    for replace in [false, true] {
        let request = RegisterRequest {
            mode: OperatingMode::User,
            server: ServerOrigin::parse(ORIGIN).unwrap(),
            server_key: shared_crypto::random_32().unwrap(),
            replace,
        };
        let code = RegistrationCode::generate().unwrap();
        let error = register_in(
            &request,
            &key,
            &code,
            &env.fx.registration_dir(),
            Timeouts::default(),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 3);
        assert!(
            error.to_string().contains("control channel session"),
            "the message names a running session"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a11_a_second_open_in_the_same_process_is_already_running() {
    let fx = Fixture::new();
    let first = fx.open().unwrap();
    assert!(matches!(fx.open(), Err(ControlError::AlreadyRunning)));
    // And while the session runs.
    let (tx, _rx) = mpsc::unbounded_channel();
    let seams = Seams {
        dial: Arc::new(FakeDial {
            conns: tx,
            refuse: AtomicBool::new(true),
            hang: AtomicBool::new(false),
            flags: Arc::new(Flags::default()),
        }),
        signer: fx.key.clone(),
        random: shared_crypto::random_32,
        intake: Arc::new(LogIntake),
    };
    let handle = first.start_with(&fx.key.public_key(), seams).unwrap();
    assert!(matches!(fx.open(), Err(ControlError::AlreadyRunning)));
    handle.stop();
    handle.stopped().await;
    assert!(fx.open().is_ok(), "the lock is free after the session");
}

#[tokio::test(start_paused = true)]
async fn a12_stop_a_dropped_handle_and_a_panic_release_both_locks_and_delete_no_file() {
    // Stop: stopped() resolves after the release.
    let env = Env::new();
    env.handle.stop();
    env.handle.stopped().await;
    assert!(env.fx.locks_are_free(), "after stop");
    assert!(env.fx.connection_lock().exists() && env.fx.registration_lock().exists());
    assert_eq!(env.status().phase, Phase::Stopped);
    drop(env);

    // A dropped handle.
    let env = Env::new();
    let mut done = env.handle.done.clone();
    drop(env.handle);
    tokio::time::timeout(DEADLINE, done.wait_for(|done| *done))
        .await
        .unwrap()
        .unwrap();
    assert!(env.fx.locks_are_free(), "after a dropped handle");
    drop(env.fx);

    // A panic in the task.
    let mut env = Env::with(Opts {
        random: panicking_random,
        ..Opts::default()
    });
    let _ = env.challenge().await;
    env.handle.stopped().await;
    assert!(env.fx.locks_are_free(), "after a panic");
}

#[test]
fn a12_runtime_shutdown_and_an_unstarted_opened_release_both_locks() {
    let fx = Fixture::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let handle = runtime.block_on(async {
        let (tx, _rx) = mpsc::unbounded_channel();
        let seams = Seams {
            dial: Arc::new(FakeDial {
                conns: tx,
                refuse: AtomicBool::new(true),
                hang: AtomicBool::new(false),
                flags: Arc::new(Flags::default()),
            }),
            signer: fx.key.clone(),
            random: shared_crypto::random_32,
            intake: Arc::new(LogIntake),
        };
        let handle = fx
            .open()
            .unwrap()
            .start_with(&fx.key.public_key(), seams)
            .unwrap();
        tokio::task::yield_now().await;
        handle
    });
    assert!(!fx.locks_are_free(), "held while the runtime lives");
    drop(runtime);
    assert!(fx.locks_are_free(), "after the runtime shut down");
    drop(handle);

    // An Opened dropped unstarted.
    let opened = fx.open().unwrap();
    assert!(!fx.locks_are_free());
    drop(opened);
    assert!(fx.locks_are_free(), "after an unstarted Opened");
    assert!(fx.connection_lock().exists() && fx.registration_lock().exists());
}

#[tokio::test(start_paused = true)]
async fn a13_a_record_edited_during_a_session_is_not_read_again() {
    let mut env = Env::new();
    env.fail_challenge(503, &[], b"").await;
    let mut edited = record_for(&env.fx.key.public_key(), ORIGIN);
    edited.agent_id = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d50".to_owned();
    env.fx.write(&edited);
    // The session request names the agent of the record as it was read at the start (`session`
    // checks the agent ID against the original).
    let _ = env.until_session().await;
}

// ---------------------------------------------------------------------------------------------
// B. The challenge route

#[tokio::test(start_paused = true)]
async fn b1_a_good_challenge_leads_to_the_session_request() {
    let mut env = Env::new();
    let (at, mut stream, req) = env.request().await;
    assert!(req.is_post(CHALLENGE_PATH));
    assert!(req.body.is_empty());
    assert!(req.header("host") == Some("127.0.0.1:8080"));
    assert!(req.header("origin").is_none());
    let challenge = shared_crypto::random_32().unwrap();
    json_reply(&mut stream, 200, &challenge_body(&challenge)).await;
    let (next, ..) = env.session(challenge).await;
    assert_after(at, next, 0, "the session request follows at once");
}

#[tokio::test(start_paused = true)]
async fn b2_an_answer_that_is_not_the_strict_challenge_is_an_ordinary_failure() {
    let mut env = Env::new();
    let good = STANDARD.encode(shared_crypto::random_32().unwrap());
    let object = |text: &str| format!(r#"{{"challenge":"{text}"}}"#);
    let mut padded = object(&good);
    padded.push_str(&" ".repeat(4097));
    let mut non_canonical = "A".repeat(42);
    non_canonical.push_str("B=");
    let cases: Vec<(&str, String, String)> = vec![
        ("text/plain", "text/plain".into(), object(&good)),
        ("over 4096 bytes", "application/json".into(), padded),
        (
            "an extra property",
            "application/json".into(),
            format!(r#"{{"challenge":"{good}","x":1}}"#),
        ),
        (
            "43 characters",
            "application/json".into(),
            object(&good[1..]),
        ),
        (
            "45 characters",
            "application/json".into(),
            object(&format!("{good}A")),
        ),
        (
            "another alphabet",
            "application/json".into(),
            object(&good.replace(|c: char| c.is_ascii_alphabetic(), "-")),
        ),
        (
            "trailing bits",
            "application/json".into(),
            object(&non_canonical),
        ),
        ("not an object", "application/json".into(), "[]".into()),
        ("not JSON", "application/json".into(), "{".into()),
        ("empty", "application/json".into(), String::new()),
    ];
    let mut times = Vec::new();
    for (_, content_type, body) in &cases {
        let at = env
            .fail_challenge(200, &[("Content-Type", content_type)], body.as_bytes())
            .await;
        times.push(at);
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(cases.len()));
    assert_eq!(
        env.log.warns("challenge-answer"),
        1,
        "one warning per class"
    );
}

#[tokio::test(start_paused = true)]
async fn b3_a_429_with_a_usable_retry_after_waits_the_longer_then_doubles() {
    let mut env = Env::new();
    let mut times = vec![env.fail_challenge(429, &[("Retry-After", "30")], b"").await];
    // The delay is 2 now: the next wait is the delay.
    times.push(env.fail_challenge(503, &[], b"").await);
    times.push(env.fail_challenge(503, &[], b"").await);
    // The delay is 8 now: a shorter Retry-After does not shorten the wait.
    times.push(env.fail_challenge(429, &[("Retry-After", "5")], b"").await);
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), vec![30, 2, 4, 8]);
    // 60 is the longest Retry-After.
    let mut env = Env::new();
    let first = env.fail_challenge(429, &[("Retry-After", "60")], b"").await;
    let (second, ..) = env.request().await;
    assert_eq!(gaps(&[first, second]), vec![60]);
}

#[tokio::test(start_paused = true)]
async fn b3_the_status_shows_the_wait_a_long_retry_after_sets() {
    let mut env = Env::new();
    let at = env.fail_challenge(429, &[("Retry-After", "30")], b"").await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.status().next_delay, secs(30));
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![30]);
    // The wait is over: the delay it doubled to is shown.
    assert_eq!(env.status().next_delay, secs(2));
}

#[tokio::test(start_paused = true)]
async fn b4_a_429_without_a_usable_retry_after_is_an_ordinary_failure() {
    let mut env = Env::new();
    let variants: Vec<Vec<(&str, &str)>> = vec![
        vec![],
        vec![("Retry-After", "0")],
        vec![("Retry-After", "61")],
        vec![("Retry-After", "abc")],
        vec![("Retry-After", "1.5")],
        vec![("Retry-After", "-3")],
        vec![("Retry-After", "")],
        vec![("Retry-After", "17"), ("Retry-After", "17")],
    ];
    let mut times = Vec::new();
    for headers in &variants {
        times.push(env.fail_challenge(429, headers, b"").await);
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(variants.len()));
}

#[tokio::test(start_paused = true)]
async fn b5_any_other_status_is_an_ordinary_failure_with_the_status_logged() {
    let mut env = Env::new();
    let mut times = Vec::new();
    for (status, headers) in [
        (503u16, vec![]),
        (500, vec![]),
        (404, vec![]),
        (302, vec![("Location", "http://127.0.0.1:9/elsewhere")]),
        (204, vec![]),
    ] {
        times.push(env.fail_challenge(status, &headers, b"").await);
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(5));
    for status in [503, 500, 404, 302, 204] {
        assert!(
            env.log.contains(&format!("status=Some({status})")),
            "the status {status} is logged"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn b6_a_transport_failure_or_a_broken_answer_is_an_ordinary_failure() {
    let mut env = Env::new();
    let mut times = Vec::new();
    // The time each failure takes before it is decided, in whole seconds.
    let mut took = Vec::new();
    let mut held = Vec::new();

    // The server closes without an answer.
    let (at, stream) = env.conn().await;
    times.push(at);
    took.push(0);
    drop(stream);
    // The server never answers: the whole call is bound at 30 s.
    let (at, mut stream) = env.conn().await;
    times.push(at);
    took.push(30);
    let _ = read_req(&mut stream).await;
    held.push(stream);
    // A head over the limits.
    let (at, mut stream, _) = env.request().await;
    times.push(at);
    took.push(0);
    let long = format!("HTTP/1.1 200 OK\r\nX-Pad: {}\r\n\r\n", "a".repeat(20_000));
    let _ = stream.write_all(long.as_bytes()).await;
    held.push(stream);
    // An answer that is not HTTP.
    let (at, mut stream, _) = env.request().await;
    times.push(at);
    took.push(0);
    stream.write_all(b"this is not http\r\n\r\n").await.unwrap();
    held.push(stream);
    // A body that is cut.
    let (at, mut stream, _) = env.request().await;
    times.push(at);
    took.push(0);
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 44\r\n\r\n{\"chal",
        )
        .await
        .unwrap();
    drop(stream);
    // A refused connection.
    env.dial.refuse.store(true, Ordering::SeqCst);
    let (at, none) = env.dialed().await;
    assert!(none.is_none());
    times.push(at);
    took.push(0);
    env.dial.refuse.store(false, Ordering::SeqCst);
    // A connect that does not finish: the connect is bound at 5 s.
    env.dial.hang.store(true, Ordering::SeqCst);
    let (at, none) = env.dialed().await;
    assert!(none.is_none());
    times.push(at);
    took.push(5);
    env.dial.hang.store(false, Ordering::SeqCst);

    times.push(env.request().await.0);
    let want: Vec<u64> = ord_delays(took.len())
        .iter()
        .zip(&took)
        .map(|(delay, took)| delay + took)
        .collect();
    assert_eq!(gaps(&times), want);
    drop(held);
}

// ---------------------------------------------------------------------------------------------
// C. The session route

#[tokio::test(start_paused = true)]
async fn c1_a_valid_token_leads_to_the_upgrade_and_clears_the_refusal_flag() {
    let mut env = Env::new();
    // A refusal warns once and sets the flag.
    let (first, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 403, "{}").await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.log.warns("control channel refused"), 1);
    // A valid answer: the upgrade follows at once, and nothing is scheduled by `expiresInSecs`.
    let (second, mut stream, _) = env.until_session().await;
    assert_after(first, second, 60, "after the refusal's cap");
    json_reply(&mut stream, 200, &token_body(&token_text())).await;
    let (third, mut stream, req) = env.request().await;
    assert_after(second, third, 0, "the upgrade follows at once");
    assert_eq!(req.method, "GET");
    reply(&mut stream, 400, &[], b"").await;
    // The flag was cleared by the token: the next refusal warns again.
    let (_, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 403, "{}").await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.log.warns("control channel refused"), 2);
}

#[tokio::test(start_paused = true)]
async fn c2_an_invalid_session_answer_is_an_ordinary_failure_and_the_token_is_not_logged() {
    let mut env = Env::new();
    let good = token_text();
    let json = "application/json";
    let cases: Vec<(&str, String, String)> = vec![
        ("text/plain", "text/plain".into(), token_body(&good)),
        (
            "an extra property",
            json.into(),
            format!(r#"{{"token":"{good}","expiresInSecs":5,"x":1}}"#),
        ),
        ("no expiry", json.into(), format!(r#"{{"token":"{good}"}}"#)),
        (
            "a short token",
            json.into(),
            format!(r#"{{"token":"{}","expiresInSecs":5}}"#, &good[1..]),
        ),
        (
            "a token of another alphabet",
            json.into(),
            format!(r#"{{"token":"{}","expiresInSecs":5}}"#, "-".repeat(44)),
        ),
        (
            "expiry 0",
            json.into(),
            format!(r#"{{"token":"{good}","expiresInSecs":0}}"#),
        ),
        (
            "expiry as text",
            json.into(),
            format!(r#"{{"token":"{good}","expiresInSecs":"5"}}"#),
        ),
        (
            "negative expiry",
            json.into(),
            format!(r#"{{"token":"{good}","expiresInSecs":-1}}"#),
        ),
        (
            "fractional expiry",
            json.into(),
            format!(r#"{{"token":"{good}","expiresInSecs":1.5}}"#),
        ),
        ("not JSON", json.into(), "{".into()),
    ];
    let mut times = Vec::new();
    for (_, content_type, body) in &cases {
        let (at, mut stream, _) = env.until_session().await;
        times.push(at);
        reply(
            &mut stream,
            200,
            &[("Content-Type", content_type)],
            body.as_bytes(),
        )
        .await;
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(cases.len()));
    assert!(!env.log.contains(&good), "the token is not logged");
    assert_eq!(env.log.warns("session-answer"), 1);
}

#[tokio::test(start_paused = true)]
async fn c3_the_first_401_gets_a_new_challenge_nonce_and_proof_at_once() {
    let mut env = Env::new();
    let (first_at, mut stream, first) = env.until_session().await;
    reply(
        &mut stream,
        401,
        &[("Content-Type", "application/json")],
        b"{}",
    )
    .await;
    let (again, challenge) = env.challenge().await;
    assert_after(first_at, again, 0, "the new challenge comes at once");
    let (_, mut stream, second) = env.session(challenge).await;
    assert!(first["nonce"] != second["nonce"], "a new nonce");
    assert!(first["challenge"] != second["challenge"], "a new challenge");
    assert!(first["proof"] != second["proof"], "a new proof");
    assert_eq!(env.status().next_delay, secs(1), "the delay is untouched");
    json_reply(&mut stream, 200, &token_body(&token_text())).await;
    let (_, _, req) = env.request().await;
    assert_eq!(req.method, "GET");
}

#[tokio::test(start_paused = true)]
async fn c4_a_second_401_is_an_ordinary_failure() {
    let mut env = Env::new();
    let (first_at, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 401, "{}").await;
    let (_, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 401, "{}").await;
    // No further request at once: the next attempt comes after the delay.
    let (next, mut stream, req) = env.request().await;
    assert!(req.is_post(CHALLENGE_PATH));
    assert_eq!(gaps(&[first_at, next]), vec![1]);
    reply(&mut stream, 503, &[], b"").await;
    let (after, ..) = env.request().await;
    assert_eq!(gaps(&[next, after]), vec![2]);
}

#[tokio::test(start_paused = true)]
async fn c5_a_403_backs_off_to_the_cap_with_one_warning() {
    let mut env = Env::new();
    let mut times = Vec::new();
    for _ in 0..3 {
        let (at, mut stream, _) = env.until_session().await;
        times.push(at);
        json_reply(&mut stream, 403, "{}").await;
        if times.len() == 1 {
            env.wait_phase(Phase::Waiting).await;
            assert_eq!(env.status().next_delay, secs(60));
        }
    }
    assert_eq!(gaps(&times), vec![60, 60]);
    assert_eq!(env.log.warns("control channel refused"), 1, "one warning");
    assert!(env.fx.record_path().exists(), "nothing is erased");
}

#[tokio::test(start_paused = true)]
async fn c6_a_429_on_the_session_route_is_as_a_403() {
    let mut env = Env::new();
    let mut times = Vec::new();
    let (at, mut stream, _) = env.until_session().await;
    times.push(at);
    json_reply(&mut stream, 403, "{}").await;
    let (at, mut stream, _) = env.until_session().await;
    times.push(at);
    // A Retry-After is not used on this route.
    reply(&mut stream, 429, &[("Retry-After", "1")], b"").await;
    let (at, ..) = env.until_session().await;
    times.push(at);
    assert_eq!(gaps(&times), vec![60, 60]);
    assert_eq!(env.log.warns("control channel refused"), 1, "one warning");
}

#[tokio::test(start_paused = true)]
async fn c7_a_400_is_an_ordinary_failure_with_a_warning() {
    let mut env = Env::new();
    let (first, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 400, "{}").await;
    let (second, mut stream, _) = env.until_session().await;
    json_reply(&mut stream, 400, "{}").await;
    let (third, ..) = env.request().await;
    assert_eq!(gaps(&[first, second, third]), vec![1, 2]);
    assert_eq!(env.log.warns("session-bad-request"), 1);
}

#[tokio::test(start_paused = true)]
async fn c8_any_other_status_or_a_transport_failure_is_an_ordinary_failure() {
    let mut env = Env::new();
    let mut times = Vec::new();
    for status in [500u16, 404, 302] {
        let (at, mut stream, _) = env.until_session().await;
        times.push(at);
        reply(&mut stream, status, &[], b"").await;
    }
    // The connection is closed before an answer.
    let (at, stream, _) = env.until_session().await;
    times.push(at);
    drop(stream);
    // The session call is refused at the dial.
    let dial = env.dial.clone();
    times.push(env.challenge_then(&dial.refuse).await);
    let (_, none) = env.dialed().await;
    assert!(none.is_none());
    env.dial.refuse.store(false, Ordering::SeqCst);
    // Every attempt begins at the challenge route (`challenge` asserts it): the old challenge
    // is never presented again.
    times.push(env.challenge().await.0);
    assert_eq!(gaps(&times), ord_delays(5));
}

#[tokio::test(start_paused = true)]
async fn c9_a_failure_to_make_a_nonce_or_a_proof_sends_nothing() {
    for random_fails in [true, false] {
        let mut env = Env::with(Opts {
            random: if random_fails {
                failing_random
            } else {
                shared_crypto::random_32
            },
            broken_signer: !random_fails,
            ..Opts::default()
        });
        let (first, _) = env.challenge().await;
        // No session request follows: the next dial is the next attempt's challenge.
        let (second, _) = env.challenge().await;
        assert_eq!(gaps(&[first, second]), vec![1]);
        assert_eq!(
            env.log
                .warns(if random_fails { "random" } else { "signing" }),
            1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn c10_a_failed_second_challenge_is_judged_as_the_first_one_is() {
    let mut env = Env::new();
    let mut times = Vec::new();
    // 401, then a challenge that fails with 503: an ordinary failure.
    let (at, mut stream, _) = env.until_session().await;
    times.push(at);
    json_reply(&mut stream, 401, "{}").await;
    env.fail_challenge(503, &[], b"").await;
    // 401, then a 429 with Retry-After: the wait is the longer one.
    let (at, mut stream, _) = env.until_session().await;
    times.push(at);
    json_reply(&mut stream, 401, "{}").await;
    env.fail_challenge(429, &[("Retry-After", "20")], b"").await;
    // 401, then a transport failure.
    let (at, mut stream, _) = env.until_session().await;
    times.push(at);
    json_reply(&mut stream, 401, "{}").await;
    let (_, stream) = env.conn().await;
    drop(stream);
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), vec![1, 20, 4]);
}

// ---------------------------------------------------------------------------------------------
// D. The upgrade

#[tokio::test(start_paused = true)]
async fn d1_a_101_with_the_subprotocol_leads_to_hello() {
    let mut env = Env::new();
    let exchanged = env.exchange().await;
    let (_, mut ws, seen) = env.upgrade().await;
    assert_eq!(seen.path, channel::CONTROL_PATH);
    assert!(seen.header("host") == Some("127.0.0.1:8080"));
    let bearer = format!("Bearer {}", exchanged.token);
    assert!(
        seen.header("authorization") == Some(bearer.as_str()),
        "the bearer token"
    );
    assert!(seen.header("sec-websocket-protocol") == Some(CONTROL_SUBPROTOCOL));
    assert!(seen.header("origin").is_none(), "no Origin");
    assert!(
        seen.header("sec-websocket-extensions").is_none(),
        "no extensions"
    );
    env.wait_phase(Phase::Hello).await;
    send(&mut ws, &hello(20)).await;
    env.wait_phase(Phase::Connected).await;

    // Port 80 has no port in Host.
    let mut env = Env::with(Opts {
        origin: "http://127.0.0.1",
        ..Opts::default()
    });
    let (_, mut stream, req) = env.request().await;
    assert!(req.header("host") == Some("127.0.0.1"));
    json_reply(&mut stream, 503, "{}").await;
}

/// A 101 the fake server writes by hand: the accept key and the subprotocol are the test's choice.
async fn raw_101(
    stream: &mut DuplexStream,
    request: &Req,
    accept: Option<&str>,
    protocol: Option<&str>,
) {
    let key = request.header("sec-websocket-key").expect("a key");
    let accept = accept
        .map(str::to_owned)
        .unwrap_or_else(|| tungstenite::handshake::derive_accept_key(key.as_bytes()));
    let mut head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n"
    );
    if let Some(protocol) = protocol {
        head.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
}

/// The agent has dropped its end: the server reads the end of the stream.
async fn assert_dropped(stream: &mut DuplexStream) {
    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(DEADLINE, stream.read(&mut buf))
        .await
        .expect("the agent did not drop the connection");
    assert!(matches!(read, Ok(0) | Err(_)), "the connection is dropped");
}

#[tokio::test(start_paused = true)]
async fn d2_a_handshake_the_stack_refuses_is_dropped_with_a_warning() {
    let mut env = Env::new();
    let mut times = Vec::new();
    for variant in 0..3 {
        let _ = env.exchange().await;
        let (at, mut stream, request) = env.request().await;
        times.push(at);
        match variant {
            0 => raw_101(&mut stream, &request, None, None).await,
            1 => raw_101(&mut stream, &request, None, Some("other.v1")).await,
            _ => {
                raw_101(
                    &mut stream,
                    &request,
                    Some("AAAAAAAAAAAAAAAAAAAAAAAAAAA="),
                    Some(CONTROL_SUBPROTOCOL),
                )
                .await
            }
        }
        assert_dropped(&mut stream).await;
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(3));
    assert_eq!(env.log.warns("upgrade-handshake"), 1);
}

/// An exchange, then the upgrade request, which the test answers by hand.
async fn to_upgrade(env: &mut Env) -> (Instant, DuplexStream, Req) {
    let _ = env.exchange().await;
    env.request().await
}

#[tokio::test(start_paused = true)]
async fn d4_a_400_is_an_ordinary_failure_with_a_warning() {
    let mut env = Env::new();
    let (first, mut stream, _) = to_upgrade(&mut env).await;
    reply(&mut stream, 400, &[], b"").await;
    let (second, ..) = env.request().await;
    assert_eq!(gaps(&[first, second]), vec![1]);
    assert_eq!(env.log.warns("upgrade-bad-request"), 1);
}

#[tokio::test(start_paused = true)]
async fn d5_a_401_is_an_ordinary_failure_with_no_immediate_exchange() {
    let mut env = Env::new();
    let (first, mut stream, _) = to_upgrade(&mut env).await;
    reply(&mut stream, 401, &[], b"").await;
    let (second, mut stream, req) = env.request().await;
    assert!(req.is_post(CHALLENGE_PATH));
    assert_eq!(gaps(&[first, second]), vec![1]);
    reply(&mut stream, 503, &[], b"").await;
}

#[tokio::test(start_paused = true)]
async fn d6_a_redirect_is_not_followed() {
    let mut env = Env::new();
    let (first, mut stream, _) = to_upgrade(&mut env).await;
    reply(
        &mut stream,
        302,
        &[("Location", "http://127.0.0.1:9/elsewhere")],
        b"",
    )
    .await;
    // The next dial is the next attempt's challenge: nothing was dialled for the location.
    let (second, mut stream, req) = env.request().await;
    assert!(req.is_post(CHALLENGE_PATH));
    assert_eq!(gaps(&[first, second]), vec![1]);
    reply(&mut stream, 503, &[], b"").await;
}

#[tokio::test(start_paused = true)]
async fn d7_any_other_status_is_an_ordinary_failure() {
    let mut env = Env::new();
    let mut times = Vec::new();
    for status in [500u16, 404, 200] {
        let (at, mut stream, _) = to_upgrade(&mut env).await;
        times.push(at);
        reply(&mut stream, status, &[], b"not an upgrade").await;
    }
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), ord_delays(3));
}

#[tokio::test(start_paused = true)]
async fn d8_a_transport_failure_is_an_ordinary_failure() {
    let mut env = Env::new();
    let mut times = Vec::new();
    // The connection ends before an answer.
    let (at, stream, _) = to_upgrade(&mut env).await;
    times.push(at);
    drop(stream);
    // The dial is refused.
    let dial = env.dial.clone();
    times.push(env.exchange_then(&dial.refuse).await);
    let (_, none) = env.dialed().await;
    assert!(none.is_none());
    env.dial.refuse.store(false, Ordering::SeqCst);
    // The connect does not finish: bound at 5 s.
    times.push(env.exchange_then(&dial.hang).await);
    let (_, none) = env.dialed().await;
    assert!(none.is_none());
    env.dial.hang.store(false, Ordering::SeqCst);
    times.push(env.request().await.0);
    assert_eq!(gaps(&times), vec![1, 2, 4 + 5]);
}

impl Env {
    /// A challenge answered after `flag` was set: the dial that follows meets the fault.
    async fn challenge_then(&mut self, flag: &AtomicBool) -> Instant {
        let (at, mut stream, req) = self.request().await;
        assert!(req.is_post(CHALLENGE_PATH));
        flag.store(true, Ordering::SeqCst);
        let challenge = shared_crypto::random_32().unwrap();
        json_reply(&mut stream, 200, &challenge_body(&challenge)).await;
        at
    }

    /// An exchange whose token is given after `flag` was set.
    async fn exchange_then(&mut self, flag: &AtomicBool) -> Instant {
        let (at, challenge) = self.challenge().await;
        let (_, mut stream, _) = self.session(challenge).await;
        flag.store(true, Ordering::SeqCst);
        json_reply(&mut stream, 200, &token_body(&token_text())).await;
        at
    }
}

#[tokio::test(start_paused = true)]
async fn d9_a_token_serves_one_request_and_every_attempt_starts_at_the_challenge() {
    let mut env = Env::new();
    let first = env.exchange().await;
    let (_, mut stream, req) = env.request().await;
    let bearer = format!("Bearer {}", first.token);
    assert!(req.header("authorization") == Some(bearer.as_str()));
    reply(&mut stream, 500, &[], b"").await;
    // The next attempt does not present the token again: it begins at the challenge, and its own
    // token is another.
    let second = env.exchange().await;
    assert!(first.token != second.token, "a new token");
    let (_, _, seen) = env.upgrade().await;
    let bearer = format!("Bearer {}", second.token);
    assert!(seen.header("authorization") == Some(bearer.as_str()));
}

// ---------------------------------------------------------------------------------------------
// E. Between the 101 and hello

#[tokio::test(start_paused = true)]
async fn e1_a_valid_hello_connects_and_ticks_fall_on_the_interval() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(60).await;
    let status = env.status();
    assert_eq!(status.phase, Phase::Connected);
    assert_eq!(status.heartbeat, Some(secs(60)));
    assert_after(hello_at, heartbeat(&mut ws).await, 60, "the first tick");
    assert_after(hello_at, heartbeat(&mut ws).await, 120, "the second tick");
    assert_eq!(
        env.log
            .lines("INFO")
            .iter()
            .filter(|l| l.contains("control channel connected"))
            .count(),
        1
    );
    assert_eq!(env.log.warns(""), 0);
}

#[tokio::test(start_paused = true)]
async fn e2_no_hello_by_the_deadline_closes_with_1002() {
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    let upgraded_at = Instant::now();
    let at = close_of(&mut ws, 1002, true).await;
    assert_after(upgraded_at, at, 10, "the hello deadline");
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn e3_text_that_does_not_decode_closes_with_1002() {
    let mut env = Env::new();
    let texts = [
        "not json".to_owned(),
        r#"{"type":"nope"}"#.to_owned(),
        hello(0),
        hello(61),
        r#"{"type":"hello","heartbeatSecs":20,"extra":1}"#.to_owned(),
    ];
    for text in &texts {
        let (_, mut ws) = env.upgraded().await;
        send(&mut ws, text).await;
        close_of(&mut ws, 1002, true).await;
    }
    assert!(env.status().heartbeat.is_none(), "never connected");
}

#[tokio::test(start_paused = true)]
async fn e4_a_valid_message_that_is_not_hello_closes_with_1002_and_counts_nothing() {
    let mut env = Env::new();
    let texts = [
        r#"{"type":"heartbeat"}"#.to_owned(),
        job_command("cmd-1"),
        r#"{"type":"eventAck","seq":1}"#.to_owned(),
    ];
    for text in &texts {
        let (_, mut ws) = env.upgraded().await;
        send(&mut ws, text).await;
        close_of(&mut ws, 1002, true).await;
    }
    assert_eq!(env.counts(), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn e5_a_binary_frame_closes_with_1003() {
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    ws.send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    close_of(&mut ws, 1003, true).await;
}

#[tokio::test(start_paused = true)]
async fn e6_a_frame_the_stack_refuses_gets_its_close_and_the_agent_waits_without_reading() {
    let mut env = Env::new();
    // The fault's instant and the least time the next attempt waits (the linger or the delay).
    let mut earlier: Option<(Instant, u64)> = None;
    for (k, fault) in FAULTS.into_iter().enumerate() {
        let (started, mut ws) = env.upgraded().await;
        if let Some((at, want)) = earlier {
            assert_after(at, started, want, "the linger or the delay");
        }
        let code = inject(&mut ws, fault).await;
        earlier = Some((Instant::now(), 5u64.max(1u64 << k.min(6))));
        // The server does not answer: the agent keeps the connection for 5 s without reading.
        let closed = close_of(&mut ws, code, false).await;
        assert_after(earlier.unwrap().0, closed, 0, "the close comes at once");
    }
}

#[tokio::test(start_paused = true)]
async fn e7_a_ping_is_answered_by_the_stack_and_does_not_extend_the_hello_deadline() {
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    let upgraded_at = Instant::now();
    tokio::time::sleep(secs(3)).await;
    ws.send(Message::Ping(vec![9].into())).await.unwrap();
    // The pong comes without the agent writing anything else.
    let (at, message) = got(&mut ws).await;
    assert_eq!(message, Got::Pong);
    assert_after(upgraded_at, at, 3, "the pong");
    tokio::time::sleep(secs(2)).await;
    ws.send(Message::Pong(Vec::<u8>::new().into()))
        .await
        .unwrap();
    let at = close_of(&mut ws, 1002, true).await;
    assert_after(upgraded_at, at, 10, "the deadline is not extended");
}

#[tokio::test(start_paused = true)]
async fn e8_a_server_close_is_answered_and_an_eof_is_dropped() {
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    ws.send(close_frame(Some(1000))).await.unwrap();
    let at = Instant::now();
    // The stack answers the server's close.
    let (_, message) = got(&mut ws).await;
    assert!(matches!(message, Got::Close(_)));
    // The server ends the TCP connection after the answer, as the real one does.
    drop(ws);
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
    // An end of stream before hello.
    let mut env = Env::new();
    let (_, ws) = env.upgraded().await;
    let at = Instant::now();
    drop(ws);
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn e9_stop_before_hello_closes_with_1001() {
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    env.wait_phase(Phase::Hello).await;
    env.handle.stop();
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    assert!(
        env.conns.try_recv().is_err(),
        "nothing is dialled after the stop"
    );
}

// ---------------------------------------------------------------------------------------------
// F. Connected

#[tokio::test(start_paused = true)]
async fn f1_a_heartbeat_from_the_server_sends_nothing_and_extends_the_idle_deadline() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 10, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 2");
    // Unsolicited, between ticks.
    until(hello_at + secs(25)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    // Nothing is sent in answer: the next message is the tick.
    assert_after(hello_at, heartbeat(&mut ws).await, 30, "tick 3");
    // An echo of that tick, a second later.
    until(hello_at + secs(31)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 40, "tick 4");
    assert_after(hello_at, heartbeat(&mut ws).await, 50, "tick 5");
    assert_after(hello_at, heartbeat(&mut ws).await, 60, "tick 6");
    // The last frame was at 31: idle after three intervals more.
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 61, "the idle close");
}

#[test]
fn f2_the_heartbeat_text_is_what_the_message_encodes_to() {
    assert_eq!(AgentMessage::Heartbeat.encode().unwrap(), HEARTBEAT);
}

#[tokio::test(start_paused = true)]
async fn f2_a_tick_sends_a_heartbeat_and_leaves_the_last_frame_alone() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(20).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 40, "tick 2");
    // The agent's own heartbeats did not count as frames received: the idle close is at 3
    // intervals from hello.
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 60, "idle");
}

#[tokio::test(start_paused = true)]
async fn f3_a_write_that_fails_or_takes_over_10_seconds_drops_the_connection() {
    let mut env = Env::new();
    // A write that fails at the tick: no frame, the connection is dropped.
    let (hello_at, mut ws) = env.connected(20).await;
    env.dial.flags.fail_write.store(true, Ordering::SeqCst);
    env.wait_phase(Phase::Waiting).await;
    assert_after(hello_at, Instant::now(), 20, "the failed write");
    env.dial.flags.fail_write.store(false, Ordering::SeqCst);
    assert_eq!(got(&mut ws).await.1, Got::Eof, "nothing was written");
    // A write that does not finish: the bound is 10 s.
    let (hello_at, mut ws) = env.connected(20).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    env.wait_phase(Phase::Waiting).await;
    assert_after(
        hello_at,
        Instant::now(),
        30,
        "the stalled write ends 10 s after the tick",
    );
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    assert_eq!(got(&mut ws).await.1, Got::Eof, "nothing was written");
}

/// F3b: a job command that is the first frame at 60 s or later after `hello` makes the connection
/// one that lived, so the failure of its answer's write resets the delay (1 s, then the ordinary
/// doubling), though the loop's earlier last frame was `hello` itself.
async fn job_command_at_60_s_then_the_answer_write_goes_wrong(stall: bool) {
    let mut env = Env::new();
    // Three failures: the delay would be 8 s afterwards.
    for _ in 0..3 {
        env.fail_challenge(503, &[], b"").await;
    }
    // The command is the first frame received after `hello` and comes at 61 s, after the tick at
    // 60 s (an answer is not a frame, so the last frame is the command's).
    let (hello_at, mut ws) = env.connected(60).await;
    until(hello_at + secs(61)).await;
    let flag = if stall {
        &env.dial.flags.stall_write
    } else {
        &env.dial.flags.fail_write
    };
    flag.store(true, Ordering::SeqCst);
    send(
        &mut ws,
        &signed_command(&env.fx, "c", AGENT_ID, Tamper::None).text,
    )
    .await;
    env.wait_phase(Phase::Waiting).await;
    let ended = Instant::now();
    assert_after(
        hello_at,
        ended,
        if stall { 71 } else { 61 },
        "the answer's write",
    );
    env.dial.flags.fail_write.store(false, Ordering::SeqCst);
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    assert_eq!(env.counts(), (0, 0));
    assert_eq!(env.intake.0.lock().unwrap().len(), 1);
    // RST: the delay is 1 s, not 8; then ORD: the next failure doubles it.
    let (next, mut stream, _) = env.request().await;
    assert_eq!(gaps(&[ended, next]), vec![1], "reset to 1 s, not 8");
    reply(&mut stream, 503, &[], b"").await;
    let (after, ..) = env.request().await;
    assert_eq!(gaps(&[next, after]), vec![2]);
}

#[tokio::test(start_paused = true)]
async fn f3b_a_job_command_at_60_s_whose_answer_write_fails_resets_the_delay() {
    job_command_at_60_s_then_the_answer_write_goes_wrong(false).await;
}

#[tokio::test(start_paused = true)]
async fn f3b_a_job_command_at_60_s_whose_answer_write_stalls_resets_the_delay() {
    job_command_at_60_s_then_the_answer_write_goes_wrong(true).await;
}

#[tokio::test(start_paused = true)]
async fn f4_no_frame_for_three_intervals_closes_with_4003() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(20).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "hb");
    assert_after(hello_at, heartbeat(&mut ws).await, 40, "2hb");
    // No heartbeat at 3hb: the idle deadline comes first.
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 60, "the idle close");
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
}

#[tokio::test(start_paused = true)]
async fn f5_a_ping_or_a_pong_counts_as_a_frame() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 10, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 2");
    until(hello_at + secs(25)).await;
    ws.send(Message::Ping(vec![1].into())).await.unwrap();
    // The pong is flushed without any other write by the agent.
    let (at, message) = got(&mut ws).await;
    assert_eq!(message, Got::Pong);
    assert_after(hello_at, at, 25, "the pong");
    assert_after(hello_at, heartbeat(&mut ws).await, 30, "tick 3");
    until(hello_at + secs(32)).await;
    ws.send(Message::Pong(Vec::<u8>::new().into()))
        .await
        .unwrap();
    for tick in [40, 50, 60] {
        assert_after(hello_at, heartbeat(&mut ws).await, tick, "a tick");
    }
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 62, "idle 3 intervals after the pong");
}

#[tokio::test(start_paused = true)]
async fn f6_a_partial_frame_counts_as_nothing() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 10, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 2");
    until(hello_at + secs(25)).await;
    // The head of a frame, and no payload.
    raw(&mut ws, &[0x81, 20]).await;
    let at = close_of(&mut ws, 4003, false).await;
    assert_after(hello_at, at, 30, "idle, counted from hello");
}

#[tokio::test(start_paused = true)]
async fn f7_a_second_hello_closes_with_1002() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    send(&mut ws, &hello(10)).await;
    close_of(&mut ws, 1002, true).await;
}

#[tokio::test(start_paused = true)]
async fn f8_a_verified_job_command_is_answered_with_an_ack_and_the_answer_is_no_frame() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    assert_after(hello_at, heartbeat(&mut ws).await, 10, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 2");
    until(hello_at + secs(25)).await;
    let first = signed_command(&env.fx, "cmd-1", AGENT_ID, Tamper::None);
    let second = signed_command(&env.fx, "cmd-2", AGENT_ID, Tamper::None);
    send(&mut ws, &first.text).await;
    send(&mut ws, &second.text).await;
    for id in ["cmd-1", "cmd-2"] {
        let (at, answer) = answer(&mut ws).await;
        assert_after(hello_at, at, 25, "the answer comes at once");
        assert_eq!(
            answer,
            serde_json::json!({"type": "commandAck", "commandId": id})
        );
    }
    assert_eq!(env.counts(), (2, 0));
    // The intake had both, byte for byte, before the acks.
    assert_eq!(
        *env.intake.0.lock().unwrap(),
        vec![
            (JOB_ID.to_owned(), first.envelope),
            (JOB_ID.to_owned(), second.envelope)
        ]
    );
    // The answers did not reset the idle deadline, and the next message is the tick at 30. The
    // last frame received was at 25: the idle close is at 55.
    assert_after(hello_at, heartbeat(&mut ws).await, 30, "tick");
    assert_after(hello_at, heartbeat(&mut ws).await, 40, "tick");
    assert_after(hello_at, heartbeat(&mut ws).await, 50, "tick");
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 55, "idle from the job command");
    // No refusal: no end-of-connection line.
    assert!(!env.log.contains("refused on this connection"));
}

/// Every reason the gate has is answered `commandRejected` with the command's ID and logged
/// once, without a field of the payload.
#[tokio::test(start_paused = true)]
async fn f9_every_refusal_reason_is_answered_with_a_rejection_and_logged() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    let cases = [
        ("malformed", "Malformed", Tamper::Truncate),
        ("unknownKey", "UnknownKey", Tamper::ForeignKey),
        ("badSignature", "BadSignature", Tamper::FlipPayload),
        ("unsupportedVersion", "UnsupportedVersion", Tamper::Format),
        ("notForThisAgent", "NotForThisAgent", Tamper::OtherAgent),
    ];
    for (k, (wire, word, tamper)) in cases.iter().enumerate() {
        let id = format!("cmd-{wire}");
        send(
            &mut ws,
            &signed_command(&env.fx, &id, AGENT_ID, *tamper).text,
        )
        .await;
        let (_, got) = answer(&mut ws).await;
        assert_eq!(
            got,
            serde_json::json!({"type": "commandRejected", "commandId": id, "reason": wire})
        );
        assert_eq!(env.counts(), (0, k as u64 + 1));
        assert_eq!(env.log.warns("job instruction refused"), k + 1);
        assert!(
            env.log
                .lines("WARN")
                .iter()
                .any(|line| line.contains(&format!("reason=\"{word}\""))),
            "{word}"
        );
    }
    assert!(env.intake.0.lock().unwrap().is_empty(), "nothing was taken");
    assert_eq!(env.status().phase, Phase::Connected);
    // The unknown key's entry names the fingerprint it offered, which is a hash.
    assert!(env.log.contains("offered_key="));
    let text = env.log.text();
    for forbidden in [
        "cmd-",
        JOB_ID,
        "other-agent",
        "operator-1",
        "VIN-UNDER-TEST",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in the log");
    }
}

#[tokio::test(start_paused = true)]
async fn f9_a_job_command_without_a_job_is_answered_malformed() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    let texts = [
        r#"{"type":"jobCommand","commandId":"c1"}"#,
        r#"{"type":"jobCommand","commandId":"c2","signedJob":7}"#,
        r#"{"type":"jobCommand","commandId":"c3","signedJob":null}"#,
        // Text that is not base64 is the same refusal.
        r#"{"type":"jobCommand","commandId":"c4","signedJob":"not base64!"}"#,
    ];
    for (k, text) in texts.iter().enumerate() {
        send(&mut ws, text).await;
        let (_, got) = answer(&mut ws).await;
        assert_eq!(
            got,
            serde_json::json!({
                "type": "commandRejected",
                "commandId": format!("c{}", k + 1),
                "reason": "malformed"
            })
        );
    }
    assert_eq!(env.counts(), (0, 4));
    assert_eq!(env.log.warns("job instruction refused"), 1);
    assert_eq!(env.status().phase, Phase::Connected);
}

/// A flood of forged commands writes one warning per reason on the connection, and the end of the
/// connection one line with the counts; the next connection starts again.
#[tokio::test(start_paused = true)]
async fn f9_a_flood_of_forged_commands_logs_once_per_reason_and_counts_at_the_end() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    for k in 0..20 {
        let id = format!("forged-{k}");
        send(
            &mut ws,
            &signed_command(&env.fx, &id, AGENT_ID, Tamper::ForeignKey).text,
        )
        .await;
        let _ = answer(&mut ws).await;
    }
    for k in 0..3 {
        let id = format!("bad-{k}");
        send(
            &mut ws,
            &signed_command(&env.fx, &id, AGENT_ID, Tamper::FlipPayload).text,
        )
        .await;
        let _ = answer(&mut ws).await;
    }
    assert_eq!(env.counts(), (0, 23));
    assert_eq!(env.log.warns("job instruction refused"), 2);
    let debug = env
        .log
        .lines("DEBUG")
        .iter()
        .filter(|line| line.contains("job instruction refused"))
        .count();
    assert_eq!(debug, 21);
    assert!(!env.log.contains("refused on this connection"));
    drop(ws);
    env.wait_phase(Phase::Waiting).await;
    let lines: Vec<String> = env
        .log
        .lines("INFO")
        .into_iter()
        .filter(|line| line.contains("refused on this connection"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    for want in [
        "unknown_key=20",
        "bad_signature=3",
        "malformed=0",
        "unsupported_version=0",
        "not_for_this_agent=0",
    ] {
        assert!(lines[0].contains(want), "{want} in {}", lines[0]);
    }
    assert!(!env.log.contains("forged-") && !env.log.contains("bad-"));
    // A new connection warns again, and its counts are its own.
    let (_, mut ws) = env.connected(10).await;
    send(
        &mut ws,
        &signed_command(&env.fx, "again", AGENT_ID, Tamper::ForeignKey).text,
    )
    .await;
    let _ = answer(&mut ws).await;
    assert_eq!(env.log.warns("job instruction refused"), 3);
    assert_eq!(env.counts(), (0, 24));
}

#[tokio::test(start_paused = true)]
async fn f9_a_stalled_answer_is_dropped_after_10_seconds_and_counts_nothing() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(60).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    let at = Instant::now();
    send(
        &mut ws,
        &signed_command(&env.fx, "c", AGENT_ID, Tamper::None).text,
    )
    .await;
    env.wait_phase(Phase::Waiting).await;
    assert_after(at, Instant::now(), 10, "the write bound");
    assert!(Instant::now() < hello_at + secs(60));
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    assert_eq!(got(&mut ws).await.1, Got::Eof, "nothing was written");
    assert_eq!(env.counts(), (0, 0));
    // F8b: the intake took the instruction before the ack, which was lost.
    assert_eq!(env.intake.0.lock().unwrap().len(), 1);
    // The server redelivers it under a new commandId; the intake gets it a second time.
    let (_, mut ws) = env.connected(60).await;
    send(
        &mut ws,
        &signed_command(&env.fx, "c-again", AGENT_ID, Tamper::None).text,
    )
    .await;
    let (_, got) = answer(&mut ws).await;
    assert_eq!(
        got,
        serde_json::json!({"type": "commandAck", "commandId": "c-again"})
    );
    assert_eq!(env.counts(), (1, 0));
    assert_eq!(env.intake.0.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn f9_a_stop_during_a_stalled_answer_drops_the_connection_at_once() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(60).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    send(
        &mut ws,
        &signed_command(&env.fx, "c", AGENT_ID, Tamper::None).text,
    )
    .await;
    tokio::time::sleep(secs(2)).await;
    let at = Instant::now();
    env.handle.stop();
    tokio::time::timeout(DEADLINE, env.handle.stopped())
        .await
        .expect("the stop waited for the stalled write");
    assert_after(at, Instant::now(), 0, "no waiting");
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    assert_eq!(got(&mut ws).await.1, Got::Eof, "no answer and no Close");
    assert_eq!(env.counts(), (0, 0));
    // F8b: the intake took the instruction before the ack.
    assert_eq!(env.intake.0.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn f9_a_command_that_comes_after_the_stop_is_not_answered() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(60).await;
    env.handle.stop();
    send(
        &mut ws,
        &signed_command(&env.fx, "late", AGENT_ID, Tamper::None).text,
    )
    .await;
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    assert_eq!(env.counts(), (0, 0));
    assert!(env.intake.0.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn f9_a_command_behind_the_peers_close_is_not_answered() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(60).await;
    ws.send(close_frame(Some(1000))).await.unwrap();
    // The stack may refuse a write after its Close; the command is behind it either way.
    let _ = ws
        .send(Message::Text(
            signed_command(&env.fx, "late", AGENT_ID, Tamper::None)
                .text
                .into(),
        ))
        .await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.counts(), (0, 0));
    assert!(env.intake.0.lock().unwrap().is_empty());
    while let (_, Got::Text(text)) = got(&mut ws).await {
        assert!(!text.contains("commandAck"), "{text}");
    }
}

#[tokio::test(start_paused = true)]
async fn f10_a_job_command_without_a_usable_id_or_with_an_extra_property_closes_with_1002() {
    let mut env = Env::new();
    for text in [
        r#"{"type":"jobCommand","signedJob":"AA=="}"#,
        r#"{"type":"jobCommand","commandId":5,"signedJob":"AA=="}"#,
        r#"{"type":"jobCommand","commandId":"c","signedJob":"AA==","extra":1}"#,
    ] {
        let (_, mut ws) = env.connected(10).await;
        send(&mut ws, text).await;
        close_of(&mut ws, 1002, true).await;
    }
    assert_eq!(env.counts(), (0, 0));
}

/// F9c: a `commandId` whose longest possible answer (a `commandRejected` with the longest reason
/// word) cannot be encoded is not usable, as a missing one is not: the connection is closed with
/// 1002 before the gate, so nothing is taken, counted or logged by it. One byte less is still
/// answered.
#[tokio::test(start_paused = true)]
async fn f9c_a_command_id_whose_answer_cannot_be_encoded_closes_with_1002() {
    use shared_proto::{InstructionRejectReason, MAX_CONTROL_MESSAGE_LEN};
    let encodes = |len: usize, reason: InstructionRejectReason| {
        AgentMessage::CommandRejected {
            command_id: "x".repeat(len),
            reason,
        }
        .encode()
        .is_ok()
    };
    // The longest ID whose answer with the longest reason still encodes, found from the codec.
    let longest = (0..MAX_CONTROL_MESSAGE_LEN)
        .rev()
        .find(|len| encodes(*len, channel::LONGEST_REASON))
        .unwrap();
    assert!(!encodes(longest + 1, channel::LONGEST_REASON));
    // No other reason's answer is longer, so the check covers every answer.
    for reason in [
        InstructionRejectReason::Malformed,
        InstructionRejectReason::UnknownKey,
        InstructionRejectReason::BadSignature,
        InstructionRejectReason::UnsupportedVersion,
        InstructionRejectReason::NotForThisAgent,
    ] {
        assert!(encodes(longest, reason), "{reason:?}");
    }
    let command = |len: usize| {
        let text = format!(
            r#"{{"type":"jobCommand","commandId":"{}"}}"#,
            "x".repeat(len)
        );
        assert!(text.len() <= MAX_CONTROL_MESSAGE_LEN, "the command decodes");
        text
    };
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    send(&mut ws, &command(longest)).await;
    let (_, got) = answer(&mut ws).await;
    assert_eq!(got["type"], "commandRejected");
    assert_eq!(env.counts(), (0, 1));
    send(&mut ws, &command(longest + 1)).await;
    close_of(&mut ws, 1002, true).await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.counts(), (0, 1));
    assert!(env.intake.0.lock().unwrap().is_empty());
    // The gate saw the first command only.
    let summary = env.log.lines("refused on this connection");
    assert_eq!(summary.len(), 1, "{summary:?}");
    assert!(summary[0].contains("malformed=1"), "{summary:?}");
}

/// F-drain: a job command read while the agent drains its own Close (after 4003 or 1002) is
/// discarded: no answer and no intake.
#[tokio::test(start_paused = true)]
async fn f_drain_a_job_command_during_the_agents_own_close_is_discarded() {
    for second_hello in [false, true] {
        let mut env = Env::new();
        let (_, mut ws) = env.connected(10).await;
        let code = if second_hello {
            send(&mut ws, &hello(10)).await;
            1002
        } else {
            4003
        };
        // `finish` sets the phase before it writes the Close, and this end has not read the
        // Close yet, so the stack is still open for writing: the command goes to the pipe while
        // the agent drains.
        env.wait_phase(Phase::Closing).await;
        ws.send(Message::Text(
            signed_command(&env.fx, "late", AGENT_ID, Tamper::None)
                .text
                .into(),
        ))
        .await
        .expect("the command is written while the agent drains");
        let (_, got_code) = close_after_heartbeats(&mut ws, false).await;
        assert_eq!(got_code, Some(code));
        env.wait_phase(Phase::Waiting).await;
        assert_eq!(env.counts(), (0, 0));
        assert!(env.intake.0.lock().unwrap().is_empty());
        // The stack stops reading at the agent's Close; whatever the agent wrote after it is
        // still in the pipe. There must be none: the command was not answered.
        let mut rest = Vec::new();
        let ended = tokio::time::timeout(DEADLINE, ws.get_mut().read_to_end(&mut rest)).await;
        assert!(matches!(ended, Ok(Ok(_))), "the agent ended the connection");
        assert!(rest.is_empty(), "the agent wrote after its Close: {rest:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn f11_other_valid_messages_are_dropped_with_a_debug_line_and_count_as_frames() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    let texts = [
        format!(r#"{{"type":"cancelCommand","jobId":"{JOB_ID}"}}"#),
        format!(r#"{{"type":"hmiResponse","jobId":"{JOB_ID}","requestId":"r1"}}"#),
        format!(r#"{{"type":"seedKeyResponse","jobId":"{JOB_ID}","requestId":"r1","key":"AAAA"}}"#),
        r#"{"type":"eventAck","seq":3}"#.to_owned(),
    ];
    assert_after(hello_at, heartbeat(&mut ws).await, 10, "tick 1");
    assert_after(hello_at, heartbeat(&mut ws).await, 20, "tick 2");
    until(hello_at + secs(25)).await;
    for text in &texts {
        send(&mut ws, text).await;
    }
    assert_after(
        hello_at,
        heartbeat(&mut ws).await,
        30,
        "tick: nothing was sent before it",
    );
    for tick in [40, 50] {
        assert_after(hello_at, heartbeat(&mut ws).await, tick, "tick");
    }
    let at = close_of(&mut ws, 4003, true).await;
    assert_after(hello_at, at, 55, "idle from the last of them");
    assert_eq!(env.counts(), (0, 0));
    let debug = env
        .log
        .lines("DEBUG")
        .iter()
        .filter(|line| line.contains("not handled"))
        .count();
    assert_eq!(debug, texts.len());
}

#[tokio::test(start_paused = true)]
async fn f12_text_that_does_not_decode_closes_with_1002() {
    let mut env = Env::new();
    for text in [
        "not json".to_owned(),
        r#"{"type":"nope"}"#.to_owned(),
        r#"{"type":"heartbeat","extra":1}"#.to_owned(),
        r#"{"type":"eventAck"}"#.to_owned(),
    ] {
        let (_, mut ws) = env.connected(10).await;
        send(&mut ws, &text).await;
        close_of(&mut ws, 1002, true).await;
    }
}

#[test]
fn f13_a_decode_error_of_too_large_closes_with_1009() {
    // The stack's limit is the decoder's, so the stack refuses such a text first (F15); what
    // the loop does with the decoder's error is its close code.
    let text = "x".repeat(MAX_CONTROL_MESSAGE_LEN + 1);
    let error = ServerMessage::decode(&text).unwrap_err();
    assert_eq!(error.close_code(), CLOSE_CODE_TOO_BIG);
}

#[tokio::test(start_paused = true)]
async fn f14_a_binary_frame_closes_with_1003() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    ws.send(Message::Binary(vec![0].into())).await.unwrap();
    close_of(&mut ws, 1003, true).await;
}

#[tokio::test(start_paused = true)]
async fn f15_a_frame_over_the_limit_or_against_the_rules_gets_its_close() {
    let mut env = Env::new();
    // 131072 bytes pass.
    let (_, mut ws) = env.connected(10).await;
    let head = r#"{"type":"jobCommand","commandId":"c","signedJob":""#;
    let pad = MAX_CONTROL_MESSAGE_LEN - head.len() - 2;
    send(&mut ws, &format!("{head}{}\"}}", "A".repeat(pad))).await;
    env.wait_commands(1).await;
    assert_eq!(env.status().phase, Phase::Connected);
    drop(ws);
    env.wait_phase(Phase::Waiting).await;
    // 131073 do not, and the rest are against the rules: the agent waits without reading.
    for fault in FAULTS {
        let (_, mut ws) = env.connected(10).await;
        let code = inject(&mut ws, fault).await;
        let at = Instant::now();
        let closed = close_of(&mut ws, code, false).await;
        assert_after(at, closed, 0, "the close comes at once");
    }
}

#[tokio::test(start_paused = true)]
async fn f16_an_end_of_stream_or_an_io_error_drops_the_connection() {
    let mut env = Env::new();
    let (_, ws) = env.connected(10).await;
    let at = Instant::now();
    drop(ws);
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
    // An I/O error: the next read fails.
    let (_, mut ws) = env.connected(10).await;
    env.dial.flags.fail_read.store(true, Ordering::SeqCst);
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    env.wait_phase(Phase::Waiting).await;
    env.dial.flags.fail_read.store(false, Ordering::SeqCst);
    assert!(env.status().heartbeat.is_none());
}

#[tokio::test(start_paused = true)]
async fn f17_stop_closes_with_1001_and_the_task_ends() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    env.handle.stop();
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    assert_eq!(env.status().phase, Phase::Stopped);
    assert!(env.conns.try_recv().is_err());
}

// ---------------------------------------------------------------------------------------------
// G. A close from the server

/// A connection with `hello` 10 that the server closes with `code`; returns when it did.
async fn server_closes(env: &mut Env, code: Option<u16>) -> Instant {
    let (_, mut ws) = env.connected(10).await;
    ws.send(close_frame(code)).await.unwrap();
    let at = Instant::now();
    // The stack answers: one more read of the agent lets it.
    let (_, message) = got(&mut ws).await;
    assert!(matches!(message, Got::Close(_)));
    at
}

#[tokio::test(start_paused = true)]
async fn g1_1001_is_an_end_with_an_info_line() {
    let mut env = Env::new();
    let at = server_closes(&mut env, Some(1001)).await;
    let (next, _) = env.challenge().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
    assert_eq!(env.log.lines("WARN").len(), 0);
    assert!(env.log.contains("code=Some(1001)"));
}

#[tokio::test(start_paused = true)]
async fn g2_1000_no_code_or_an_unknown_code_is_an_end_with_an_info_line() {
    let mut env = Env::new();
    for code in [Some(1000), None, Some(4999), Some(3000)] {
        let _ = server_closes(&mut env, code).await;
    }
    let _ = env.challenge().await;
    assert_eq!(env.log.lines("WARN").len(), 0);
    assert!(env.log.contains("code=None"));
    assert!(!env.log.contains("reason-text-that-must-not-be-logged"));
}

#[tokio::test(start_paused = true)]
async fn g3_1002_1003_and_1009_are_an_end_with_a_warning() {
    let mut env = Env::new();
    for code in [1002, 1003, 1009] {
        let _ = server_closes(&mut env, Some(code)).await;
    }
    let _ = env.challenge().await;
    for code in [1002, 1003, 1009] {
        assert!(env.log.contains(&format!("code=Some({code})")));
    }
    // Each connection warns once, since `hello` opened a new window.
    assert_eq!(env.log.warns("server-closed-protocol"), 3);
}

/// A connection that lived 65 seconds from `hello` to its last frame, then the server's close.
async fn lived_then_closed(env: &mut Env, code: u16) -> Instant {
    let (hello_at, mut ws) = env.connected(30).await;
    until(hello_at + secs(65)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    until(hello_at + secs(70)).await;
    ws.send(close_frame(Some(code))).await.unwrap();
    let at = Instant::now();
    let _ = close_after_heartbeats(&mut ws, true).await;
    at
}

#[tokio::test(start_paused = true)]
async fn g4_4001_backs_off_to_the_cap_even_when_the_connection_lived() {
    let mut env = Env::new();
    let at = lived_then_closed(&mut env, 4001).await;
    let (next, _) = env.challenge().await;
    assert_eq!(gaps(&[at, next]), vec![60]);
    assert_eq!(env.log.warns("replaced"), 1);
}

#[tokio::test(start_paused = true)]
async fn g5_4002_backs_off_to_the_cap_with_one_refusal_warning_and_erases_nothing() {
    let mut env = Env::new();
    let record = std::fs::read(env.fx.record_path()).unwrap();
    let at = lived_then_closed(&mut env, 4002).await;
    let (next, challenge) = env.challenge().await;
    assert_eq!(gaps(&[at, next]), vec![60]);
    assert_eq!(env.log.warns("control channel refused"), 1);
    // The agent keeps running; a 403 now is no new warning, since the flag is set.
    let (_, mut stream, _) = env.session(challenge).await;
    json_reply(&mut stream, 403, "{}").await;
    env.wait_phase(Phase::Waiting).await;
    assert_eq!(env.log.warns("control channel refused"), 1);
    assert_ne!(env.status().phase, Phase::Stopped);
    assert!(
        std::fs::read(env.fx.record_path()).unwrap() == record,
        "the record is untouched"
    );
}

#[tokio::test(start_paused = true)]
async fn g6_4003_is_an_end_with_a_warning() {
    let mut env = Env::new();
    let at = server_closes(&mut env, Some(4003)).await;
    let (next, _) = env.challenge().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
    assert_eq!(env.log.warns("server-closed-idle"), 1);
}

#[tokio::test(start_paused = true)]
async fn g8_a_close_code_the_stack_rewrites_is_seen_as_1002() {
    // The stack replaces a received Close code it does not accept (here 2000) with 1002 before the
    // agent sees it, so the agent logs 1002 as a protocol close at warn. That is accepted: the
    // path is G3's, the delay is the ordinary one.
    let mut env = Env::new();
    let at = server_closes(&mut env, Some(2000)).await;
    let (next, _) = env.challenge().await;
    assert_eq!(gaps(&[at, next]), vec![1]);
    assert_eq!(env.log.warns("server-closed-protocol"), 1);
    assert!(env.log.contains("code=Some(1002)"));
    assert!(!env.log.contains("code=Some(2000)"));
}

#[tokio::test(start_paused = true)]
async fn g7_a_close_while_the_agent_drains_its_own_is_discarded() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    let at = close_of_after(&mut ws, hello_at).await;
    // The server answers with a Close of its own, 4001, by hand: the agent's decision stands.
    let mut bytes = vec![0x88, 2];
    bytes.extend_from_slice(&4001u16.to_be_bytes());
    raw(&mut ws, &bytes).await;
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[at, next]), vec![1], "an ordinary delay, not the cap");
}

/// The idle close of a server that is silent: skips the heartbeats.
async fn close_of_after(ws: &mut Ws, hello_at: Instant) -> Instant {
    let (at, code) = close_after_heartbeats(ws, false).await;
    assert_eq!(code, Some(4003));
    assert_after(hello_at, at, 30, "the idle close");
    at
}

// ---------------------------------------------------------------------------------------------
// H. Stop, by phase

#[tokio::test(start_paused = true)]
async fn h1_stop_while_waiting_returns_at_once() {
    let mut env = Env::new();
    env.fail_challenge(503, &[], b"").await;
    env.wait_phase(Phase::Waiting).await;
    let at = Instant::now();
    env.handle.stop();
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 0, "no waiting");
}

#[tokio::test(start_paused = true)]
async fn h2_stop_during_an_http_call_drops_the_call() {
    let mut env = Env::new();
    let (_, mut stream, _) = env.request().await;
    env.handle.stop();
    env.handle.stopped().await;
    assert_dropped(&mut stream).await;
}

#[tokio::test(start_paused = true)]
async fn h3_stop_during_the_upgrade_handshake_drops_the_connection() {
    let mut env = Env::new();
    let _ = env.exchange().await;
    let (_, mut stream, _) = env.request().await;
    env.handle.stop();
    env.handle.stopped().await;
    assert_dropped(&mut stream).await;
}

#[tokio::test(start_paused = true)]
async fn h4_stop_after_the_101_closes_with_1001_within_the_write_and_drain_bounds() {
    // The peer answers: the stop returns at once.
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    let at = Instant::now();
    env.handle.stop();
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 0, "answered");

    // The peer does not answer: the drain is bound at 5 s.
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    let at = Instant::now();
    env.handle.stop();
    close_of(&mut ws, 1001, false).await;
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 5, "the drain");

    // The write does not finish: bound at 10 s, and no drain after it.
    let mut env = Env::new();
    let (_, _ws) = env.connected(10).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    let at = Instant::now();
    env.handle.stop();
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 10, "the write");
}

#[tokio::test(start_paused = true)]
async fn h5_stop_during_the_agents_own_drain_or_linger_ends_it() {
    // A drain: the idle close is written, the server does not answer, a stop comes.
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    let at = close_of_after(&mut ws, hello_at).await;
    tokio::time::sleep(secs(2)).await;
    env.handle.stop();
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 2, "the drain was cut short");

    // A linger.
    let mut env = Env::new();
    let (_, mut ws) = env.upgraded().await;
    let code = inject(&mut ws, Fault::BadUtf8).await;
    let at = close_of(&mut ws, code, false).await;
    tokio::time::sleep(secs(2)).await;
    env.handle.stop();
    env.handle.stopped().await;
    assert_after(at, Instant::now(), 2, "the linger was cut short");
}

#[tokio::test(start_paused = true)]
async fn h5_stop_during_a_stalled_heartbeat_write_drops_the_connection_at_once() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    // The first tick starts a write that never finishes; the stop comes 2 s into it.
    until(hello_at + secs(12)).await;
    let at = Instant::now();
    env.handle.stop();
    tokio::time::timeout(DEADLINE, env.handle.stopped())
        .await
        .expect("the stop waited for the stalled write");
    // Neither the 10 s write bound nor a Close's own write and drain.
    assert_after(at, Instant::now(), 0, "no waiting");
    assert!(env.fx.locks_are_free(), "both locks are released");
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    // No Close frame was sent: the connection was dropped.
    let (_, next) = got(&mut ws).await;
    assert_eq!(next, Got::Eof);
}

#[tokio::test(start_paused = true)]
async fn h5_a_heartbeat_write_that_completes_as_the_stop_comes_is_followed_by_close_1001() {
    let mut env = Env::new();
    let (hello_at, mut ws) = env.connected(10).await;
    env.dial.flags.stall_write.store(true, Ordering::SeqCst);
    until(hello_at + secs(12)).await;
    // The transport can write again, and the stop is what wakes the session: the write is
    // polled before the stop, so it completes and the stop then sends Close 1001.
    env.dial.flags.stall_write.store(false, Ordering::SeqCst);
    env.handle.stop();
    let (_, first) = got(&mut ws).await;
    assert_eq!(first, Got::Text(r#"{"type":"heartbeat"}"#.to_owned()));
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    assert_eq!(env.status().phase, Phase::Stopped);
}

#[tokio::test(start_paused = true)]
async fn h7_a_stopped_session_shows_no_heartbeat_interval() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    assert_eq!(env.status().heartbeat, Some(secs(10)));
    env.handle.stop();
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    let status = env.status();
    assert_eq!(status.phase, Phase::Stopped);
    assert!(status.heartbeat.is_none());
}

#[tokio::test(start_paused = true)]
async fn h6_a_dropped_handle_or_a_second_stop_is_one_stop() {
    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    env.handle.stop();
    env.handle.stop();
    close_of(&mut ws, 1001, true).await;
    env.handle.stopped().await;
    let (_, next) = got(&mut ws).await;
    assert_eq!(next, Got::Eof, "one close only");

    let mut env = Env::new();
    let (_, mut ws) = env.connected(10).await;
    let mut done = env.handle.done.clone();
    drop(env.handle);
    close_of(&mut ws, 1001, true).await;
    tokio::time::timeout(DEADLINE, done.wait_for(|done| *done))
        .await
        .unwrap()
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// I. Writers, and the delay's rules

#[test]
fn i_the_delay_doubles_to_the_cap_resets_and_a_retry_after_only_lengthens() {
    let mut delay = Backoff::new();
    assert_eq!(delay.delay(), secs(1));
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(delay.delay().as_secs());
        delay.after_wait();
    }
    assert_eq!(seen, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    delay.reset();
    assert_eq!(delay.delay(), backoff::INITIAL);
    delay.cap();
    assert_eq!(delay.delay(), backoff::CAP);
    let mut delay = Backoff::new();
    assert_eq!(delay.wait(Some(30)), secs(30));
    assert_eq!(delay.wait(Some(1)), secs(1));
    delay.after_wait();
    delay.after_wait();
    assert_eq!(delay.wait(Some(3)), secs(4));
    assert_eq!(delay.wait(None), secs(4));
    let base = Instant::now();
    assert!(!backoff::lived(base, base + Duration::from_millis(59_999)));
    assert!(backoff::lived(base, base + secs(60)));
}

#[tokio::test(start_paused = true)]
async fn i_the_delay_resets_only_when_the_connection_lived_from_hello_to_its_last_frame() {
    let mut env = Env::new();
    // Three failures: the delay is 8 afterwards.
    for _ in 0..3 {
        env.fail_challenge(503, &[], b"").await;
    }
    // Lived: the last frame 65 s after hello; the idle close follows 90 s later.
    let (hello_at, mut ws) = env.connected(30).await;
    until(hello_at + secs(65)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    let (closed, code) = close_after_heartbeats(&mut ws, true).await;
    assert_eq!(code, Some(4003));
    assert_after(hello_at, closed, 155, "the idle close");
    let (next, mut stream, _) = env.request().await;
    assert_eq!(gaps(&[closed, next]), vec![1], "reset to 1 s, not 8");
    // This attempt fails with 503: its wait is 2 s, and the delay is 4 afterwards.
    reply(&mut stream, 503, &[], b"").await;
    // The last frame of this connection was at 55 s: it did not live, though it lasted 145 s.
    let (hello_at, mut ws) = env.connected(30).await;
    until(hello_at + secs(55)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    let (closed, _) = close_after_heartbeats(&mut ws, true).await;
    assert_after(hello_at, closed, 145, "the idle close");
    let (next, mut stream, _) = env.request().await;
    assert_eq!(
        gaps(&[closed, next]),
        vec![4],
        "not reset: the delay stays doubled"
    );
    // This attempt fails too: 8 s wait, then 16.
    reply(&mut stream, 503, &[], b"").await;
    // Exactly 60 s lives.
    let (hello_at, mut ws) = env.connected(40).await;
    until(hello_at + secs(60)).await;
    send(&mut ws, r#"{"type":"heartbeat"}"#).await;
    let (closed, _) = close_after_heartbeats(&mut ws, true).await;
    let (next, ..) = env.request().await;
    assert_eq!(gaps(&[closed, next]), vec![1], "60 s lived is enough");
}

// ---------------------------------------------------------------------------------------------
// J. Clocks

#[test]
fn j_the_session_reads_no_wall_clock() {
    for source in [
        include_str!("../control.rs"),
        include_str!("backoff.rs"),
        include_str!("channel.rs"),
        include_str!("dial.rs"),
        include_str!("exchange.rs"),
    ] {
        let code = source
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for wall in ["SystemTime", "std::time::Instant", "chrono"] {
            assert!(!code.contains(wall), "the library reads {wall}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// L. Logging

#[tokio::test(start_paused = true)]
async fn l_one_info_line_at_hello_and_one_at_the_end_and_no_secret_is_logged() {
    let mut env = Env::new();
    let exchanged = env.exchange().await;
    let (_, mut ws, _) = env.upgrade().await;
    send(&mut ws, &hello(10)).await;
    env.wait_phase(Phase::Connected).await;
    send(&mut ws, &job_command("command-id-that-must-not-be-logged")).await;
    env.wait_commands(1).await;
    ws.send(close_frame(Some(1000))).await.unwrap();
    let _ = got(&mut ws).await;
    env.wait_phase(Phase::Waiting).await;
    let info = env.log.lines("INFO");
    assert_eq!(
        info.iter()
            .filter(|l| l.contains("control channel connected"))
            .count(),
        1
    );
    assert_eq!(
        info.iter()
            .filter(|l| l.contains("control channel ended"))
            .count(),
        1
    );
    for secret in exchanged.secrets.iter().map(String::as_str).chain([
        "command-id-that-must-not-be-logged",
        "reason-text-that-must-not-be-logged",
        "c2lnbmVk",
    ]) {
        assert!(
            !env.log.contains(secret),
            "a secret or a body is in the log"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The stack's read errors, mirrored from the server

#[test]
fn a_read_error_is_classified_by_what_the_stack_says() {
    use tungstenite::Error;
    use tungstenite::error::{CapacityError, ProtocolError};
    assert_eq!(
        channel::classify_read_error(&Error::Capacity(CapacityError::MessageTooLong {
            size: 2,
            max_size: 1
        })),
        channel::Action::Linger(1009)
    );
    assert_eq!(
        channel::classify_read_error(&Error::Utf8("bad".to_owned())),
        channel::Action::Linger(1002)
    );
    assert_eq!(
        channel::classify_read_error(&Error::Protocol(ProtocolError::MaskedFrameFromServer)),
        channel::Action::Linger(1002)
    );
    assert_eq!(
        channel::classify_read_error(&Error::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        )),
        channel::Action::Drop
    );
    assert_eq!(
        channel::classify_read_error(&Error::ConnectionClosed),
        channel::Action::Drop
    );
    assert_eq!(
        channel::classify_read_error(&Error::AlreadyClosed),
        channel::Action::Drop
    );
    assert_eq!(
        channel::classify_read_error(&Error::Io(io::Error::other("gone"))),
        channel::Action::Drop
    );
}

// ---------------------------------------------------------------------------------------------
// R1: real loopback, real time

/// A key of the agent's kind, and the record that names it, in a fresh state directory.
fn real_fixture(port: u16) -> (Fixture, crate::identity::AgentKey, Arc<SigningKey>) {
    let signing = Arc::new(SigningKey::generate().unwrap());
    let agent = agent_key_for(&signing);
    let origin = format!("http://127.0.0.1:{port}");
    let fx = Fixture::with_origin(&origin);
    fx.write(&record_for(&agent.public_key(), &origin));
    (fx, agent, signing)
}

async fn real_challenge(stream: &mut tokio::net::TcpStream) {
    let req = read_req(stream).await;
    assert!(req.is_post(CHALLENGE_PATH));
    json_reply(
        stream,
        200,
        &challenge_body(&shared_crypto::random_32().unwrap()),
    )
    .await;
}

async fn real_session(stream: &mut tokio::net::TcpStream) -> String {
    let req = read_req(stream).await;
    assert!(req.is_post(SESSION_PATH));
    let token = token_text();
    json_reply(stream, 200, &token_body(&token)).await;
    token
}

#[tokio::test]
async fn r1_over_real_loopback_an_exchange_an_upgrade_a_heartbeat_and_a_stop() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (fx, agent, _) = real_fixture(port);
    let handle = fx
        .open()
        .unwrap()
        .start(agent, Arc::new(LogIntake))
        .unwrap();
    tokio::time::timeout(secs(30), async {
        let (mut stream, _) = listener.accept().await.unwrap();
        real_challenge(&mut stream).await;
        let (mut stream, _) = listener.accept().await.unwrap();
        let token = real_session(&mut stream).await;
        let (stream, _) = listener.accept().await.unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut ws = accept_ws(stream, seen.clone()).await;
        let bearer = format!("Bearer {token}");
        assert!(seen.lock().unwrap().header("authorization") == Some(bearer.as_str()));
        // A 5 s interval gives a 15 s idle deadline in real time: a stalled CI thread does not
        // trip it.
        ws.send(Message::Text(hello(5).into())).await.unwrap();
        // One heartbeat from the agent after the interval, and one echo from here.
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text(HEARTBEAT.into()));
        ws.send(Message::Text(HEARTBEAT.into())).await.unwrap();
        handle.stop();
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Close(Some(frame)) => {
                    assert_eq!(u16::from(frame.code), 1001);
                    break;
                }
                Message::Text(_) => {}
                other => panic!("unexpected: {other:?}"),
            }
        }
        let _ = ws.flush().await;
        handle.stopped().await;
    })
    .await
    .expect("the smoke test took too long");
    assert!(fx.locks_are_free());
}

#[tokio::test]
async fn r1_over_real_loopback_a_refused_connection_and_then_the_server_appears() {
    // A bound socket that does not listen yet: connects to it are refused, and the port stays
    // ours until `listen`.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket
        .bind((std::net::Ipv4Addr::LOCALHOST, 0).into())
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let (fx, agent, _) = real_fixture(port);
    let handle = fx
        .open()
        .unwrap()
        .start(agent, Arc::new(LogIntake))
        .unwrap();
    tokio::time::timeout(secs(30), async {
        // The first attempt is refused: the loop waits.
        let mut status = handle.status();
        status
            .wait_for(|s| s.phase == Phase::Waiting)
            .await
            .unwrap();
        // The server appears; the retry reaches it.
        let listener = socket.listen(16).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        real_challenge(&mut stream).await;
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = real_session(&mut stream).await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_ws(stream, Arc::new(Mutex::new(Seen::default()))).await;
        ws.send(Message::Text(hello(20).into())).await.unwrap();
        status
            .wait_for(|s| s.phase == Phase::Connected)
            .await
            .unwrap();
        handle.stop();
        handle.stopped().await;
    })
    .await
    .expect("the smoke test took too long");
}

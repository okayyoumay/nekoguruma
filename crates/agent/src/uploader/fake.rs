//! A dial that hands the client one end of an in-memory pipe, and the helpers that play the
//! server's end, so a request runs against an in-process server on a paused clock. The server's
//! end is plain, or TLS under a certificate the test holds the pin of ([`FakeDial::tls`]): the
//! object server's side of the data channel is TLS only (ADR-505).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use ngr_pinned_tls::CertPin;
use ngr_pinned_tls::test_server::{Acceptor, Identity, Key};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

use shared_crypto::Signer as _;

use crate::answer_proof::{TestProver, Verifier, nonce_of};
use crate::control::dial::{Dialled, PortDial, Stream};

/// What the dial does with a call.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Connect,
    Refuse,
    /// The dial never finishes.
    Hang,
}

/// The server's end of a pipe whose every read waits `per_read` and returns at most 128 bytes: a
/// server that takes its input slowly but steadily, below the TLS that runs over it.
struct Throttled {
    inner: DuplexStream,
    per_read: Duration,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl AsyncRead for Throttled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let per_read = self.per_read;
        let wait = self
            .wait
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(per_read)));
        ready!(wait.as_mut().poll(cx));
        let mut bytes = [0u8; 128];
        let mut part = ReadBuf::new(&mut bytes[..buf.remaining().min(128)]);
        ready!(Pin::new(&mut self.inner).poll_read(cx, &mut part))?;
        buf.put_slice(part.filled());
        self.wait = None;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Throttled {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A connection's server end, as a byte stream.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The server's end of a connection the handler plays: the pipe itself, or the TLS stream the
/// handshake made over it.
pub(crate) type ServerEnd = Box<dyn Io>;

type Handler = dyn Fn(u16, ServerEnd) + Send + Sync;

/// The fake dial: each call makes a pipe, gives the server's end to the handler and returns the
/// client's.
pub(crate) struct FakeDial {
    handler: Arc<Handler>,
    /// Set when the server's end is TLS: the handshake runs before the handler gets the stream.
    acceptor: Option<Acceptor>,
    /// Set when the server reads its end of the pipe slowly ([`FakeDial::tls_throttled`]).
    throttle: Option<Duration>,
    pub(crate) mode: Mutex<Mode>,
    /// The ports dialled, in order.
    pub(crate) dialled: Mutex<Vec<u16>>,
    /// The pipe's buffer, in bytes per direction.
    capacity: usize,
}

impl FakeDial {
    /// A dial whose server end is a plain pipe.
    pub(crate) fn new(
        capacity: usize,
        handler: impl Fn(u16, ServerEnd) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            handler: Arc::new(handler),
            acceptor: None,
            throttle: None,
            mode: Mutex::new(Mode::Connect),
            dialled: Mutex::new(Vec::new()),
            capacity,
        })
    }

    /// A dial whose server end is TLS under a new certificate, whose pin is returned: the
    /// handler gets the stream once the handshake is done, and a connection whose handshake
    /// fails is dropped.
    pub(crate) fn tls(
        capacity: usize,
        handler: impl Fn(u16, ServerEnd) + Send + Sync + 'static,
    ) -> (Arc<Self>, CertPin) {
        let identity = Identity::generate(Key::P256);
        let pin = CertPin::of_certificate(identity.certificate());
        let dial = Arc::new(Self {
            handler: Arc::new(handler),
            acceptor: Some(Acceptor::new(&identity)),
            throttle: None,
            mode: Mutex::new(Mode::Connect),
            dialled: Mutex::new(Vec::new()),
            capacity,
        });
        (dial, pin)
    }

    /// [`FakeDial::tls`] whose server reads the bytes of the pipe (handshake included) slowly:
    /// at most 128 bytes per read, each read `per_read` after the last.
    pub(crate) fn tls_throttled(
        capacity: usize,
        per_read: Duration,
        handler: impl Fn(u16, ServerEnd) + Send + Sync + 'static,
    ) -> (Arc<Self>, CertPin) {
        let identity = Identity::generate(Key::P256);
        let pin = CertPin::of_certificate(identity.certificate());
        let dial = Arc::new(Self {
            handler: Arc::new(handler),
            acceptor: Some(Acceptor::new(&identity)),
            throttle: Some(per_read),
            mode: Mutex::new(Mode::Connect),
            dialled: Mutex::new(Vec::new()),
            capacity,
        });
        (dial, pin)
    }

    pub(crate) fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub(crate) fn count(&self) -> usize {
        self.dialled.lock().unwrap().len()
    }
}

impl PortDial for FakeDial {
    fn dial_port(&self, port: u16) -> Dialled<'_> {
        self.dialled.lock().unwrap().push(port);
        let mode = *self.mode.lock().unwrap();
        let (client, server) = tokio::io::duplex(self.capacity);
        if matches!(mode, Mode::Connect) {
            match &self.acceptor {
                None => (self.handler)(port, Box::new(server)),
                Some(acceptor) => {
                    let (acceptor, handler) = (acceptor.clone(), Arc::clone(&self.handler));
                    let throttle = self.throttle;
                    tokio::spawn(async move {
                        let accepted = match throttle {
                            None => acceptor
                                .accept(server)
                                .await
                                .map(|stream| Box::new(stream) as ServerEnd),
                            Some(per_read) => acceptor
                                .accept(Throttled {
                                    inner: server,
                                    per_read,
                                    wait: None,
                                })
                                .await
                                .map(|stream| Box::new(stream) as ServerEnd),
                        };
                        if let Ok(stream) = accepted {
                            handler(port, stream);
                        }
                    });
                }
            }
        }
        Box::pin(async move {
            match mode {
                Mode::Connect => Ok(Box::new(client) as Stream),
                Mode::Refuse => Err(io::ErrorKind::ConnectionRefused.into()),
                Mode::Hang => std::future::pending::<io::Result<Stream>>().await,
            }
        })
    }
}

/// One request as the server read it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    pub(crate) head: String,
    pub(crate) body: Vec<u8>,
}

impl Seen {
    pub(crate) fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or_default()
    }

    /// The value of a header, by lowercase name.
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

/// Reads the head of a request, and its body when the head has a length.
pub(crate) async fn read_request<S: AsyncRead + Unpin>(server: &mut S) -> Option<Seen> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if server.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        raw.push(byte[0]);
    }
    let mut seen = Seen {
        head: String::from_utf8_lossy(&raw).into_owned(),
        body: Vec::new(),
    };
    let length = seen
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    seen.body = vec![0u8; length];
    server.read_exact(&mut seen.body).await.ok()?;
    Some(seen)
}

/// A complete answer with `body` and a declared length.
pub(crate) fn answer(status: u16, extra_headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A JSON answer.
pub(crate) fn json(status: u16, body: &str) -> Vec<u8> {
    answer(status, "Content-Type: application/json\r\n", body)
}

/// What a fake server does with the proof of an answer it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Proof {
    /// A valid proof of the answer to the request as it was made.
    Valid,
    /// No `Ngr-Server-Proof` header.
    Missing,
    /// A proof under another key.
    OtherKey,
    /// A proof for another nonce.
    OtherNonce,
    /// A proof of a body one byte longer than the one sent.
    BodyAltered,
    /// A proof of another status.
    StatusAltered,
    /// A proof of another request's target.
    OtherRequest,
    /// Two proof headers, both valid.
    TwoValues,
    /// A proof whose text is not the canonical base64 of its bytes.
    NotCanonical,
    /// A valid proof of a body over the 64 KiB the client reads.
    OverLimit,
    /// A valid proof of a body of which only part arrives.
    Cut,
    /// A proof made over another method than the request's.
    OtherMethod,
}

impl Proof {
    /// Every way an answer can fail to be proved.
    pub(crate) const UNPROVED: [Proof; 11] = [
        Proof::Missing,
        Proof::OtherKey,
        Proof::OtherNonce,
        Proof::BodyAltered,
        Proof::StatusAltered,
        Proof::OtherRequest,
        Proof::TwoValues,
        Proof::NotCanonical,
        Proof::OverLimit,
        Proof::Cut,
        Proof::OtherMethod,
    ];
}

/// An answer a fake server serves: its proof is made when it is served, from the request that
/// asked.
#[derive(Debug, Clone)]
pub(crate) struct Served {
    pub(crate) status: u16,
    pub(crate) headers: String,
    pub(crate) body: String,
    pub(crate) proof: Proof,
}

impl Served {
    /// A JSON answer with a valid proof.
    pub(crate) fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: "Content-Type: application/json\r\n".to_owned(),
            body: body.to_owned(),
            proof: Proof::Valid,
        }
    }

    /// The same answer with the extra header lines `headers` (each ending in CRLF).
    pub(crate) fn with_headers(mut self, headers: &str) -> Self {
        self.headers.push_str(headers);
        self
    }

    /// The same answer with `proof` instead of a valid one.
    pub(crate) fn with(mut self, proof: Proof) -> Self {
        self.proof = proof;
        self
    }

    /// The same answer with no proof.
    pub(crate) fn unproved(self) -> Self {
        self.with(Proof::Missing)
    }

    /// The bytes of the answer to `request`, proved as `self.proof` says.
    pub(crate) fn bytes(&self, prover: &TestProver, request: &Seen) -> Vec<u8> {
        if self.proof == Proof::Missing {
            // A request with no nonce (the challenge) gets no proof either.
            return answer(self.status, &self.headers, &self.body);
        }
        let mut words = request.request_line().split(' ');
        let (method, target) = (words.next().unwrap(), words.next().unwrap());
        let mut body = self.body.clone();
        if self.proof == Proof::OverLimit {
            // Still JSON, so that only the size is wrong.
            body.push_str(&" ".repeat(70_000));
        }
        let nonce = nonce_of(request.header("ngr-nonce").as_deref(), &request.body);
        let sign = |prover: &TestProver, nonce, target: &str, status, body: &[u8]| {
            prover.proof(nonce, method, target, &request.body, status, body)
        };
        let other_method = if method == "GET" { "POST" } else { "GET" };
        let valid = sign(prover, nonce, target, self.status, body.as_bytes());
        let lines = |proofs: &[String]| {
            proofs
                .iter()
                .map(|p| format!("Ngr-Server-Proof: {p}\r\n"))
                .collect::<String>()
        };
        let proofs: Vec<String> = match self.proof {
            Proof::Valid | Proof::OverLimit | Proof::Cut => vec![valid],
            Proof::Missing => unreachable!("answered above"),
            Proof::OtherKey => {
                let other =
                    prover.with_key(Arc::new(shared_crypto::SigningKey::generate().unwrap()));
                vec![sign(&other, nonce, target, self.status, body.as_bytes())]
            }
            Proof::OtherNonce => {
                let mut other = nonce;
                other[0] ^= 1;
                vec![sign(prover, other, target, self.status, body.as_bytes())]
            }
            Proof::BodyAltered => {
                let longer = format!("{body} ");
                vec![sign(prover, nonce, target, self.status, longer.as_bytes())]
            }
            Proof::StatusAltered => {
                let other = if self.status == 200 { 201 } else { 200 };
                vec![sign(prover, nonce, target, other, body.as_bytes())]
            }
            Proof::OtherRequest => vec![sign(
                prover,
                nonce,
                "/api/v1/agent/other",
                self.status,
                body.as_bytes(),
            )],
            Proof::OtherMethod => vec![prover.proof(
                nonce,
                other_method,
                target,
                &request.body,
                self.status,
                body.as_bytes(),
            )],
            Proof::TwoValues => vec![valid.clone(), valid],
            Proof::NotCanonical => {
                // The last character carries two bits of the signature and four of padding:
                // the same bits with nonzero padding are not the text the bytes encode to.
                let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let mut text = valid.into_bytes();
                let at = text.len() - 3;
                let value = alphabet.iter().position(|c| *c == text[at]).unwrap();
                text[at] = alphabet[(value & 0x30) | 1];
                vec![String::from_utf8(text).unwrap()]
            }
        };
        let head = format!(
            "HTTP/1.1 {} X\r\n{}{}",
            self.status,
            self.headers,
            lines(&proofs)
        );
        let declared = if self.proof == Proof::Cut {
            body.len() + 10
        } else {
            body.len()
        };
        format!("{head}Content-Length: {declared}\r\nConnection: close\r\n\r\n{body}").into_bytes()
    }
}

/// A JSON answer that carries the server's proof of it, for the request `request` (the nonce is
/// the request's `Ngr-Nonce` header, or the session body's).
pub(crate) fn proved_json(prover: &TestProver, request: &Seen, status: u16, body: &str) -> Vec<u8> {
    Served::new(status, body).bytes(prover, request)
}

/// Accepts connections on a real listener until the task is dropped and, for each, runs the TLS
/// handshake when `tls` gives an acceptor for it (a connection whose handshake fails is dropped),
/// reads one request whole and hands it and the connection to `serve`, which answers it.
pub(crate) async fn accept_with<Fut>(
    listener: tokio::net::TcpListener,
    tls: impl Fn() -> Option<Acceptor> + Send + Sync + 'static,
    serve: impl Fn(Seen, ServerEnd) -> Fut + Send + Sync + 'static,
) where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let serve = Arc::new(serve);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let serve = serve.clone();
        let acceptor = tls();
        tokio::spawn(async move {
            let mut stream: ServerEnd = match acceptor {
                None => Box::new(stream),
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(stream) => Box::new(stream),
                    Err(_) => return,
                },
            };
            let Some(seen) = read_request(&mut stream).await else {
                return;
            };
            serve(seen, stream).await;
        });
    }
}

/// The handler of the scripted dials: reads a request whole, answers what `reply` makes of it and
/// records what it read.
fn scripted_handler(
    reply: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static,
    log: Arc<Mutex<Vec<Seen>>>,
) -> impl Fn(u16, ServerEnd) + Send + Sync + 'static {
    let reply = Arc::new(reply);
    move |_, mut server| {
        let reply = reply.clone();
        let log = log.clone();
        tokio::spawn(async move {
            let bytes = match read_request(&mut server).await {
                Some(request) => {
                    let bytes = reply(&request);
                    log.lock().unwrap().push(request);
                    bytes
                }
                None => Vec::new(),
            };
            let _ = server.write_all(&bytes).await;
            let _ = server.shutdown().await;
        });
    }
}

/// A dial whose server answers every request with what `reply` makes of it, after reading it
/// whole, and records what it read.
pub(crate) fn scripted_with(
    reply: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static,
) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dial = FakeDial::new(64 * 1024, scripted_handler(reply, seen.clone()));
    (dial, seen)
}

/// [`scripted_with`] over TLS: also the pin of the server's certificate.
pub(crate) fn scripted_tls_with(
    reply: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static,
) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>, CertPin) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (dial, pin) = FakeDial::tls(64 * 1024, scripted_handler(reply, seen.clone()));
    (dial, seen, pin)
}

/// [`scripted`] over TLS: also the pin of the server's certificate.
pub(crate) fn scripted_tls(reply: Vec<u8>) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>, CertPin) {
    scripted_tls_with(move |_| reply.clone())
}

/// A dial whose server answers every request with `reply` as it is.
pub(crate) fn scripted(reply: Vec<u8>) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>) {
    scripted_with(move |_| reply.clone())
}

/// A dial whose server answers every request with `reply`, proved as it says by `prover`.
pub(crate) fn scripted_served(
    prover: TestProver,
    reply: Served,
) -> (Arc<FakeDial>, Arc<Mutex<Vec<Seen>>>) {
    scripted_with(move |request| reply.bytes(&prover, request))
}

/// A server key's prover for the agent `agent_id` of the server at `origin`, and the verifier
/// an agent that registered with that key holds.
pub(crate) fn proving(origin: &str, agent_id: &str) -> (TestProver, Arc<Verifier>) {
    let key = Arc::new(shared_crypto::SigningKey::generate().unwrap());
    let prover = TestProver::new(key.clone(), origin, agent_id);
    let verifier = Verifier {
        key: key.public_key(),
        audience: prover.audience.clone(),
        agent_id: agent_id.to_owned(),
    };
    (prover, Arc::new(verifier))
}

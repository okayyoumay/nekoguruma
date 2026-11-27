//! The data channel's HTTP/1 client (design 5.4, ADR-493 items 12 and 13). `server_http` is the
//! POST-only client of small answers; this one also GETs, and PUTs a part's body that is larger
//! than anything held in memory.
//!
//! One request is one connection, which the agent dials itself (a loopback literal, no name is
//! resolved) and runs hyper's HTTP/1 client connection over. There is no pool and no redirect
//! handling: a 3xx is an answer like any other. The request carries `Host`, `Connection: close`
//! and the body's length. A [`Client::put`] or [`Client::get`] takes a [`CertPin`] and runs a TLS
//! handshake over the dialled stream before the request (ADR-505): the server is accepted only
//! with that certificate, and a refused handshake is [`Error::Tls`] with nothing of the request
//! sent. The JSON routes are the server's own and are plain; no object is sent or read without
//! TLS. The handshake sits above the byte counting and below the request, so the no-progress
//! clock counts the bytes on the socket. Time is judged as design 5.4 asks, by progress:
//!
//! - the connect, and the handshake when there is one, within [`CONNECT_TIMEOUT`];
//! - at most [`NO_PROGRESS`] with no byte moved in either direction, counted from the dial, for
//!   every request ([`Error::NoProgress`]);
//! - [`JSON_TOTAL`] in all for the JSON routes, and no total for a part's PUT.
//!
//! A head is at most [`MAX_HEAD`] bytes. A body is read only for the statuses the request's
//! [`JsonRequest::reads`] names ([`reads_body`] for the upload routes: the statuses that carry
//! their answers) and is at most [`MAX_ANSWER`] bytes; the body of any other status is never read,
//! and a PUT's answer body is never read at all: its status says whether the part was stored.
//! Every JSON request carries a fresh `Ngr-Nonce` and its answer's `Ngr-Server-Proof` is kept
//! beside the body, for the caller to verify (ADR-504); the PUT and the GET carry neither.
//!
//! [`Client::get`] is the download leg's request (design 14.3, ADR-497): a presigned GET with no
//! `Authorization` and no redirect, whose body is streamed into a writer the caller gives, on a
//! blocking thread, so memory does not depend on the object's size. The body is read only for a
//! 200 that declares exactly one canonical `Content-Length` within the bound the caller gives;
//! it has no total time, only the no-progress clock, which a completed write of the writer
//! restarts as a byte on the socket does.

use std::io::{self, Read, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::client::conn::http1;
use hyper::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, HeaderMap, HeaderName, RETRY_AFTER,
    TRANSFER_ENCODING,
};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use ngr_pinned_tls::{CertPin, Refused};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::control::dial::{LoopbackPorts, PortDial, Stream};
use shared_proto::{NONCE_HEADER, PROOF_HEADER};

use crate::server_http::{CONNECT_TIMEOUT, MAX_HEAD, MAX_HEADERS, single_text};

/// How long a connection may go with no byte read or written, in either direction.
pub const NO_PROGRESS: Duration = Duration::from_secs(30);
/// How long a JSON route's request may take in all.
pub const JSON_TOTAL: Duration = Duration::from_secs(60);
/// The largest answer body read, in bytes.
pub const MAX_ANSWER: usize = 64 * 1024;
/// The size of one chunk of a PUT's body.
pub const CHUNK: usize = 64 * 1024;
/// The chunks that wait between the file's reader and the socket.
const CHUNKS_IN_FLIGHT: usize = 2;

/// Whether a body is read for this status: the statuses the four routes answer with a document.
pub fn reads_body(status: u16) -> bool {
    matches!(status, 200 | 201 | 202 | 409)
}

/// Why no answer came back.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The connection was refused or the address is unreachable: nothing was sent. The source is
    /// the dial's error.
    #[error("the connection failed; nothing was sent")]
    NotSent(#[source] io::Error),
    /// The connect, or the TLS handshake after it, did not finish in time: nothing was sent.
    #[error("the connect timed out; nothing was sent")]
    ConnectTimeout,
    /// The TLS handshake did not accept the server: nothing of the request was sent.
    #[error("the TLS handshake did not accept the server; nothing of the request was sent")]
    Tls(#[source] Refused),
    /// No byte moved for [`NO_PROGRESS`].
    #[error("no byte moved in time")]
    NoProgress,
    /// A JSON route did not finish within [`JSON_TOTAL`].
    #[error("the request did not finish in time")]
    Timeout,
    /// The writer a GET streams into failed (the disk is full, the file cannot be written).
    #[error("the writer failed: {0:?}")]
    Write(io::ErrorKind),
    /// The request may have been sent and no complete answer came: a reset, a head over the
    /// limits, an answer that is not HTTP, a body that is cut, or a request that could not be
    /// built. The source is the failure the HTTP client reported, when there was one.
    #[error("no complete answer came; the request may have been sent")]
    Failed(#[source] Option<Box<dyn std::error::Error + Send + Sync>>),
}

impl Error {
    /// [`Error::Failed`] caused by `source`.
    fn failed(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Error::Failed(Some(Box::new(source)))
    }
}

/// The body of an answer.
#[derive(Debug, PartialEq, Eq)]
pub enum AnswerBody {
    /// The status carries no document the caller reads; nothing was read.
    Unread,
    Bytes(Vec<u8>),
    /// More than [`MAX_ANSWER`] bytes, declared or arrived. The bytes are not kept.
    TooLarge,
    /// The body ended before its declared end, or failed while it was read.
    Cut,
}

/// What came back. `Debug` shows the status and the body's length, never the body.
pub struct Answer {
    pub status: u16,
    /// The single `Retry-After` value, when the answer has exactly one that is text.
    pub retry_after: Option<String>,
    /// The single `Ngr-Server-Proof` value, when the answer has exactly one that is text.
    pub proof: Option<String>,
    pub body: AnswerBody,
}

/// What a GET came to.
#[derive(Debug)]
pub struct Got<W> {
    pub status: u16,
    /// The single `Retry-After` value, when the answer has exactly one that is text.
    pub retry_after: Option<String>,
    pub body: GetBody<W>,
}

/// The body of a GET's answer.
#[derive(Debug)]
pub enum GetBody<W> {
    /// The status is not 200: no body byte was read.
    Unread,
    /// A 200 without exactly one canonical `Content-Length`, or one over the bound (or with a
    /// `Transfer-Encoding`): no body byte was read.
    BadLength,
    /// The declared number of bytes was written to the writer, which is handed back.
    Complete { length: u64, writer: W },
}

/// A request to one of the four JSON routes. `Debug` leaves out the token and shows only the
/// body's length, as `server_http::PostRequest` does.
pub struct JsonRequest<'a> {
    pub method: Method,
    /// The request target: the path.
    pub target: &'a str,
    /// The `Host` header's value.
    pub host: &'a str,
    /// The session token, sent as the bearer. Only these routes carry one.
    pub bearer: &'a str,
    /// The JSON body of a POST with one.
    pub body: Option<Vec<u8>>,
    /// The request's nonce, sent in the `Ngr-Nonce` header (ADR-504).
    pub nonce: [u8; 32],
    /// Whether the body of an answer with this status is read.
    pub reads: fn(u16) -> bool,
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let body = match &self.body {
            AnswerBody::Unread => "unread".to_owned(),
            AnswerBody::Bytes(bytes) => format!("{} bytes", bytes.len()),
            AnswerBody::TooLarge => "too large".to_owned(),
            AnswerBody::Cut => "cut".to_owned(),
        };
        f.debug_struct("Answer")
            .field("status", &self.status)
            .field("retry_after", &self.retry_after.is_some())
            .field("body", &body)
            .finish()
    }
}

impl std::fmt::Debug for JsonRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRequest")
            .field("method", &self.method)
            .field("target", &self.target)
            .field("host", &self.host)
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// The data channel's client: a dial and nothing else. It holds no connection.
#[derive(Clone)]
pub struct Client {
    dial: Arc<dyn PortDial>,
    no_progress: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

impl Client {
    /// The production client: TCP to the loopback address, at the port each call names.
    pub fn loopback() -> Self {
        Self {
            dial: Arc::new(LoopbackPorts),
            no_progress: NO_PROGRESS,
        }
    }

    /// A client over another dial (the tests').
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "tests replace the dial; the loop uses `loopback`")
    )]
    pub(crate) fn with_dial(dial: Arc<dyn PortDial>) -> Self {
        Self {
            dial,
            no_progress: NO_PROGRESS,
        }
    }

    /// The same client with another no-progress limit, so that a test of the write side need not
    /// wait 30 real seconds.
    #[cfg(test)]
    pub(crate) fn with_no_progress(mut self, limit: Duration) -> Self {
        self.no_progress = limit;
        self
    }

    /// One JSON route's request, at `port`, within [`JSON_TOTAL`].
    pub async fn json(&self, port: u16, request: &JsonRequest<'_>) -> Result<Answer, Error> {
        let body = request.body.clone().unwrap_or_default();
        let mut builder = Request::builder()
            .method(request.method.clone())
            .uri(request.target)
            .header(HOST, request.host)
            .header(CONNECTION, "close")
            .header(AUTHORIZATION, format!("Bearer {}", request.bearer))
            .header(
                HeaderName::from_static(NONCE_HEADER),
                STANDARD.encode(request.nonce),
            );
        if request.body.is_some() {
            builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        if request.method == Method::POST {
            // Declared even when empty: the part URL and complete routes send no body.
            builder = builder.header(CONTENT_LENGTH, body.len());
        }
        self.send(
            port,
            None,
            || {
                builder
                    .body(Full::new(Bytes::from(body)))
                    .map_err(Error::failed)
            },
            Some(JSON_TOTAL),
            request.reads,
        )
        .await
    }

    /// PUTs `length` bytes read from `source` to `target` at `port`, with no total time: the
    /// connection is judged by progress alone. The connection is TLS to the server with the
    /// certificate `pin` names, and nothing of the request is sent unless the server presents
    /// it. No `Authorization` is sent (the URL is the bearer). The body is read in [`CHUNK`]s on a blocking thread, so memory does not depend
    /// on `length`. A `source` that ends before `length` bytes, or fails, fails the request.
    pub async fn put(
        &self,
        port: u16,
        pin: &CertPin,
        target: &str,
        host: &str,
        length: u64,
        source: Box<dyn Read + Send>,
    ) -> Result<Answer, Error> {
        let builder = Request::builder()
            .method(Method::PUT)
            .uri(target)
            .header(HOST, host)
            .header(CONNECTION, "close")
            // Always declared, so that an empty part still carries `Content-Length: 0`.
            .header(CONTENT_LENGTH, length);
        // The reader starts only once the connection is up, and ends when the body is dropped
        // (with this future) and the channel closes.
        self.send(
            port,
            Some(pin),
            move || {
                builder
                    .body(ChannelBody::start(length, source))
                    .map_err(Error::failed)
            },
            None,
            |_| false,
        )
        .await
    }

    /// GETs `target` at `port` and streams the body into `writer`: no `Authorization` is sent and
    /// a redirect is an answer like any other. The body is read only for a 200 that declares
    /// exactly one canonical `Content-Length` of at most `max_len`; any other answer is returned
    /// with no body byte read. A body that is cut short, or whose reader sees more than the
    /// declared length, fails the request ([`Error::Failed`]); a writer that fails is
    /// [`Error::Write`]. There is no total time: the connection is judged by progress alone. The
    /// connection is TLS with the certificate `pin` names, as for [`Client::put`].
    pub async fn get<W: Write + Send + 'static>(
        &self,
        port: u16,
        pin: &CertPin,
        target: &str,
        host: &str,
        max_len: u64,
        writer: W,
    ) -> Result<Got<W>, Error> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(target)
            .header(HOST, host)
            .header(CONNECTION, "close")
            .body(Empty::<Bytes>::new())
            .map_err(Error::failed)?;
        let start = Instant::now();
        let progress = Progress::new(start);
        let stream = self.connect(port, Some(pin), start, &progress).await?;
        tokio::select! {
            got = exchange_get(stream, request, max_len, writer, progress.clone()) => got,
            () = progress.stalled(self.no_progress) => Err(Error::NoProgress),
        }
    }

    /// Dials `port` and, given a pin, runs the TLS handshake over the dialled stream, all within
    /// [`CONNECT_TIMEOUT`] of `start`.
    async fn connect(
        &self,
        port: u16,
        pin: Option<&CertPin>,
        start: Instant,
        progress: &Progress,
    ) -> Result<Wire, Error> {
        let connecting = async {
            let stream = self.dial.dial_port(port).await.map_err(Error::NotSent)?;
            let stream = Tracked {
                inner: stream,
                progress: progress.clone(),
            };
            match pin {
                None => Ok(Wire::Plain(stream)),
                Some(pin) => ngr_pinned_tls::connect(pin, stream)
                    .await
                    .map(|tls| Wire::Tls(Box::new(tls)))
                    .map_err(Error::Tls),
            }
        };
        tokio::time::timeout_at(start + CONNECT_TIMEOUT, connecting)
            .await
            .unwrap_or(Err(Error::ConnectTimeout))
    }

    async fn send<B>(
        &self,
        port: u16,
        pin: Option<&CertPin>,
        make_request: impl FnOnce() -> Result<Request<B>, Error>,
        total: Option<Duration>,
        reads: fn(u16) -> bool,
    ) -> Result<Answer, Error>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let start = Instant::now();
        let progress = Progress::new(start);
        let stream = self.connect(port, pin, start, &progress).await?;
        let request = make_request()?;
        let judged = async {
            tokio::select! {
                answer = exchange(stream, request, reads) => answer,
                () = progress.stalled(self.no_progress) => Err(Error::NoProgress),
            }
        };
        match total {
            Some(total) => tokio::time::timeout_at(start + total, judged)
                .await
                .unwrap_or(Err(Error::Timeout)),
            None => judged.await,
        }
    }
}

/// The connection a request runs over.
enum Wire {
    Plain(Tracked),
    /// TLS over the byte-counting stream, so the no-progress clock counts the socket's bytes.
    Tls(Box<ngr_pinned_tls::Stream<Tracked>>),
}

impl AsyncRead for Wire {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// The instant of the last byte moved on a connection, in either direction.
#[derive(Clone)]
struct Progress(Arc<Mutex<Instant>>);

impl Progress {
    fn new(start: Instant) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    fn moved(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Completes when [`NO_PROGRESS`] has passed since the last byte.
    async fn stalled(&self, limit: Duration) {
        loop {
            let due = *self.0.lock().unwrap_or_else(|e| e.into_inner()) + limit;
            if Instant::now() >= due {
                return;
            }
            tokio::time::sleep_until(due).await;
        }
    }
}

/// The stream, recording the instant of every byte read or written.
struct Tracked {
    inner: Stream,
    progress: Progress,
}

impl AsyncRead for Tracked {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.progress.moved();
        }
        polled
    }
}

impl AsyncWrite for Tracked {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(polled, Poll::Ready(Ok(n)) if n > 0) {
            self.progress.moved();
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Aborts the connection's task when the request ends, however it ends.
struct Driver(JoinHandle<()>);

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn exchange<B>(
    stream: Wire,
    request: Request<B>,
    reads: fn(u16) -> bool,
) -> Result<Answer, Error>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(MAX_HEAD)
        .max_headers(MAX_HEADERS)
        .handshake::<_, B>(TokioIo::new(stream))
        .await
        .map_err(Error::failed)?;
    let _driver = Driver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    // The answer to a PUT is its status alone: a stored part is not undone by a body that is
    // late or cut.
    let is_put = request.method() == Method::PUT;
    let response = sender.send_request(request).await.map_err(Error::failed)?;
    let status = response.status().as_u16();
    let retry_after = single_text(response.headers(), &RETRY_AFTER);
    let proof = single_text(response.headers(), &HeaderName::from_static(PROOF_HEADER));
    let body = if !is_put && reads(status) {
        read_body(response).await?
    } else {
        AnswerBody::Unread
    };
    Ok(Answer {
        status,
        retry_after,
        proof,
        body,
    })
}

async fn exchange_get<W: Write + Send + 'static>(
    stream: Wire,
    request: Request<Empty<Bytes>>,
    max_len: u64,
    writer: W,
    progress: Progress,
) -> Result<Got<W>, Error> {
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(MAX_HEAD)
        .max_headers(MAX_HEADERS)
        .handshake::<_, Empty<Bytes>>(TokioIo::new(stream))
        .await
        .map_err(Error::failed)?;
    let _driver = Driver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let response = sender.send_request(request).await.map_err(Error::failed)?;
    let status = response.status().as_u16();
    let retry_after = single_text(response.headers(), &RETRY_AFTER);
    let answer = |body| Got {
        status,
        retry_after: retry_after.clone(),
        body,
    };
    if status != 200 {
        return Ok(answer(GetBody::Unread));
    }
    let Some(length) = declared_length(response.headers(), max_len) else {
        return Ok(answer(GetBody::BadLength));
    };
    let mut body = response.into_body();
    // The writer runs on a blocking thread and ends when the channel closes, whether the body
    // ended, failed or this future was dropped.
    let (tx, mut rx) = mpsc::channel::<Bytes>(CHUNKS_IN_FLIGHT);
    let sink = tokio::task::spawn_blocking(move || {
        let mut writer = writer;
        while let Some(chunk) = rx.blocking_recv() {
            writer.write_all(&chunk)?;
            // A write that completes is progress too: a slow disk that keeps taking bytes
            // must not read as a stalled connection while the socket waits for it.
            progress.moved();
        }
        writer.flush()?;
        Ok::<W, io::Error>(writer)
    });
    let mut received = 0u64;
    let mut failed = false;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            failed = true;
            break;
        };
        if let Ok(data) = frame.into_data() {
            received += data.len() as u64;
            if received > length {
                failed = true;
                break;
            }
            if tx.send(data).await.is_err() {
                // The writer failed; its result says why.
                break;
            }
        }
    }
    drop(tx);
    match sink.await {
        Ok(Ok(writer)) if !failed && received == length => {
            Ok(answer(GetBody::Complete { length, writer }))
        }
        Ok(Ok(_)) => Err(Error::Failed(None)),
        Ok(Err(error)) => Err(Error::Write(error.kind())),
        Err(_) => Err(Error::Write(io::ErrorKind::Other)),
    }
}

/// The declared body length of a 200: exactly one `Content-Length`, in canonical decimal (digits
/// only, no sign, no leading zero), at most `max_len`, and no `Transfer-Encoding` beside it.
fn declared_length(headers: &HeaderMap, max_len: u64) -> Option<u64> {
    if headers.contains_key(TRANSFER_ENCODING) {
        return None;
    }
    let mut values = headers.get_all(CONTENT_LENGTH).iter();
    let text = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let canonical = !text.is_empty()
        && text.len() <= 20
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    let length = text.parse::<u64>().ok().filter(|_| canonical)?;
    (length <= max_len).then_some(length)
}

async fn read_body(response: hyper::Response<hyper::body::Incoming>) -> Result<AnswerBody, Error> {
    let declared =
        single_text(response.headers(), &CONTENT_LENGTH).and_then(|text| text.parse::<u64>().ok());
    if declared.is_some_and(|len| len > MAX_ANSWER as u64) {
        return Ok(AnswerBody::TooLarge);
    }
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return Ok(AnswerBody::Cut);
        };
        if let Ok(data) = frame.into_data() {
            if bytes.len() + data.len() > MAX_ANSWER {
                return Ok(AnswerBody::TooLarge);
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(AnswerBody::Bytes(bytes))
}

/// The PUT's body: chunks a blocking reader sends through a bounded channel, so that at most
/// [`CHUNKS_IN_FLIGHT`] chunks are held whatever the length.
struct ChannelBody {
    chunks: mpsc::Receiver<io::Result<Bytes>>,
    /// The bytes still to be handed to the connection.
    remaining: u64,
}

impl ChannelBody {
    fn start(length: u64, mut source: Box<dyn Read + Send>) -> Self {
        let (tx, chunks) = mpsc::channel(CHUNKS_IN_FLIGHT);
        tokio::task::spawn_blocking(move || {
            let mut left = length;
            while left > 0 {
                let want = usize::try_from(left).map_or(CHUNK, |l| l.min(CHUNK));
                let mut chunk = vec![0u8; want];
                let mut filled = 0;
                while filled < want {
                    match source.read(&mut chunk[filled..]) {
                        Ok(0) => {
                            // Shorter than declared: the request fails, not a short object.
                            let _ = tx.blocking_send(Err(io::ErrorKind::UnexpectedEof.into()));
                            return;
                        }
                        Ok(n) => filled += n,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            let _ = tx.blocking_send(Err(error));
                            return;
                        }
                    }
                }
                left -= want as u64;
                if tx.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                    return;
                }
            }
        });
        Self {
            chunks,
            remaining: length,
        }
    }
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        match self.chunks.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                self.remaining = self.remaining.saturating_sub(chunk.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into()))),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests;

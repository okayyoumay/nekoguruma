//! The data channel's HTTP/1 client (design 5.4, ADR-493 items 12 and 13). `server_http` is the
//! POST-only client of small answers; this one also GETs, and PUTs a part's body that is larger
//! than anything held in memory.
//!
//! One request is one connection, which the agent dials itself (a loopback literal, no name is
//! resolved) and runs hyper's HTTP/1 client connection over. There is no pool and no redirect
//! handling: a 3xx is an answer like any other. The request carries `Host`, `Connection: close`
//! and the body's length. Time is judged as design 5.4 asks, by progress:
//!
//! - the connect within [`CONNECT_TIMEOUT`];
//! - at most [`NO_PROGRESS`] with no byte moved in either direction, counted from the dial, for
//!   every request ([`Error::NoProgress`]);
//! - [`JSON_TOTAL`] in all for the JSON routes, and no total for a part's PUT.
//!
//! A head is at most [`MAX_HEAD`] bytes. A body is read only for the statuses that carry the
//! routes' answers ([`reads_body`]) and is at most [`MAX_ANSWER`] bytes; the body of any other
//! status is never read.

use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::client::conn::http1;
use hyper::header::{AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, RETRY_AFTER};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::control::dial::{LoopbackPorts, PortDial, Stream};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The connection was refused or the address is unreachable: nothing was sent.
    NotSent,
    /// The connect did not finish in time: nothing was sent.
    ConnectTimeout,
    /// No byte moved for [`NO_PROGRESS`].
    NoProgress,
    /// A JSON route did not finish within [`JSON_TOTAL`].
    Timeout,
    /// The request may have been sent and no complete answer came: a reset, a head over the
    /// limits, an answer that is not HTTP, a body that is cut, or a request that could not be
    /// built.
    Failed,
}

/// The body of an answer.
#[derive(Debug, PartialEq, Eq)]
pub enum AnswerBody {
    /// The status carries no document the caller reads; nothing was read.
    Unread,
    Bytes(Vec<u8>),
    /// More than [`MAX_ANSWER`] bytes, declared or arrived. The bytes are not kept.
    TooLarge,
}

/// What came back.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    /// The single `Retry-After` value, when the answer has exactly one that is text.
    pub retry_after: Option<String>,
    pub body: AnswerBody,
}

/// A request to one of the four JSON routes.
#[derive(Debug)]
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
}

/// The data channel's client: a dial and nothing else. It holds no connection.
#[derive(Clone)]
pub struct Client {
    dial: Arc<dyn PortDial>,
}

impl Client {
    /// The production client: TCP to the loopback address, at the port each call names.
    pub fn loopback() -> Self {
        Self {
            dial: Arc::new(LoopbackPorts),
        }
    }

    /// A client over another dial (the tests').
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "tests replace the dial; the loop uses `loopback`")
    )]
    pub(crate) fn with_dial(dial: Arc<dyn PortDial>) -> Self {
        Self { dial }
    }

    /// One JSON route's request, at `port`, within [`JSON_TOTAL`].
    pub async fn json(&self, port: u16, request: &JsonRequest<'_>) -> Result<Answer, Error> {
        let body = request.body.clone().unwrap_or_default();
        let mut builder = Request::builder()
            .method(request.method.clone())
            .uri(request.target)
            .header(HOST, request.host)
            .header(CONNECTION, "close")
            .header(AUTHORIZATION, format!("Bearer {}", request.bearer));
        if request.body.is_some() {
            builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
        }
        if request.method == Method::POST {
            // Declared even when empty: the part URL and complete routes send no body.
            builder = builder.header(CONTENT_LENGTH, body.len());
        }
        let http_request = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| Error::Failed)?;
        self.send(port, http_request, Some(JSON_TOTAL)).await
    }

    /// PUTs `length` bytes read from `source` to `target` at `port`, with no total time: the
    /// connection is judged by progress alone. No `Authorization` is sent (the URL is the
    /// bearer). The body is read in [`CHUNK`]s on a blocking thread, so memory does not depend
    /// on `length`. A `source` that ends before `length` bytes, or fails, fails the request.
    pub async fn put(
        &self,
        port: u16,
        target: &str,
        host: &str,
        length: u64,
        source: Box<dyn Read + Send>,
    ) -> Result<Answer, Error> {
        // The reader ends by itself when the body is dropped and the channel closes.
        let body = ChannelBody::start(length, source);
        let request = Request::builder()
            .method(Method::PUT)
            .uri(target)
            .header(HOST, host)
            .header(CONNECTION, "close")
            // Always declared, so that an empty part still carries `Content-Length: 0`.
            .header(CONTENT_LENGTH, length)
            .body(body)
            .map_err(|_| Error::Failed)?;
        self.send(port, request, None).await
    }

    async fn send<B>(
        &self,
        port: u16,
        request: Request<B>,
        total: Option<Duration>,
    ) -> Result<Answer, Error>
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let start = Instant::now();
        let progress = Progress::new(start);
        let stream =
            match tokio::time::timeout_at(start + CONNECT_TIMEOUT, self.dial.dial_port(port)).await
            {
                Err(_) => return Err(Error::ConnectTimeout),
                Ok(Err(_)) => return Err(Error::NotSent),
                Ok(Ok(stream)) => stream,
            };
        let stream = Tracked {
            inner: stream,
            progress: progress.clone(),
        };
        let judged = async {
            tokio::select! {
                answer = exchange(stream, request) => answer,
                () = progress.stalled() => Err(Error::NoProgress),
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
    async fn stalled(&self) {
        loop {
            let due = *self.0.lock().unwrap_or_else(|e| e.into_inner()) + NO_PROGRESS;
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
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, data);
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

async fn exchange<B>(stream: Tracked, request: Request<B>) -> Result<Answer, Error>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(MAX_HEAD)
        .max_headers(MAX_HEADERS)
        .handshake::<_, B>(TokioIo::new(stream))
        .await
        .map_err(|_| Error::Failed)?;
    let _driver = Driver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| Error::Failed)?;
    let status = response.status().as_u16();
    let retry_after = single_text(response.headers(), &RETRY_AFTER);
    let body = if reads_body(status) {
        read_body(response).await?
    } else {
        AnswerBody::Unread
    };
    Ok(Answer {
        status,
        retry_after,
        body,
    })
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
        let frame = frame.map_err(|_| Error::Failed)?;
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

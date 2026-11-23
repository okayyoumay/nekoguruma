//! The object store's HTTP client (ADR-492): one HTTP/1 request on one connection to the object
//! server on `127.0.0.1`, no pool, no redirects, no retry, no HTTP/2. The connection is closed
//! after its response (the request says `connection: close`), and the task that drives it ends
//! with the response.
//!
//! The connection is TLS, accepted only when the peer presents the certificate the client's pin
//! names (ADR-505): nothing, the request line and its signed URL included, is written before the
//! handshake accepted the peer. The object server may close the connection without a TLS
//! close_notify after a complete response; a body that is complete by its length is then read in
//! full, and one that is cut short, or that has no length and ends in such a close, is an error,
//! never a short success.
//!
//! Nothing here formats a URL, a header or a body: a presigned URL carries its signature in its
//! query, and the object server's messages may repeat keys and upload IDs. Errors are an
//! operation name and an [`io::ErrorKind`].

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::{Body, Bytes, Incoming};
use hyper::client::conn::http1;
use hyper::header::{CONNECTION, CONTENT_LENGTH, HOST};
use hyper::{HeaderMap, Method, Request};
use hyper_util::rt::TokioIo;
use ngr_pinned_tls::CertPin;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use url::Url;

use crate::storage::StoreError;

/// Ends the task that drives a connection when dropped, so that a response dropped half read
/// closes its connection.
struct EndOnDrop(JoinHandle<()>);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The client of one object server: its address and the pin of the certificate it must present.
pub(super) struct Client {
    addr: SocketAddr,
    pin: CertPin,
}

impl Client {
    pub(super) fn new(port: u16, pin: CertPin) -> Client {
        Client {
            addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            pin,
        }
    }

    /// Sends one request and returns once the response head has arrived. The connect, the
    /// request and the head are not bounded here: the caller bounds them.
    ///
    /// `url` is the presigned URL; only its path and query are sent, to the loopback address the
    /// client was made for, with the host the signature covers. A request with a body (`PUT`,
    /// `POST`) always carries its `content-length`, `0` included.
    pub(super) async fn send(
        &self,
        op: &'static str,
        method: Method,
        url: &Url,
        body: Option<Vec<u8>>,
    ) -> Result<Response, StoreError> {
        let io_error = |error: io::Error| StoreError::Io {
            op,
            kind: error.kind(),
        };
        let mut target = url.path().to_owned();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_owned(),
        };
        let has_body = matches!(method, Method::PUT | Method::POST);
        let body = Bytes::from(body.unwrap_or_default());
        let mut request = Request::builder()
            .method(method)
            .uri(target)
            .header(HOST, host)
            .header(CONNECTION, "close");
        if has_body {
            request = request.header(CONTENT_LENGTH, body.len());
        }
        let request =
            request
                .body(http_body_util::Full::new(body))
                .map_err(|_| StoreError::Io {
                    op,
                    kind: io::ErrorKind::InvalidInput,
                })?;
        let stream = TcpStream::connect(self.addr).await.map_err(io_error)?;
        let _ = stream.set_nodelay(true);
        let stream = ngr_pinned_tls::connect(&self.pin, stream)
            .await
            .map_err(|refused| StoreError::Untrusted { op, refused })?;
        let (mut sender, connection) =
            http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|error| StoreError::Io {
                    op,
                    kind: hyper_kind(&error),
                })?;
        let driver = EndOnDrop(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| StoreError::Io {
                op,
                kind: hyper_kind(&error),
            })?;
        let (head, body) = response.into_parts();
        Ok(Response {
            op,
            status: head.status.as_u16(),
            headers: head.headers,
            body,
            _driver: driver,
        })
    }
}

/// The kind of I/O error behind a hyper error, else `TimedOut` for a timeout, `UnexpectedEof` for
/// a response cut short and `Other`.
fn hyper_kind(error: &hyper::Error) -> io::ErrorKind {
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if let Some(io_error) = inner.downcast_ref::<io::Error>() {
            return io_error.kind();
        }
        source = inner.source();
    }
    if error.is_timeout() {
        io::ErrorKind::TimedOut
    } else if error.is_incomplete_message() {
        io::ErrorKind::UnexpectedEof
    } else {
        io::ErrorKind::Other
    }
}

/// A response whose head has arrived.
pub(super) struct Response {
    op: &'static str,
    pub(super) status: u16,
    pub(super) headers: HeaderMap,
    body: Incoming,
    _driver: EndOnDrop,
}

impl Response {
    /// The body's declared length, if the head has a well-formed `content-length`.
    pub(super) fn content_length(&self) -> Option<u64> {
        self.headers
            .get(CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse::<u64>()
            .ok()
    }

    /// Reads the body up to `limit` bytes. The flag says that more followed, which was not read;
    /// the connection is closed then.
    pub(super) async fn collect(mut self, limit: usize) -> Result<(Vec<u8>, bool), StoreError> {
        let mut data = Vec::new();
        while let Some(frame) = self.body.frame().await {
            let frame = frame.map_err(|error| StoreError::Io {
                op: self.op,
                kind: hyper_kind(&error),
            })?;
            if let Ok(chunk) = frame.into_data() {
                if data.len() + chunk.len() > limit {
                    data.extend_from_slice(&chunk[..limit - data.len()]);
                    return Ok((data, true));
                }
                data.extend_from_slice(&chunk);
            }
        }
        Ok((data, false))
    }

    /// The body as a reader whose every wait for the next bytes is bounded by `idle` and has no
    /// total bound: a body may take as long as it makes progress.
    pub(super) fn into_reader(self, idle: Duration) -> BodyReader {
        BodyReader {
            body: self.body,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
            pending: Bytes::new(),
            done: false,
            _driver: self._driver,
        }
    }
}

/// A response body read as bytes, with an idle timeout.
pub(super) struct BodyReader {
    body: Incoming,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
    pending: Bytes,
    done: bool,
    _driver: EndOnDrop,
}

impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if !this.pending.is_empty() {
                let n = this.pending.len().min(buf.remaining());
                buf.put_slice(&this.pending.split_to(n));
                return Poll::Ready(Ok(()));
            }
            if this.done {
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.pending = data;
                        let next = tokio::time::Instant::now() + this.idle;
                        this.sleep.as_mut().reset(next);
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(io::Error::new(hyper_kind(&error), "read failed")));
                }
                Poll::Ready(None) => this.done = true,
                Poll::Pending => {
                    return if this.sleep.as_mut().poll(cx).is_ready() {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the object server stopped sending",
                        )))
                    } else {
                        Poll::Pending
                    };
                }
            }
        }
    }
}

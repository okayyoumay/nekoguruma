//! The seam that carries all socket I/O of the control channel session (ADR-471).
//!
//! Production opens a TCP stream to `127.0.0.1:<port>` and nothing else: the address is a
//! literal, so no name is resolved (ADR-456, ADR-470). Tests hand the loop one end of an
//! in-memory duplex pipe instead, so the whole session runs against an in-process fake server on
//! a paused clock.

use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::Instant;

/// A byte stream the session runs HTTP or WebSocket over.
pub(super) trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(super) type Stream = Box<dyn AsyncStream>;

pub(super) type Dialled<'a> = Pin<Box<dyn Future<Output = io::Result<Stream>> + Send + 'a>>;

/// Opens one connection to the server.
pub(super) trait Dial: Send + Sync {
    fn dial(&self) -> Dialled<'_>;
}

/// The production dial: a TCP stream to the loopback address and the record's port.
pub(super) struct Loopback {
    pub(super) port: u16,
}

impl Dial for Loopback {
    fn dial(&self) -> Dialled<'_> {
        Box::pin(async move {
            let stream =
                TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))).await?;
            // A heartbeat is a few bytes; do not hold it back. A failure here changes nothing.
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream) as Stream)
        })
    }
}

/// Why no connection came.
pub(super) enum ConnectError {
    /// The connection was refused or the address is unreachable.
    Failed,
    /// It did not finish by the connect bound.
    Timeout,
}

/// Dials, giving up at `by`.
pub(super) async fn connect(dial: &dyn Dial, by: Instant) -> Result<Stream, ConnectError> {
    match tokio::time::timeout_at(by, dial.dial()).await {
        Err(_) => Err(ConnectError::Timeout),
        Ok(Err(_)) => Err(ConnectError::Failed),
        Ok(Ok(stream)) => Ok(stream),
    }
}

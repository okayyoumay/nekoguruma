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
pub(crate) trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(crate) type Stream = Box<dyn AsyncStream>;

pub(crate) type Dialled<'a> = Pin<Box<dyn Future<Output = io::Result<Stream>> + Send + 'a>>;

/// Opens one connection to the server.
pub(crate) trait Dial: Send + Sync {
    fn dial(&self) -> Dialled<'_>;
}

/// Opens one connection to the given port of the server's host. The control channel's [`Dial`]
/// knows its port; a second caller (the uploader, whose object server has a port of its own,
/// ADR-493 item 13) names it per call.
pub(crate) trait PortDial: Send + Sync {
    fn dial_port(&self, port: u16) -> Dialled<'_>;
}

/// The production dial: a TCP stream to the loopback address and a port. The address is a
/// literal in this one place, so nothing a caller passes can leave the loopback.
pub(crate) struct LoopbackPorts;

impl PortDial for LoopbackPorts {
    fn dial_port(&self, port: u16) -> Dialled<'_> {
        Box::pin(async move {
            let stream = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
            // A heartbeat is a few bytes; do not hold it back. A failure here changes nothing.
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream) as Stream)
        })
    }
}

/// The production dial of the control channel: [`LoopbackPorts`] at the record's port.
pub(crate) struct Loopback {
    pub(crate) port: u16,
}

impl Dial for Loopback {
    fn dial(&self) -> Dialled<'_> {
        LoopbackPorts.dial_port(self.port)
    }
}

/// A [`PortDial`] fixed to one port, which is the portless [`Dial`] the exchange takes.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the uploader loop wires its exchange with it (next item)"
    )
)]
pub(crate) struct AtPort<'a> {
    pub(crate) dial: &'a dyn PortDial,
    pub(crate) port: u16,
}

impl Dial for AtPort<'_> {
    fn dial(&self) -> Dialled<'_> {
        self.dial.dial_port(self.port)
    }
}

/// Why no connection came.
pub(crate) enum ConnectError {
    /// The connection was refused or the address is unreachable.
    Failed,
    /// It did not finish by the connect bound.
    Timeout,
}

/// Dials, giving up at `by`.
pub(crate) async fn connect(dial: &dyn Dial, by: Instant) -> Result<Stream, ConnectError> {
    match tokio::time::timeout_at(by, dial.dial()).await {
        Err(_) => Err(ConnectError::Timeout),
        Ok(Err(_)) => Err(ConnectError::Failed),
        Ok(Ok(stream)) => Ok(stream),
    }
}

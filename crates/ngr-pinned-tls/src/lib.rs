//! The client side of TLS to the bundled object server (design 13.3, ADR-505): [`connect`] runs a
//! TLS handshake over a stream the caller opened and accepts the server only when its leaf
//! certificate has the given [`CertPin`], the SHA-256 of the certificate's DER.
//!
//! What the handshake checks, and what it does not:
//!
//! - The leaf's SHA-256 equals the pin. Nothing else about the certificate is judged: not its
//!   validity dates, not its names (the certificate is self-signed for the loopback address and
//!   the pin already names it), and not its issuer. Certificates after the leaf carry no
//!   authority and are ignored.
//! - The handshake's signature verifies under the leaf's public key. A peer that has only seen
//!   the public certificate cannot produce it, so presenting the right certificate with another
//!   key is refused.
//! - The connection is TLS 1.3 only, with the aws-lc-rs provider, no session resumption (every
//!   connection is a full handshake judged against its own pin), no ALPN and no server name
//!   (the name is the loopback address, for which no SNI is sent).
//!
//! The caller bounds the handshake in time: there is no timer here. A refused handshake is a
//! [`Refused`], which says only whether the certificate, the signature or the handshake's
//! mechanics failed. Nothing is written to the stream before the handshake has verified the
//! server, so a refused peer receives no application byte.
//!
//! The pin is a public value, so it is compared with `==`.
//!
//! # Feature `test-server`
//!
//! The [`test_server`] module, behind the feature of that name, makes self-signed test
//! certificates and TLS acceptors, including an impostor that presents a certificate it does not
//! hold the key of. It exists so that a caller's tests need no TLS crate of their own; no
//! production code enables it, and it adds `rcgen` to the build.

mod pin;
mod verifier;

#[cfg(feature = "test-server")]
pub mod test_server;

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::pki_types::ServerName;
use rustls::{CertificateError, ClientConfig, Error as TlsError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;

pub use pin::{CertPin, PinError};

/// Why the server was not accepted. The words are fixed: a refusal says which check failed and
/// nothing the peer sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The leaf certificate is not the one the pin names.
    Certificate,
    /// The certificate is the pinned one, or at least was presented, but the handshake is not
    /// signed by its key.
    Signature,
    /// The handshake failed for another reason: a peer that does not speak TLS, a close, a
    /// message that is not valid. The kind is that of the I/O error the handshake ended with.
    Handshake(io::ErrorKind),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Certificate => f.write_str("the server's certificate is not the pinned one"),
            Self::Signature => f.write_str("the server's handshake signature does not verify"),
            Self::Handshake(_) => f.write_str("the TLS handshake failed"),
        }
    }
}

impl std::error::Error for Refused {}

/// A stream with the handshake done and the server accepted.
pub struct Stream<S>(tokio_rustls::client::TlsStream<S>);

impl<S> std::fmt::Debug for Stream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Runs the TLS handshake as the client over `stream` and accepts the server only if it presents
/// the certificate `pin` names and proves it holds its key. There is no time bound here; the
/// caller bounds the call.
pub async fn connect<S>(pin: &CertPin, stream: S) -> Result<Stream<S>, Refused>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // No SNI is sent for an IP address; the name is not checked either.
    let name = ServerName::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST).into());
    TlsConnector::from(Arc::new(client_config(pin)))
        .connect(name, stream)
        .await
        .map(Stream)
        .map_err(refusal)
}

fn client_config(pin: &CertPin) -> ClientConfig {
    let verifier = verifier::PinVerifier::new(*pin);
    let mut config = ClientConfig::builder_with_provider(verifier.provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("the aws-lc-rs provider supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    config
}

/// What the I/O error a handshake ended with says. The verifier fails a wrong certificate with
/// `ApplicationVerificationFailure` and a bad signature with whatever the provider reports; no
/// other certificate error can arise in this client.
fn refusal(error: io::Error) -> Refused {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<TlsError>())
    {
        Some(TlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure)) => {
            Refused::Certificate
        }
        Some(TlsError::InvalidCertificate(_)) => Refused::Signature,
        _ => Refused::Handshake(error.kind()),
    }
}

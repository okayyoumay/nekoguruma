//! A TLS server for tests (feature `test-server`): self-signed certificates with random keys, an
//! [`Acceptor`] that serves one, and an impostor that presents a certificate it holds no key
//! for. Nothing here is for production, and no key material is fixed: every [`Identity`] is
//! generated when it is asked for.

use std::io;
use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384};
use rustls::ServerConfig;
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::pem::{self, SectionKind};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsAcceptor;

/// The kind of key an [`Identity`] has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    P256,
    P384,
}

/// A self-signed certificate and its private key.
pub struct Identity {
    certificate: Vec<u8>,
    key: Vec<u8>,
    certificate_pem: String,
    key_pem: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").finish_non_exhaustive()
    }
}

impl Identity {
    /// A certificate for `127.0.0.1`, valid now, with a new key of the given kind.
    pub fn generate(key: Key) -> Self {
        Self::make(key, Validity::Current, true)
    }

    /// As [`generate`](Self::generate) with a P-256 key and a validity that ended in the past.
    pub fn expired() -> Self {
        Self::make(Key::P256, Validity::Expired, true)
    }

    /// As [`generate`](Self::generate) with a P-256 key and a validity that begins in the future.
    pub fn not_yet_valid() -> Self {
        Self::make(Key::P256, Validity::NotYetValid, true)
    }

    /// As [`generate`](Self::generate) with a P-256 key and no subject alternative name.
    pub fn without_name() -> Self {
        Self::make(Key::P256, Validity::Current, false)
    }

    /// An identity from the PEM text of a certificate and of its PKCS#8 private key, as the
    /// files of the object server's credentials directory hold them. The key is not checked
    /// against the certificate. `None` when either text holds no section of its kind.
    pub fn from_pem(certificate_pem: &str, key_pem: &str) -> Option<Self> {
        let certificate = section(certificate_pem, SectionKind::Certificate)?;
        let key = section(key_pem, SectionKind::PrivateKey)?;
        Some(Self {
            certificate,
            key,
            certificate_pem: certificate_pem.to_owned(),
            key_pem: key_pem.to_owned(),
        })
    }

    /// The certificate's DER encoding.
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    /// The certificate as PEM text.
    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }

    /// The private key as PKCS#8 PEM text.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    fn make(key: Key, validity: Validity, name: bool) -> Self {
        let algorithm = match key {
            Key::P256 => &PKCS_ECDSA_P256_SHA256,
            Key::P384 => &PKCS_ECDSA_P384_SHA384,
        };
        let key = KeyPair::generate_for(algorithm).expect("a key pair is generated");
        let names = if name {
            vec!["127.0.0.1".to_owned()]
        } else {
            Vec::new()
        };
        let mut params = CertificateParams::new(names).expect("the names are valid");
        match validity {
            Validity::Current => {}
            Validity::Expired => {
                params.not_before = rcgen::date_time_ymd(2001, 1, 1);
                params.not_after = rcgen::date_time_ymd(2002, 1, 1);
            }
            Validity::NotYetValid => {
                params.not_before = rcgen::date_time_ymd(3001, 1, 1);
                params.not_after = rcgen::date_time_ymd(3002, 1, 1);
            }
        }
        let certificate = params.self_signed(&key).expect("the certificate is signed");
        Self {
            certificate: certificate.der().to_vec(),
            key: key.serialize_der(),
            certificate_pem: certificate.pem(),
            key_pem: key.serialize_pem(),
        }
    }
}

/// The DER of the first section of `kind` in `text`.
fn section(text: &str, kind: SectionKind) -> Option<Vec<u8>> {
    let mut rest = text.as_bytes();
    while let Ok(Some((found, der))) = pem::from_buf(&mut rest) {
        if found == kind {
            return Some(der);
        }
    }
    None
}

enum Validity {
    Current,
    Expired,
    NotYetValid,
}

/// Accepts TLS 1.3 connections, presenting a chain and signing with a key.
#[derive(Clone)]
pub struct Acceptor(TlsAcceptor);

impl std::fmt::Debug for Acceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acceptor").finish_non_exhaustive()
    }
}

impl Acceptor {
    /// An acceptor that serves `identity`: its certificate, signed with its key.
    pub fn new(identity: &Identity) -> Self {
        Self::presenting(&[identity.certificate()], identity)
    }

    /// An acceptor that presents `certificates` (DER, leaf first) and signs the handshake with
    /// `signing`'s key. The key is not checked against the certificates, so this is the impostor
    /// that has the certificate without its key (a leaf of another identity), and the server
    /// that sends a chain (several certificates).
    pub fn presenting(certificates: &[&[u8]], signing: &Identity) -> Self {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                signing.key.clone(),
            )))
            .expect("the key loads");
        let chain = certificates
            .iter()
            .map(|der| CertificateDer::from(der.to_vec()))
            .collect();
        let certified = CertifiedKey::new(chain, key);
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("the provider supports TLS 1.3")
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(SingleCertAndKey::from(certified)));
        Self(TlsAcceptor::from(Arc::new(config)))
    }

    /// Runs the server side of the handshake over `stream`.
    pub async fn accept<S>(&self, stream: S) -> io::Result<tokio_rustls::server::TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.0.accept(stream).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_rebuilt_from_its_pem_text() {
        let made = Identity::generate(Key::P384);
        let rebuilt = Identity::from_pem(made.certificate_pem(), made.key_pem()).unwrap();
        assert_eq!(rebuilt.certificate(), made.certificate());
        assert_eq!(rebuilt.key, made.key);
        // A text without the section of its kind is not an identity.
        assert!(Identity::from_pem(made.key_pem(), made.key_pem()).is_none());
        assert!(Identity::from_pem(made.certificate_pem(), "").is_none());
    }
}

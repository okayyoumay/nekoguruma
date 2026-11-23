use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error, PeerIncompatible, SignatureScheme};

use crate::CertPin;

/// Accepts one certificate, by its pin, and checks the handshake signature with its key.
#[derive(Debug)]
pub(crate) struct PinVerifier {
    pin: CertPin,
    provider: Arc<CryptoProvider>,
}

impl PinVerifier {
    pub(crate) fn new(pin: CertPin) -> Self {
        Self {
            pin,
            provider: Arc::new(aws_lc_rs::default_provider()),
        }
    }

    /// The provider the client configuration is built with, which is also the one whose
    /// algorithms check the signature.
    pub(crate) fn provider(&self) -> Arc<CryptoProvider> {
        self.provider.clone()
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // The pin is public, so a plain comparison leaks nothing.
        if CertPin::of_certificate(end_entity) == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        // TLS 1.2 is not offered, so no handshake reaches here.
        Err(Error::PeerIncompatible(PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

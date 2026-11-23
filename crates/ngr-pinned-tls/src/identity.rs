//! The key and certificate the server gives the object server (design 13.3, ADR-505; feature
//! `identity`): [`generate_key`] and [`self_signed`] make them, and [`pin_of_pair`] checks a pair
//! read from files and computes its [`CertPin`].
//!
//! The certificate is for `127.0.0.1` (an IP subject alternative name) and signed by its own key.
//! Its validity is rcgen's default and is not judged by anything: the verifier compares the pin
//! only (ADR-505 item 9). Errors have fixed text and carry nothing from the files.

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::pem::{self, SectionKind};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use rustls::{Error as TlsError, InconsistentKeys, SignatureScheme};

use crate::CertPin;

/// Which PEM text a [`IdentityError::NotOnePem`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pem {
    Certificate,
    Key,
}

/// Why a key, a certificate or a pair was refused. The words are fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// The text is not exactly one section of the expected label and nothing else (a missing
    /// section, a second one, a section of another label, or text that is not PEM).
    NotOnePem { what: Pem },
    /// The key is not a P-256 key.
    NotP256,
    /// The key is not the certificate's key.
    Mismatch,
    /// The key or the certificate cannot be parsed.
    Malformed,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotOnePem {
                what: Pem::Certificate,
            } => "the certificate file is not exactly one certificate in PEM form",
            Self::NotOnePem { what: Pem::Key } => {
                "the key file is not exactly one PKCS#8 private key in PEM form"
            }
            Self::NotP256 => "the key is not a P-256 key",
            Self::Mismatch => "the key does not belong to the certificate",
            Self::Malformed => "the key or the certificate is malformed",
        })
    }
}

impl std::error::Error for IdentityError {}

/// A new P-256 private key as PKCS#8 PEM.
pub fn generate_key() -> String {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .expect("the aws-lc-rs provider generates a P-256 key")
        .serialize_pem()
}

/// A certificate for `127.0.0.1`, signed by the key `key_pem` (PKCS#8 PEM), as PEM.
pub fn self_signed(key_pem: &str) -> Result<String, IdentityError> {
    let key = KeyPair::from_pem(key_pem).map_err(|_| IdentityError::Malformed)?;
    let params = CertificateParams::new(["127.0.0.1".to_owned()])
        .expect("the loopback address is a valid name");
    let certificate = params
        .self_signed(&key)
        .map_err(|_| IdentityError::Malformed)?;
    Ok(certificate.pem())
}

/// The pin of the certificate in `cert_pem`, after checking that the pair is what the server
/// makes: exactly one certificate section and nothing else in the one text, exactly one PKCS#8
/// private key section and nothing else in the other, the key a P-256 key, and the key the
/// certificate's own. Both texts may have CR LF line ends.
pub fn pin_of_pair(cert_pem: &str, key_pem: &str) -> Result<CertPin, IdentityError> {
    let certificate = CertificateDer::from(only_section(
        cert_pem,
        SectionKind::Certificate,
        Pem::Certificate,
    )?);
    let key = PrivatePkcs8KeyDer::from(only_section(key_pem, SectionKind::PrivateKey, Pem::Key)?);
    let signing = aws_lc_rs::default_provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(key))
        .map_err(|_| IdentityError::Malformed)?;
    if signing
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .is_none()
    {
        return Err(IdentityError::NotP256);
    }
    let pin = CertPin::of_certificate(certificate.as_ref());
    // `CertifiedKey::from_der` would accept a key whose public half it cannot read, so the match
    // is asked for and must be a positive answer.
    CertifiedKey::new(vec![certificate], signing)
        .keys_match()
        .map_err(|error| match error {
            TlsError::InconsistentKeys(InconsistentKeys::KeyMismatch) => IdentityError::Mismatch,
            _ => IdentityError::Malformed,
        })?;
    Ok(pin)
}

/// The DER of the one section `text` holds, which must be of kind `kind`.
fn only_section(text: &str, kind: SectionKind, what: Pem) -> Result<Vec<u8>, IdentityError> {
    let mut rest = text.as_bytes();
    let mut found = None;
    while let Some((section, der)) =
        pem::from_buf(&mut rest).map_err(|_| IdentityError::NotOnePem { what })?
    {
        if section != kind || found.replace(der).is_some() {
            return Err(IdentityError::NotOnePem { what });
        }
    }
    found.ok_or(IdentityError::NotOnePem { what })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (String, String) {
        let key = generate_key();
        let cert = self_signed(&key).unwrap();
        (cert, key)
    }

    fn cert_der(pem: &str) -> Vec<u8> {
        only_section(pem, SectionKind::Certificate, Pem::Certificate).unwrap()
    }

    fn crlf(text: &str) -> String {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    fn p384_key() -> String {
        KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)
            .unwrap()
            .serialize_pem()
    }

    #[test]
    fn a_made_pair_has_the_pin_of_its_certificate() {
        let (cert, key) = pair();
        let pin = pin_of_pair(&cert, &key).unwrap();
        assert_eq!(pin, CertPin::of_certificate(&cert_der(&cert)));
        // The same pair has the same pin; a second pair has another.
        assert_eq!(pin_of_pair(&cert, &key), Ok(pin));
        let (other_cert, other_key) = pair();
        assert_ne!(pin_of_pair(&other_cert, &other_key).unwrap(), pin);
    }

    #[test]
    fn the_certificate_names_the_loopback_address_by_ip() {
        let (cert, _) = pair();
        let der = cert_der(&cert);
        // The IP SAN's four octets as an OCTET STRING (tag 0x87, length 4).
        assert!(der.windows(6).any(|w| w == [0x87, 4, 127, 0, 0, 1]));
    }

    #[test]
    fn crlf_line_ends_are_accepted_in_either_text() {
        let (cert, key) = pair();
        let expected = pin_of_pair(&cert, &key).unwrap();
        assert_eq!(pin_of_pair(&crlf(&cert), &key), Ok(expected));
        assert_eq!(pin_of_pair(&cert, &crlf(&key)), Ok(expected));
        assert_eq!(pin_of_pair(&crlf(&cert), &crlf(&key)), Ok(expected));
    }

    #[test]
    fn a_text_that_is_not_one_certificate_is_refused() {
        let (cert, key) = pair();
        let (other_cert, _) = pair();
        let expected = IdentityError::NotOnePem {
            what: Pem::Certificate,
        };
        let cases = [
            ("empty", String::new()),
            ("garbage", "not a pem file".to_owned()),
            ("two certificates", format!("{cert}{other_cert}")),
            ("the key in the certificate's place", key.clone()),
            ("a certificate and a key", format!("{cert}{key}")),
            (
                "a certificate with a wrong end label",
                cert.replace("END CERTIFICATE", "END CERTIFICAT"),
            ),
        ];
        for (name, text) in cases {
            assert_eq!(pin_of_pair(&text, &key), Err(expected), "{name}");
        }
    }

    #[test]
    fn a_text_that_is_not_one_pkcs8_key_is_refused() {
        let (cert, key) = pair();
        let (_, other_key) = pair();
        let expected = IdentityError::NotOnePem { what: Pem::Key };
        let cases = [
            ("empty", String::new()),
            ("garbage", "not a pem file".to_owned()),
            ("two keys", format!("{key}{other_key}")),
            ("a key and a certificate", format!("{key}{cert}")),
            ("a certificate", cert.clone()),
            (
                "a traditional EC key label",
                key.replace("PRIVATE KEY", "EC PRIVATE KEY"),
            ),
        ];
        for (name, text) in cases {
            assert_eq!(pin_of_pair(&cert, &text), Err(expected), "{name}");
        }
    }

    #[test]
    fn a_key_that_is_not_p256_is_refused() {
        let (cert, _) = pair();
        assert_eq!(pin_of_pair(&cert, &p384_key()), Err(IdentityError::NotP256));
    }

    #[test]
    fn a_key_of_another_pair_is_refused() {
        let (cert, _) = pair();
        let (_, other_key) = pair();
        assert_eq!(pin_of_pair(&cert, &other_key), Err(IdentityError::Mismatch));
    }

    #[test]
    fn a_section_whose_body_is_not_the_object_is_refused() {
        let (cert, key) = pair();
        let bad_cert = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let bad_key = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert_eq!(pin_of_pair(bad_cert, &key), Err(IdentityError::Malformed));
        assert_eq!(pin_of_pair(&cert, bad_key), Err(IdentityError::Malformed));
    }

    #[test]
    fn a_made_certificate_needs_a_key() {
        assert_eq!(self_signed("garbage"), Err(IdentityError::Malformed));
    }

    #[test]
    fn errors_say_nothing_of_the_input() {
        let (cert, key) = pair();
        let error = pin_of_pair(&cert, &p384_key()).unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(key.lines().nth(1).unwrap()), "{shown}");
    }
}

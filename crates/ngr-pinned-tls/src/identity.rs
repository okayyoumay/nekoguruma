//! The key and certificate the server gives the object server (design 13.3, ADR-505; feature
//! `identity`): [`generate_key`] and [`self_signed`] make them, and [`pin_of_pair`] checks a pair
//! read from files and computes its [`CertPin`].
//!
//! The certificate is for `127.0.0.1` (an IP subject alternative name) and signed by its own key.
//! Its validity is rcgen's default and is not judged by anything: the verifier compares the pin
//! only (ADR-505 item 9). The text of an error is fixed and says nothing of the files; the cause
//! a library reported is the error's source, for the caller's log.

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::pem::{self, SectionKind};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use rustls::{Error as TlsError, InconsistentKeys, SignatureScheme};
use zeroize::Zeroizing;

use crate::CertPin;

/// Which PEM text a [`IdentityError::NotOnePem`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pem {
    Certificate,
    Key,
}

/// Why a key, a certificate or a pair was refused. The words are fixed.
#[derive(Debug)]
pub enum IdentityError {
    /// The text is not exactly one section of the expected label and nothing else (a missing
    /// section, a second one, a section of another label, text before or after the section, or
    /// text that is not PEM). Only white space may surround the section. `source` is the PEM
    /// reader's error when it refused the text, and `None` when the checks before and after it
    /// did.
    NotOnePem {
        what: Pem,
        source: Option<pem::Error>,
    },
    /// The key is not a P-256 key.
    NotP256,
    /// The key is not the certificate's key.
    Mismatch,
    /// The key or the certificate cannot be parsed; the source is the library's error.
    Malformed(Box<dyn std::error::Error + Send + Sync>),
    /// Operating-system randomness is unavailable, so no key could be made; the source is the
    /// generator's error.
    Random(rcgen::Error),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotOnePem {
                what: Pem::Certificate,
                ..
            } => "the certificate file is not exactly one certificate in PEM form",
            Self::NotOnePem { what: Pem::Key, .. } => {
                "the key file is not exactly one PKCS#8 private key in PEM form"
            }
            Self::NotP256 => "the key is not a P-256 key",
            Self::Mismatch => "the key does not belong to the certificate",
            Self::Malformed(_) => "the key or the certificate is malformed",
            Self::Random(_) => "operating-system randomness is unavailable",
        })
    }
}

impl std::error::Error for IdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotOnePem { source, .. } => source
                .as_ref()
                .map(|error| error as &(dyn std::error::Error + 'static)),
            Self::NotP256 | Self::Mismatch => None,
            Self::Malformed(error) => Some(error.as_ref()),
            Self::Random(error) => Some(error),
        }
    }
}

/// A new P-256 private key as PKCS#8 PEM, wiped when dropped. Fails with
/// [`IdentityError::Random`] when the operating system has no randomness to give.
pub fn generate_key() -> Result<Zeroizing<String>, IdentityError> {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map(|key| Zeroizing::new(key.serialize_pem()))
        .map_err(IdentityError::Random)
}

/// A certificate for `127.0.0.1`, signed by the key `key_pem` (PKCS#8 PEM), as PEM.
pub fn self_signed(key_pem: &str) -> Result<String, IdentityError> {
    let key = KeyPair::from_pem(key_pem).map_err(|error| IdentityError::Malformed(error.into()))?;
    let params = CertificateParams::new(["127.0.0.1".to_owned()])
        .expect("the loopback address is a valid name");
    let certificate = params
        .self_signed(&key)
        .map_err(|error| IdentityError::Malformed(error.into()))?;
    Ok(certificate.pem())
}

/// The pin of the certificate in `cert_pem`, after checking that the pair is what the server
/// makes: exactly one certificate section and nothing else in the one text, exactly one PKCS#8
/// private key section and nothing else in the other (only white space may surround a section),
/// the key a P-256 key, and the key the certificate's own. Both texts may have CR LF line ends.
/// The key's DER is wiped when this returns, except for the copy the signing key was built from,
/// which rustls and the provider own.
pub fn pin_of_pair(cert_pem: &str, key_pem: &str) -> Result<CertPin, IdentityError> {
    let certificate = CertificateDer::from(take(only_section(
        cert_pem,
        SectionKind::Certificate,
        Pem::Certificate,
    )?));
    let key = PrivatePkcs8KeyDer::from(take(only_section(
        key_pem,
        SectionKind::PrivateKey,
        Pem::Key,
    )?));
    let signing = aws_lc_rs::default_provider()
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(key))
        .map_err(|error| IdentityError::Malformed(error.into()))?;
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
            error => IdentityError::Malformed(error.into()),
        })?;
    Ok(pin)
}

/// The bytes out of their wiping holder, for a type that takes a `Vec` by value.
fn take(mut der: Zeroizing<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *der)
}

/// The DER of the one section `text` holds, which must be of kind `kind`. Only white space may
/// come before or after the section: the first and last lines are its BEGIN and END lines, and
/// no other BEGIN line exists, so text outside the section and a section of an unknown label
/// (which the PEM reader would skip) are refused.
fn only_section(
    text: &str,
    kind: SectionKind,
    what: Pem,
) -> Result<Zeroizing<Vec<u8>>, IdentityError> {
    let refused = |source| IdentityError::NotOnePem { what, source };
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().ok_or_else(|| refused(None))?;
    let last = lines.next_back().unwrap_or(first);
    let begins = text
        .lines()
        .filter(|line| line.trim().starts_with("-----BEGIN "))
        .count();
    if !first.starts_with("-----BEGIN ") || !last.starts_with("-----END ") || begins != 1 {
        return Err(refused(None));
    }
    let mut rest = text.as_bytes();
    let (section, der) = pem::from_buf(&mut rest)
        .map_err(|error| refused(Some(error)))?
        .ok_or_else(|| refused(None))?;
    let der = Zeroizing::new(der);
    if section != kind
        || pem::from_buf(&mut rest)
            .map_err(|error| refused(Some(error)))?
            .is_some()
    {
        return Err(refused(None));
    }
    Ok(der)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (String, String) {
        let key = generate_key().unwrap().to_string();
        let cert = self_signed(&key).unwrap();
        (cert, key)
    }

    fn cert_der(pem: &str) -> Vec<u8> {
        take(only_section(pem, SectionKind::Certificate, Pem::Certificate).unwrap())
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
        assert_eq!(pin_of_pair(&cert, &key).unwrap(), pin);
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
        assert_eq!(pin_of_pair(&crlf(&cert), &key).unwrap(), expected);
        assert_eq!(pin_of_pair(&cert, &crlf(&key)).unwrap(), expected);
        assert_eq!(pin_of_pair(&crlf(&cert), &crlf(&key)).unwrap(), expected);
    }

    #[test]
    fn a_text_that_is_not_one_certificate_is_refused() {
        let (cert, key) = pair();
        let (other_cert, _) = pair();
        let cases = [
            ("empty", String::new()),
            ("garbage", "not a pem file".to_owned()),
            ("two certificates", format!("{cert}{other_cert}")),
            ("the key in the certificate's place", key.clone()),
            ("a certificate and a key", format!("{cert}{key}")),
            ("a leading junk line", format!("junk\n{cert}")),
            (
                "a trailing unknown section",
                format!("{cert}-----BEGIN OTHER THING-----\nAAAA\n-----END OTHER THING-----\n"),
            ),
            ("a trailing line", format!("{cert}trailer\n")),
            (
                "an unknown section before the certificate",
                format!("-----BEGIN OTHER THING-----\nAAAA\n-----END OTHER THING-----\n{cert}"),
            ),
            (
                "a certificate with a wrong end label",
                cert.replace("END CERTIFICATE", "END CERTIFICAT"),
            ),
        ];
        for (name, text) in cases {
            assert!(
                matches!(
                    pin_of_pair(&text, &key),
                    Err(IdentityError::NotOnePem {
                        what: Pem::Certificate,
                        ..
                    })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn a_text_that_is_not_one_pkcs8_key_is_refused() {
        let (cert, key) = pair();
        let (_, other_key) = pair();
        let cases = [
            ("empty", String::new()),
            ("garbage", "not a pem file".to_owned()),
            ("two keys", format!("{key}{other_key}")),
            ("a key and a certificate", format!("{key}{cert}")),
            ("a certificate", cert.clone()),
            ("a leading junk line", format!("junk\n{key}")),
            (
                "a trailing unknown section",
                format!("{key}-----BEGIN OTHER THING-----\nAAAA\n-----END OTHER THING-----\n"),
            ),
            ("a trailing line", format!("{key}trailer\n")),
            (
                "a traditional EC key label",
                key.replace("PRIVATE KEY", "EC PRIVATE KEY"),
            ),
        ];
        for (name, text) in cases {
            assert!(
                matches!(
                    pin_of_pair(&cert, &text),
                    Err(IdentityError::NotOnePem { what: Pem::Key, .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn white_space_around_the_one_section_is_accepted() {
        let (cert, key) = pair();
        let expected = pin_of_pair(&cert, &key).unwrap();
        let padded = |text: &str| format!("\n  \r\n{text}\n\n \t\n");
        assert_eq!(
            pin_of_pair(&padded(&cert), &padded(&key)).unwrap(),
            expected
        );
    }

    #[test]
    fn a_key_that_is_not_p256_is_refused() {
        let (cert, _) = pair();
        assert!(matches!(
            pin_of_pair(&cert, &p384_key()),
            Err(IdentityError::NotP256)
        ));
    }

    #[test]
    fn a_key_of_another_pair_is_refused() {
        let (cert, _) = pair();
        let (_, other_key) = pair();
        assert!(matches!(
            pin_of_pair(&cert, &other_key),
            Err(IdentityError::Mismatch)
        ));
    }

    #[test]
    fn a_section_whose_body_is_not_the_object_is_refused() {
        let (cert, key) = pair();
        let bad_cert = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let bad_key = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        for result in [pin_of_pair(bad_cert, &key), pin_of_pair(&cert, bad_key)] {
            let error = result.unwrap_err();
            assert!(matches!(error, IdentityError::Malformed(_)), "{error:?}");
            assert!(std::error::Error::source(&error).is_some());
        }
    }

    #[test]
    fn a_made_certificate_needs_a_key() {
        assert!(matches!(
            self_signed("garbage"),
            Err(IdentityError::Malformed(_))
        ));
    }

    #[test]
    fn errors_say_nothing_of_the_input() {
        let (cert, key) = pair();
        let error = pin_of_pair(&cert, &p384_key()).unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(key.lines().nth(1).unwrap()), "{shown}");
    }
}

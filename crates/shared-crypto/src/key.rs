//! Key, signature and signer types over ECDSA P-256 with SHA-256 (ADR-459).

use std::fmt;

use p256::ecdsa::signature::{Signer as _, Verifier as _};
use p256::ecdsa::{self, VerifyingKey};
use p256::elliptic_curve::Generate as _;
use p256::elliptic_curve::zeroize::Zeroizing;
use p256::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding,
};
use sha2::{Digest, Sha256};

/// Length in bytes of a [`Signature`]: the fixed-size r followed by s.
pub const SIGNATURE_LEN: usize = 64;

/// A key could not be decoded.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The bytes are not a PKCS#8 private key for P-256.
    #[error("not a PKCS#8 P-256 private key")]
    InvalidPrivateKey,
    /// The bytes or text are not an SPKI public key for a P-256 point.
    #[error("not an SPKI P-256 public key")]
    InvalidPublicKey,
    /// The SPKI is valid but not in canonical form (uncompressed point, minimal DER).
    #[error("SPKI public key is not in canonical form")]
    NonCanonicalPublicKey,
}

/// Bytes are not a well-formed signature.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    /// A signature is exactly [`SIGNATURE_LEN`] bytes.
    #[error("signature must be {SIGNATURE_LEN} bytes, got {0}")]
    WrongLength(usize),
    /// r or s is zero or not below the group order.
    #[error("signature scalar out of range")]
    OutOfRange,
}

/// A signature did not verify.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The signature is in the high-S form, which this crate never produces and never accepts.
    #[error("signature is not in low-S form")]
    HighS,
    /// The signature does not match the key and message.
    #[error("signature does not match the key and message")]
    Mismatch,
}

/// A signer failed to sign (an operating-system or hardware store can fail).
#[derive(Debug, thiserror::Error)]
#[error("signing failed: {0}")]
pub struct SignError(pub String);

/// Operating-system randomness could not be read.
#[derive(Debug, thiserror::Error)]
#[error("operating-system randomness is unavailable")]
pub struct RandomError;

/// Fills 32 bytes from operating-system randomness: a challenge or a nonce.
pub fn random_32() -> Result<[u8; 32], RandomError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| RandomError)?;
    Ok(bytes)
}

/// A public key: P-256, carried as SPKI DER (ADR-459).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Decodes a DER SPKI. Anything but the canonical encoding is refused, so the stored bytes
    /// and the [`fingerprint`](Self::fingerprint) always describe the same key.
    pub fn from_spki_der(der: &[u8]) -> Result<Self, KeyError> {
        let key = VerifyingKey::from_public_key_der(der).map_err(|_| KeyError::InvalidPublicKey)?;
        let key = Self(key);
        if key.to_spki_der() != der {
            return Err(KeyError::NonCanonicalPublicKey);
        }
        Ok(key)
    }

    /// Decodes a PEM SPKI (label `PUBLIC KEY`).
    pub fn from_spki_pem(pem: &str) -> Result<Self, KeyError> {
        VerifyingKey::from_public_key_pem(pem)
            .map(Self)
            .map_err(|_| KeyError::InvalidPublicKey)
    }

    /// The canonical DER SPKI.
    pub fn to_spki_der(&self) -> Vec<u8> {
        self.0
            .to_public_key_der()
            .expect("encoding a valid P-256 public key cannot fail")
            .as_bytes()
            .to_vec()
    }

    /// The PEM SPKI (label `PUBLIC KEY`, LF line endings).
    pub fn to_spki_pem(&self) -> String {
        self.0
            .to_public_key_pem(LineEnding::LF)
            .expect("encoding a valid P-256 public key cannot fail")
    }

    /// SHA-256 of the canonical DER SPKI.
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::digest(self.to_spki_der()).into()
    }

    /// The fingerprint as 64 lowercase hex digits.
    pub fn fingerprint_hex(&self) -> String {
        self.fingerprint()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint_hex())
    }
}

/// A 64-byte signature (r then s). It can hold the high-S form so that [`verify`] can refuse it
/// by name; [`Signer`]s only produce the low-S form.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature(ecdsa::Signature);

impl Signature {
    /// Parses exactly [`SIGNATURE_LEN`] bytes. Zero or out-of-range scalars are refused.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SignatureError> {
        if bytes.len() != SIGNATURE_LEN {
            return Err(SignatureError::WrongLength(bytes.len()));
        }
        ecdsa::Signature::from_slice(bytes)
            .map(Self)
            .map_err(|_| SignatureError::OutOfRange)
    }

    /// The fixed-size encoding.
    pub fn to_bytes(&self) -> [u8; SIGNATURE_LEN] {
        let mut out = [0u8; SIGNATURE_LEN];
        out.copy_from_slice(&self.0.to_bytes());
        out
    }

    fn is_low_s(&self) -> bool {
        self.0.normalize_s() == self.0
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signature(")?;
        for b in self.to_bytes() {
            write!(f, "{b:02x}")?;
        }
        f.write_str(")")
    }
}

/// Checks `signature` over `message` against `key`: ECDSA P-256 with SHA-256, low-S only.
pub fn verify(key: &PublicKey, message: &[u8], signature: &Signature) -> Result<(), VerifyError> {
    if !signature.is_low_s() {
        return Err(VerifyError::HighS);
    }
    key.0
        .verify(message, &signature.0)
        .map_err(|_| VerifyError::Mismatch)
}

/// Something that holds a private key and signs with it: a [`SigningKey`] in memory, or a key
/// behind an operating-system store or a TPM, where signing can fail.
pub trait Signer {
    /// The public key that verifies this signer's signatures.
    fn public_key(&self) -> PublicKey;

    /// Signs `message` (SHA-256 is applied here) and returns the low-S form.
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError>;
}

/// A private key held in memory. Its [`Debug`](fmt::Debug) output shows only the fingerprint,
/// and the secret is wiped when it is dropped.
pub struct SigningKey(ecdsa::SigningKey);

impl SigningKey {
    /// Generates a key from operating-system randomness.
    pub fn generate() -> Self {
        Self(ecdsa::SigningKey::generate())
    }

    /// Decodes a PKCS#8 DER private key, as written by [`to_pkcs8_der`](Self::to_pkcs8_der).
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, KeyError> {
        ecdsa::SigningKey::from_pkcs8_der(der)
            .map(Self)
            .map_err(|_| KeyError::InvalidPrivateKey)
    }

    /// The PKCS#8 DER private key, in a buffer that is wiped when dropped. For the key stores.
    pub fn to_pkcs8_der(&self) -> Zeroizing<Vec<u8>> {
        let doc = self
            .0
            .to_pkcs8_der()
            .expect("encoding a valid P-256 private key cannot fail");
        Zeroizing::new(doc.as_bytes().to_vec())
    }
}

impl Signer for SigningKey {
    fn public_key(&self) -> PublicKey {
        PublicKey(*self.0.verifying_key())
    }

    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        let sig: ecdsa::Signature = self
            .0
            .try_sign(message)
            .map_err(|e| SignError(e.to_string()))?;
        Ok(Signature(sig.normalize_s()))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SigningKey({}, secret redacted)",
            self.public_key().fingerprint_hex()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The group order n of P-256.
    const ORDER: &str = "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551";

    pub(crate) fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `n - s` for a 32-byte big-endian s: flips low-S and high-S.
    fn negate_s(s: &[u8]) -> Vec<u8> {
        let n = hex(ORDER);
        let mut out = vec![0u8; 32];
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let mut d = i16::from(n[i]) - i16::from(s[i]) - borrow;
            borrow = i16::from(d < 0);
            if d < 0 {
                d += 256;
            }
            out[i] = d as u8;
        }
        out
    }

    fn with_s(sig: &Signature, s: &[u8]) -> Signature {
        let mut bytes = sig.to_bytes();
        bytes[32..].copy_from_slice(s);
        Signature::from_bytes(&bytes).unwrap()
    }

    // Known-answer data: RFC 6979 appendix A.2.5 (P-256, SHA-256).
    const KAT_SECRET: &str = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";
    const KAT_X: &str = "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6";
    const KAT_Y: &str = "7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299";
    // (message, r, s) as the RFC lists them; "sample"'s s is the high-S form.
    const KAT: [(&str, &str, &str); 2] = [
        (
            "sample",
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716",
            "f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8",
        ),
        (
            "test",
            "f1abb023518351cd71d881567b1ea663ed3efcf6c5132b354f28d3b0b7d38367",
            "019f4113742a2b14bd25926b49c649155f267e60d3814b4c0cc84250e46f0083",
        ),
    ];

    fn kat_public() -> PublicKey {
        let mut der = hex("3059301306072a8648ce3d020106082a8648ce3d030107034200");
        der.push(4);
        der.extend(hex(KAT_X));
        der.extend(hex(KAT_Y));
        PublicKey::from_spki_der(&der).unwrap()
    }

    fn kat_signing() -> SigningKey {
        // PKCS#8 wrapper around the bare scalar (no embedded public key).
        let mut der = hex("3041020100301306072a8648ce3d020106082a8648ce3d030107042730250201010420");
        der.extend(hex(KAT_SECRET));
        SigningKey::from_pkcs8_der(&der).unwrap()
    }

    fn sig_of(r: &str, s: &[u8]) -> Signature {
        let mut b = hex(r);
        b.extend_from_slice(s);
        Signature::from_bytes(&b).unwrap()
    }

    #[test]
    fn known_answer_verifies_in_low_s_form() {
        let key = kat_public();
        for (msg, r, s) in KAT {
            let s = hex(s);
            // Present each vector in its low-S form, which is the only one accepted.
            let low = if sig_of(r, &s).is_low_s() {
                s
            } else {
                negate_s(&s)
            };
            verify(&key, msg.as_bytes(), &sig_of(r, &low)).unwrap();
        }
    }

    #[test]
    fn known_answer_high_s_form_is_refused() {
        let key = kat_public();
        let (msg, r, s) = KAT[0];
        let high = sig_of(r, &hex(s));
        assert!(!high.is_low_s());
        assert_eq!(verify(&key, msg.as_bytes(), &high), Err(VerifyError::HighS));
    }

    #[test]
    fn known_answer_wrong_message_is_refused() {
        let (_, r, s) = KAT[1];
        let sig = sig_of(r, &hex(s));
        assert_eq!(
            verify(&kat_public(), b"sample", &sig),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn signing_reproduces_the_known_answers() {
        // RFC 6979 signing is deterministic; the signer returns the low-S form of each answer.
        let key = kat_signing();
        assert_eq!(key.public_key(), kat_public());
        for (msg, r, s) in KAT {
            let s = hex(s);
            let sig = sig_of(r, &s);
            let low = if sig.is_low_s() { s } else { negate_s(&s) };
            assert_eq!(key.sign(msg.as_bytes()).unwrap(), sig_of(r, &low));
        }
    }

    #[test]
    fn round_trip_and_refusals() {
        let key = SigningKey::generate();
        let other = SigningKey::generate();
        let sig = key.sign(b"message").unwrap();
        verify(&key.public_key(), b"message", &sig).unwrap();
        assert_eq!(
            verify(&other.public_key(), b"message", &sig),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(&key.public_key(), b"messagf", &sig),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verify(&key.public_key(), b"", &sig),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_signature_is_low_s_and_its_high_s_twin_is_refused() {
        let key = SigningKey::generate();
        for i in 0..16u8 {
            let sig = key.sign(&[i]).unwrap();
            assert!(sig.is_low_s());
            let twin = with_s(&sig, &negate_s(&sig.to_bytes()[32..]));
            assert!(!twin.is_low_s());
            assert_eq!(
                verify(&key.public_key(), &[i], &twin),
                Err(VerifyError::HighS)
            );
        }
    }

    #[test]
    fn signature_bytes_must_be_exactly_64() {
        let sig = SigningKey::generate().sign(b"m").unwrap();
        let bytes = sig.to_bytes();
        assert_eq!(Signature::from_bytes(&bytes).unwrap(), sig);
        assert!(matches!(
            Signature::from_bytes(&bytes[..63]),
            Err(SignatureError::WrongLength(63))
        ));
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(matches!(
            Signature::from_bytes(&long),
            Err(SignatureError::WrongLength(65))
        ));
        assert!(matches!(
            Signature::from_bytes(&[]),
            Err(SignatureError::WrongLength(0))
        ));
        assert!(matches!(
            Signature::from_bytes(&[0u8; 64]),
            Err(SignatureError::OutOfRange)
        ));
        assert!(matches!(
            Signature::from_bytes(&[0xff; 64]),
            Err(SignatureError::OutOfRange)
        ));
    }

    #[test]
    fn a_flipped_signature_bit_does_not_verify() {
        let key = SigningKey::generate();
        let sig = key.sign(b"m").unwrap();
        for bit in [0usize, 100, 255] {
            let mut bytes = sig.to_bytes();
            bytes[bit / 8] ^= 1 << (bit % 8);
            if let Ok(bad) = Signature::from_bytes(&bytes) {
                assert!(verify(&key.public_key(), b"m", &bad).is_err());
            }
        }
    }

    #[test]
    fn public_key_encodings_round_trip() {
        let key = SigningKey::generate().public_key();
        let der = key.to_spki_der();
        assert_eq!(PublicKey::from_spki_der(&der).unwrap(), key);
        let pem = key.to_spki_pem();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\n"));
        assert_eq!(PublicKey::from_spki_pem(&pem).unwrap(), key);
        assert_eq!(key.fingerprint(), <[u8; 32]>::from(Sha256::digest(&der)));
        assert_eq!(key.fingerprint_hex().len(), 64);
    }

    #[test]
    fn malformed_public_keys_are_refused() {
        let key = SigningKey::generate().public_key();
        let der = key.to_spki_der();
        assert!(PublicKey::from_spki_der(&[]).is_err());
        assert!(PublicKey::from_spki_der(&der[..der.len() - 1]).is_err());
        let mut long = der.clone();
        long.push(0);
        assert!(PublicKey::from_spki_der(&long).is_err());
        // Last byte of the point changed: no longer on the curve (or a different key at best).
        let mut off_curve = der.clone();
        *off_curve.last_mut().unwrap() ^= 1;
        assert_ne!(PublicKey::from_spki_der(&off_curve).ok(), Some(key));
        // The same curve point in compressed form is valid SPKI but not canonical.
        let mut compressed = hex("3039301306072a8648ce3d020106082a8648ce3d030107032200");
        let point = &der[der.len() - 65..];
        compressed.push(if point[64] & 1 == 0 { 2 } else { 3 });
        compressed.extend_from_slice(&point[1..33]);
        assert!(matches!(
            PublicKey::from_spki_der(&compressed),
            Err(KeyError::NonCanonicalPublicKey)
        ));
        // A key on another curve (P-384's OID with a P-256-sized body) is not accepted.
        let mut other_curve = der;
        let at = other_curve
            .windows(8)
            .position(|w| w == hex("2a8648ce3d030107"))
            .unwrap();
        other_curve[at + 7] = 0x22;
        assert!(PublicKey::from_spki_der(&other_curve).is_err());
    }

    #[test]
    fn pem_with_the_wrong_label_is_refused() {
        let key = SigningKey::generate().public_key();
        let pem = key.to_spki_pem().replace("PUBLIC KEY", "PRIVATE KEY");
        assert!(PublicKey::from_spki_pem(&pem).is_err());
        let pem = key.to_spki_pem().replace("PUBLIC KEY", "CERTIFICATE");
        assert!(PublicKey::from_spki_pem(&pem).is_err());
        assert!(PublicKey::from_spki_pem("").is_err());
        assert!(PublicKey::from_spki_pem("not pem").is_err());
    }

    #[test]
    fn private_key_round_trips_through_pkcs8() {
        let key = SigningKey::generate();
        let der = key.to_pkcs8_der();
        let back = SigningKey::from_pkcs8_der(&der).unwrap();
        assert_eq!(back.public_key(), key.public_key());
        // Signing is deterministic, so equal keys give equal signatures.
        assert_eq!(back.sign(b"m").unwrap(), key.sign(b"m").unwrap());
        assert!(SigningKey::from_pkcs8_der(&der[..der.len() - 1]).is_err());
        assert!(SigningKey::from_pkcs8_der(&[]).is_err());
        // A public key is not a private key.
        assert!(SigningKey::from_pkcs8_der(&key.public_key().to_spki_der()).is_err());
    }

    #[test]
    fn debug_output_hides_the_secret() {
        let key = kat_signing();
        let shown = format!("{key:?} {:#?}", key);
        assert!(shown.contains("redacted"));
        assert!(!shown.to_lowercase().contains(KAT_SECRET));
        assert!(!shown.to_lowercase().contains(&KAT_SECRET[..16]));
    }

    #[test]
    fn random_values_differ() {
        assert_ne!(random_32().unwrap(), random_32().unwrap());
    }

    #[test]
    fn signer_trait_object_signs() {
        let key = SigningKey::generate();
        let dynamic: &dyn Signer = &key;
        let sig = dynamic.sign(b"m").unwrap();
        verify(&dynamic.public_key(), b"m", &sig).unwrap();
    }
}

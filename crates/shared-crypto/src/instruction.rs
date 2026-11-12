//! The signed job instruction's envelope and signature (ADR-461, design 11.1).
//!
//! The server signs the exact bytes of an instruction's payload, and the agent verifies those
//! bytes before it parses them, so this module deals in bytes only. The payload's own layout
//! (the `JobInstruction` body) lives in `shared-proto`, which this crate does not depend on.
//!
//! # Envelope
//!
//! One blob, which the API carries as base64:
//!
//! | Offset | Length | Content |
//! |---|---|---|
//! | 0 | 1 | format byte, [`FORMAT_V1`] |
//! | 1 | 32 | fingerprint of the signing key ([`PublicKey::fingerprint`]) |
//! | 33 | 64 | signature, r then s, low-S |
//! | 97 | the rest | payload, at most [`MAX_PAYLOAD_LEN`] bytes |
//!
//! The format byte fixes the algorithm (ECDSA P-256 with SHA-256, ADR-459), so a new algorithm
//! is a new format byte. The fingerprint only selects which trusted key to verify with; it is
//! not signed, and a verifier compares it with the key it trusts and never takes a key from
//! it. The signature is made through [`sign_tagged`] under [`DOMAIN_TAG`], which is not the
//! connection proof's tag, so a signature made for one purpose never verifies as the other.

use crate::{
    PublicKey, SIGNATURE_LEN, SignError, Signature, SignatureError, Signer, VerifyError,
    sign_tagged, verify_tagged,
};

/// The tag that names job instructions in every signature made for them.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma job instruction";

/// The only envelope format so far.
pub const FORMAT_V1: u8 = 1;

/// Length of the key fingerprint in the envelope.
pub const FINGERPRINT_LEN: usize = 32;

/// Length of the envelope in front of the payload.
pub const HEADER_LEN: usize = 1 + FINGERPRINT_LEN + SIGNATURE_LEN;

/// Longest payload accepted, in bytes (64 KiB).
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024;

/// An envelope could not be built or split.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// Shorter than the fixed part of the envelope.
    #[error("envelope is shorter than its {HEADER_LEN}-byte header ({0} bytes)")]
    TooShort(usize),
    /// The format byte is not one this crate knows.
    #[error("unknown envelope format {0}")]
    UnknownFormat(u8),
    /// The payload is longer than [`MAX_PAYLOAD_LEN`].
    #[error("payload is {0} bytes, over the {MAX_PAYLOAD_LEN}-byte limit")]
    PayloadTooLong(usize),
}

/// Signs `payload` as a job instruction.
pub fn sign<S: Signer + ?Sized>(signer: &S, payload: &[u8]) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, payload)
}

/// Verifies a job instruction signature over `payload` against `key`.
pub fn verify(key: &PublicKey, payload: &[u8], signature: &Signature) -> Result<(), VerifyError> {
    verify_tagged(key, DOMAIN_TAG, payload, signature)
}

/// Why [`seal`] failed.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The payload is too long.
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
    /// The signer failed.
    #[error(transparent)]
    Sign(#[from] SignError),
}

/// Signs `payload` and joins the envelope: the blob the server delivers.
pub fn seal<S: Signer + ?Sized>(signer: &S, payload: &[u8]) -> Result<Vec<u8>, SealError> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(EnvelopeError::PayloadTooLong(payload.len()).into());
    }
    let signature = sign(signer, payload)?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(FORMAT_V1);
    out.extend_from_slice(&signer.public_key().fingerprint());
    out.extend_from_slice(&signature.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// The parts of an envelope, split but not yet verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope<'a> {
    fingerprint: [u8; FINGERPRINT_LEN],
    signature: [u8; SIGNATURE_LEN],
    payload: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// Splits an envelope by its layout: the format byte, the length and the payload bound are
    /// checked here, nothing cryptographic. The format byte comes first, since another format
    /// may have another header length, so a short blob of a future format reports the format.
    pub fn split(bytes: &'a [u8]) -> Result<Self, EnvelopeError> {
        match bytes.first() {
            None => return Err(EnvelopeError::TooShort(0)),
            Some(&FORMAT_V1) => {}
            Some(&other) => return Err(EnvelopeError::UnknownFormat(other)),
        }
        if bytes.len() < HEADER_LEN {
            return Err(EnvelopeError::TooShort(bytes.len()));
        }
        let payload = &bytes[HEADER_LEN..];
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(EnvelopeError::PayloadTooLong(payload.len()));
        }
        let mut fingerprint = [0u8; FINGERPRINT_LEN];
        fingerprint.copy_from_slice(&bytes[1..1 + FINGERPRINT_LEN]);
        let mut signature = [0u8; SIGNATURE_LEN];
        signature.copy_from_slice(&bytes[1 + FINGERPRINT_LEN..HEADER_LEN]);
        Ok(Self {
            fingerprint,
            signature,
            payload,
        })
    }

    /// The signing key's fingerprint as the sender wrote it. A selector only: compare it with
    /// the trusted key's fingerprint, never take a key from it.
    pub fn fingerprint(&self) -> &[u8; FINGERPRINT_LEN] {
        &self.fingerprint
    }

    /// The signature, parsed.
    pub fn signature(&self) -> Result<Signature, SignatureError> {
        Signature::from_bytes(&self.signature)
    }

    /// The payload bytes the signature covers. Verify before reading them.
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SigningKey, random_32};

    /// A payload of run-time random bytes, so no fixed value stands in for a secret.
    fn payload(len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while out.len() < len {
            out.extend(random_32().unwrap());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn the_envelope_has_the_documented_layout() {
        let key = SigningKey::generate().unwrap();
        let body = payload(40);
        let blob = seal(&key, &body).unwrap();
        assert_eq!(blob.len(), 97 + 40);
        assert_eq!(blob[0], 0x01);
        assert_eq!(&blob[1..33], &key.public_key().fingerprint());
        let sig = Signature::from_bytes(&blob[33..97]).unwrap();
        assert_eq!(&blob[97..], &body[..]);
        // The signature is over the tagged payload, and nothing else.
        verify_tagged(
            &key.public_key(),
            b"nekoguruma job instruction",
            &body,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn a_sealed_envelope_splits_and_verifies() {
        let key = SigningKey::generate().unwrap();
        let body = payload(100);
        let blob = seal(&key, &body).unwrap();
        let env = Envelope::split(&blob).unwrap();
        assert_eq!(env.fingerprint(), &key.public_key().fingerprint());
        assert_eq!(env.payload(), &body[..]);
        verify(&key.public_key(), env.payload(), &env.signature().unwrap()).unwrap();
        // An empty payload is a layout-valid envelope.
        let empty = seal(&key, &[]).unwrap();
        assert_eq!(Envelope::split(&empty).unwrap().payload(), &[] as &[u8]);
    }

    #[test]
    fn every_changed_byte_fails() {
        let key = SigningKey::generate().unwrap();
        let blob = seal(&key, &payload(48)).unwrap();
        let pk = key.public_key();
        for i in 0..blob.len() {
            let mut bad = blob.clone();
            bad[i] ^= 0x01;
            let outcome = Envelope::split(&bad).map(|env| {
                env.signature()
                    .ok()
                    .map(|sig| verify(&pk, env.payload(), &sig))
            });
            match i {
                // The format byte is refused by the split.
                0 => assert!(outcome.is_err(), "byte {i}"),
                // The fingerprint is not signed: the signature still verifies, which is why a
                // verifier compares it with its pinned key.
                1..=32 => assert_eq!(outcome, Ok(Some(Ok(()))), "byte {i}"),
                _ => assert_ne!(outcome, Ok(Some(Ok(()))), "byte {i}"),
            }
        }
    }

    #[test]
    fn another_key_fails() {
        let key = SigningKey::generate().unwrap();
        let other = SigningKey::generate().unwrap().public_key();
        let blob = seal(&key, &payload(32)).unwrap();
        let env = Envelope::split(&blob).unwrap();
        assert_eq!(
            verify(&other, env.payload(), &env.signature().unwrap()),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_proof_tag_signature_is_not_an_instruction() {
        let key = SigningKey::generate().unwrap();
        let body = payload(32);
        let proof_sig = sign_tagged(&key, crate::proof::DOMAIN_TAG, &body).unwrap();
        assert!(verify(&key.public_key(), &body, &proof_sig).is_err());
        // And an instruction signature is not a proof signature.
        let sig = sign(&key, &body).unwrap();
        assert!(verify_tagged(&key.public_key(), crate::proof::DOMAIN_TAG, &body, &sig).is_err());
        assert_ne!(DOMAIN_TAG, crate::proof::DOMAIN_TAG);
    }

    #[test]
    fn short_unknown_format_and_oversize_are_refused() {
        let key = SigningKey::generate().unwrap();
        let blob = seal(&key, &payload(8)).unwrap();
        assert_eq!(
            Envelope::split(&blob[..HEADER_LEN - 1]),
            Err(EnvelopeError::TooShort(HEADER_LEN - 1))
        );
        assert_eq!(Envelope::split(&[]), Err(EnvelopeError::TooShort(0)));
        // A short blob of another format reports the format, not the length.
        assert_eq!(
            Envelope::split(&[2, 0, 0]),
            Err(EnvelopeError::UnknownFormat(2))
        );
        let mut other_format = blob.clone();
        other_format[0] = 2;
        assert_eq!(
            Envelope::split(&other_format),
            Err(EnvelopeError::UnknownFormat(2))
        );
        // The bound is on the payload: the largest is accepted, one more is not.
        let at_limit = seal(&key, &payload(MAX_PAYLOAD_LEN)).unwrap();
        assert!(Envelope::split(&at_limit).is_ok());
        let mut over = at_limit.clone();
        over.push(0);
        assert_eq!(
            Envelope::split(&over),
            Err(EnvelopeError::PayloadTooLong(MAX_PAYLOAD_LEN + 1))
        );
        assert!(matches!(
            seal(&key, &payload(MAX_PAYLOAD_LEN + 1)),
            Err(SealError::Envelope(EnvelopeError::PayloadTooLong(_)))
        ));
    }

    #[test]
    fn a_high_s_signature_is_refused() {
        // The same signature with s replaced by n - s is a valid-looking second encoding.
        let key = SigningKey::generate().unwrap();
        let body = payload(16);
        let sig = sign(&key, &body).unwrap().to_bytes();
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let mut flipped = sig;
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let d = i16::from(N[i]) - i16::from(sig[32 + i]) - borrow;
            borrow = i16::from(d < 0);
            flipped[32 + i] = (d + 256) as u8;
        }
        let high = Signature::from_bytes(&flipped).unwrap();
        assert_eq!(
            verify(&key.public_key(), &body, &high),
            Err(VerifyError::HighS)
        );
        verify(
            &key.public_key(),
            &body,
            &Signature::from_bytes(&sig).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_malformed_signature_is_reported_by_the_parse() {
        let key = SigningKey::generate().unwrap();
        let mut blob = seal(&key, &payload(8)).unwrap();
        // r = 0 is out of range.
        blob[33..65].fill(0);
        let env = Envelope::split(&blob).unwrap();
        assert!(env.signature().is_err());
    }
}

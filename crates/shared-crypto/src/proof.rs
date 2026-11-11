//! The agent's connection proof (ADR-459, design 6.4 and 14.4).
//!
//! The agent proves it holds its key by signing a transcript that binds the server it talks to,
//! its own identity and two fresh values: a challenge the server just issued and a nonce the
//! agent chose. The transcript has no timestamp, because the device clock is untrusted
//! (design 16.3). The server issues each challenge once and refuses a repeat, which is what
//! stops replay.
//!
//! # Encoding and domain separation
//!
//! The transcript body is the protocol version ([`PROTOCOL_VERSION`], 4 bytes big-endian,
//! always written by this crate), the audience and the agent ID (each a 4-byte big-endian
//! length, then UTF-8 bytes), then the challenge and the nonce (32 bytes each). Variable-length
//! fields are length-prefixed, so two different field sets never encode to the same bytes.
//!
//! The body is signed through [`sign_tagged`] under [`DOMAIN_TAG`], which names this purpose
//! and is length-prefixed in front of the body. Job instructions will be signed with the same
//! key primitive (design 11.1) through the same pair under a tag of their own, and a verifier
//! of either purpose rebuilds its own message from its own tag. A proof signature therefore
//! does not verify as a job instruction, and the reverse. A new signed purpose must pick a new
//! tag and never reuse this one. The version lives only in the body, so a layout change bumps
//! [`PROTOCOL_VERSION`] and the tag stays.

use crate::{PublicKey, SignError, Signature, Signer, VerifyError, sign_tagged, verify_tagged};

/// The tag that names this purpose in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma agent connection proof";

/// The transcript layout version this crate writes into every transcript.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest audience or agent ID accepted, in bytes.
pub const MAX_FIELD_LEN: usize = 1024;

/// Length of the server challenge and of the agent nonce.
pub const VALUE_LEN: usize = 32;

/// A transcript could not be built.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// The audience or agent ID is empty, or longer than [`MAX_FIELD_LEN`].
    #[error("{field} must be 1 to {MAX_FIELD_LEN} bytes")]
    BadField {
        /// `audience` or `agent_id`.
        field: &'static str,
    },
}

/// What a connection proof covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofTranscript {
    audience: String,
    agent_id: String,
    challenge: [u8; VALUE_LEN],
    nonce: [u8; VALUE_LEN],
}

impl ProofTranscript {
    /// Builds a transcript. `audience` is the server's origin string as the agent was
    /// configured with it; `agent_id` is the server-assigned agent ID.
    pub fn new(
        audience: &str,
        agent_id: &str,
        challenge: [u8; VALUE_LEN],
        nonce: [u8; VALUE_LEN],
    ) -> Result<Self, TranscriptError> {
        for (field, value) in [("audience", audience), ("agent_id", agent_id)] {
            if value.is_empty() || value.len() > MAX_FIELD_LEN {
                return Err(TranscriptError::BadField { field });
            }
        }
        Ok(Self {
            audience: audience.to_owned(),
            agent_id: agent_id.to_owned(),
            challenge,
            nonce,
        })
    }

    /// The body that is signed under [`DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + DOMAIN_TAG.len()
                + 4
                + 8
                + self.audience.len()
                + self.agent_id.len()
                + 2 * VALUE_LEN,
        );
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, self.audience.as_bytes());
        put_prefixed(&mut out, self.agent_id.as_bytes());
        out.extend_from_slice(&self.challenge);
        out.extend_from_slice(&self.nonce);
        out
    }
}

fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("fields are length-checked");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Signs `transcript` with the agent's key.
pub fn sign_proof<S: Signer + ?Sized>(
    signer: &S,
    transcript: &ProofTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, &transcript.encode())
}

/// Verifies a connection proof against the agent's registered public key.
pub fn verify_proof(
    key: &PublicKey,
    transcript: &ProofTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    verify_tagged(key, DOMAIN_TAG, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SignErrorKind, SigningKey};

    const AUDIENCE: &str = "https://ngr.example.com";
    const AGENT: &str = "3f1c2a52-6a0e-4c0d-9d3e-0a1b2c3d4e5f";

    /// A fresh 32-byte value for a challenge or nonce. The tests make their values at run time
    /// rather than writing fixed ones.
    fn fresh() -> [u8; 32] {
        crate::random_32().unwrap()
    }

    /// A challenge and a nonce that differ from each other.
    fn pair() -> ([u8; 32], [u8; 32]) {
        let challenge = fresh();
        loop {
            let nonce = fresh();
            if nonce != challenge {
                return (challenge, nonce);
            }
        }
    }

    fn transcript(challenge: [u8; 32], nonce: [u8; 32]) -> ProofTranscript {
        ProofTranscript::new(AUDIENCE, AGENT, challenge, nonce).unwrap()
    }

    #[test]
    fn a_proof_verifies() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        verify_proof(&key.public_key(), &transcript(c, n), &sig).unwrap();
    }

    #[test]
    fn encoding_is_the_documented_layout() {
        let (c, n) = pair();
        let t = ProofTranscript::new("ab", "c", c, n).unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(2u32.to_be_bytes());
        want.extend(b"ab");
        want.extend(1u32.to_be_bytes());
        want.extend(b"c");
        want.extend(c);
        want.extend(n);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_field_fails() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        let pk = key.public_key();
        let mut other_c = c;
        other_c[0] ^= 1;
        let mut other_n = n;
        other_n[31] ^= 1;
        let cases = [
            ProofTranscript::new("https://ngr.example.org", AGENT, c, n).unwrap(),
            ProofTranscript::new(AUDIENCE, "other", c, n).unwrap(),
            transcript(other_c, n),
            transcript(c, other_n),
            // Challenge and nonce swapped.
            transcript(n, c),
        ];
        for t in cases {
            assert_eq!(
                verify_proof(&pk, &t, &sig),
                Err(VerifyError::Mismatch),
                "{t:?}"
            );
        }
    }

    #[test]
    fn a_changed_version_fails() {
        let (c, n) = pair();
        // The version is internal, so build the bytes of another version by hand.
        let key = SigningKey::generate().unwrap();
        let mut body = transcript(c, n).encode();
        body[..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let sig = sign_tagged(&key, DOMAIN_TAG, &body).unwrap();
        assert!(verify_proof(&key.public_key(), &transcript(c, n), &sig).is_err());
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        // Moving a byte between audience and agent ID must change the encoding.
        let (c, n) = pair();
        let a = ProofTranscript::new("ab", "c", c, n).unwrap();
        let b = ProofTranscript::new("a", "bc", c, n).unwrap();
        assert_ne!(a.encode(), b.encode());
    }

    #[test]
    fn another_key_or_signature_form_fails() {
        let (c, n) = pair();
        let key = SigningKey::generate().unwrap();
        let sig = sign_proof(&key, &transcript(c, n)).unwrap();
        let other = SigningKey::generate().unwrap().public_key();
        assert_eq!(
            verify_proof(&other, &transcript(c, n), &sig),
            Err(VerifyError::Mismatch)
        );
        let bytes = sig.to_bytes();
        assert!(Signature::from_bytes(&bytes[..63]).is_err());
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(Signature::from_bytes(&long).is_err());
    }

    #[test]
    fn a_signature_without_the_tag_is_not_a_proof() {
        let (c, n) = pair();
        // The same body signed untagged (the raw primitive) does not verify as a proof.
        let key = SigningKey::generate().unwrap();
        let t = transcript(c, n);
        let raw = key.sign(&t.encode()).unwrap();
        assert!(verify_proof(&key.public_key(), &t, &raw).is_err());
    }

    #[test]
    fn a_signature_under_another_tag_is_not_a_proof() {
        let (c, n) = pair();
        // What a job instruction signer would do: the same body under its own tag.
        let key = SigningKey::generate().unwrap();
        let t = transcript(c, n);
        let other = sign_tagged(&key, b"nekoguruma job instruction", &t.encode()).unwrap();
        assert!(verify_proof(&key.public_key(), &t, &other).is_err());
        verify_tagged(
            &key.public_key(),
            b"nekoguruma job instruction",
            &t.encode(),
            &other,
        )
        .unwrap();
        // And a proof does not verify under the other tag.
        let proof = sign_proof(&key, &t).unwrap();
        assert!(
            verify_tagged(
                &key.public_key(),
                b"nekoguruma job instruction",
                &t.encode(),
                &proof
            )
            .is_err()
        );
    }

    #[test]
    fn empty_or_oversized_fields_are_refused() {
        let (c, n) = pair();
        let bad = |a: &str, g: &str| ProofTranscript::new(a, g, c, n);
        assert_eq!(
            bad("", AGENT),
            Err(TranscriptError::BadField { field: "audience" })
        );
        assert_eq!(
            bad(AUDIENCE, ""),
            Err(TranscriptError::BadField { field: "agent_id" })
        );
        let long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(bad(&long, AGENT).is_err());
        assert!(bad(AUDIENCE, &long).is_err());
        assert!(bad(&"x".repeat(MAX_FIELD_LEN), AGENT).is_ok());
    }

    #[test]
    fn signing_failure_is_reported_without_detail() {
        struct Broken(SigningKey);
        impl Signer for Broken {
            fn public_key(&self) -> PublicKey {
                self.0.public_key()
            }
            fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
                Err(SignError::with_source(
                    SignErrorKind::Unavailable,
                    "collection locked",
                ))
            }
        }
        let err = sign_proof(
            &Broken(SigningKey::generate().unwrap()),
            &transcript(fresh(), fresh()),
        )
        .unwrap_err();
        assert_eq!(err.kind(), SignErrorKind::Unavailable);
        assert!(!err.to_string().contains("collection"));
        assert!(
            std::error::Error::source(&err)
                .unwrap()
                .to_string()
                .contains("collection")
        );
    }
}

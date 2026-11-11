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
//! The signed bytes are the purpose tag, the protocol version (4 bytes, big-endian), the
//! audience and the agent ID (each a 4-byte big-endian length, then UTF-8 bytes), then the
//! challenge and the nonce (32 bytes each). Variable-length fields are length-prefixed, so two
//! different field sets never encode to the same bytes.
//!
//! The tag, [`DOMAIN_TAG`], is itself length-prefixed and names this purpose. Job instructions
//! will be signed with the same key primitive (design 11.1) under a tag of their own, and a
//! verifier of either purpose rebuilds its own message from its own tag. A proof signature
//! therefore does not verify as a job instruction, and the reverse, even though both come from
//! one primitive. A new signed purpose must pick a new tag and never reuse this one.

use crate::{PublicKey, SignError, Signature, Signer, VerifyError, key};

/// The tag that names this purpose in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma agent connection proof v1";

/// The transcript layout version this crate writes.
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
    version: u32,
    audience: String,
    agent_id: String,
    challenge: [u8; VALUE_LEN],
    nonce: [u8; VALUE_LEN],
}

impl ProofTranscript {
    /// Builds a transcript. `audience` is the server's origin string as the agent was
    /// configured with it; `agent_id` is the server-assigned agent ID.
    pub fn new(
        version: u32,
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
            version,
            audience: audience.to_owned(),
            agent_id: agent_id.to_owned(),
            challenge,
            nonce,
        })
    }

    /// The bytes that are signed.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + DOMAIN_TAG.len()
                + 4
                + 8
                + self.audience.len()
                + self.agent_id.len()
                + 2 * VALUE_LEN,
        );
        put_prefixed(&mut out, DOMAIN_TAG);
        out.extend_from_slice(&self.version.to_be_bytes());
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
    signer.sign(&transcript.encode())
}

/// Verifies a connection proof against the agent's registered public key.
pub fn verify_proof(
    key: &PublicKey,
    transcript: &ProofTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    key::verify(key, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Signature, SigningKey};

    const AUDIENCE: &str = "https://ngr.example.com";
    const AGENT: &str = "3f1c2a52-6a0e-4c0d-9d3e-0a1b2c3d4e5f";

    fn base() -> ProofTranscript {
        ProofTranscript::new(PROTOCOL_VERSION, AUDIENCE, AGENT, [1; 32], [2; 32]).unwrap()
    }

    #[test]
    fn a_proof_verifies() {
        let key = SigningKey::generate();
        let sig = sign_proof(&key, &base()).unwrap();
        verify_proof(&key.public_key(), &base(), &sig).unwrap();
    }

    #[test]
    fn encoding_is_the_documented_layout() {
        let t = ProofTranscript::new(7, "ab", "c", [1; 32], [2; 32]).unwrap();
        let mut want = Vec::new();
        want.extend((DOMAIN_TAG.len() as u32).to_be_bytes());
        want.extend(DOMAIN_TAG);
        want.extend(7u32.to_be_bytes());
        want.extend(2u32.to_be_bytes());
        want.extend(b"ab");
        want.extend(1u32.to_be_bytes());
        want.extend(b"c");
        want.extend([1u8; 32]);
        want.extend([2u8; 32]);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_field_fails() {
        let key = SigningKey::generate();
        let sig = sign_proof(&key, &base()).unwrap();
        let pk = key.public_key();
        let cases = [
            ProofTranscript::new(2, AUDIENCE, AGENT, [1; 32], [2; 32]).unwrap(),
            ProofTranscript::new(1, "https://ngr.example.org", AGENT, [1; 32], [2; 32]).unwrap(),
            ProofTranscript::new(1, AUDIENCE, "other", [1; 32], [2; 32]).unwrap(),
            ProofTranscript::new(1, AUDIENCE, AGENT, [3; 32], [2; 32]).unwrap(),
            ProofTranscript::new(1, AUDIENCE, AGENT, [1; 32], [3; 32]).unwrap(),
            // Challenge and nonce swapped.
            ProofTranscript::new(1, AUDIENCE, AGENT, [2; 32], [1; 32]).unwrap(),
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
    fn field_boundaries_are_unambiguous() {
        // Moving a byte between audience and agent ID must change the encoding.
        let a = ProofTranscript::new(1, "ab", "c", [0; 32], [0; 32]).unwrap();
        let b = ProofTranscript::new(1, "a", "bc", [0; 32], [0; 32]).unwrap();
        assert_ne!(a.encode(), b.encode());
    }

    #[test]
    fn another_key_or_signature_form_fails() {
        let key = SigningKey::generate();
        let sig = sign_proof(&key, &base()).unwrap();
        let other = SigningKey::generate().public_key();
        assert_eq!(
            verify_proof(&other, &base(), &sig),
            Err(VerifyError::Mismatch)
        );
        let bytes = sig.to_bytes();
        assert!(Signature::from_bytes(&bytes[..63]).is_err());
        let mut long = bytes.to_vec();
        long.push(0);
        assert!(Signature::from_bytes(&long).is_err());
    }

    #[test]
    fn a_proof_signature_is_not_a_signature_over_the_bare_transcript_under_another_tag() {
        // A signature over the same fields without this purpose's tag (what a different
        // purpose would sign) does not verify as a proof, and the reverse.
        let key = SigningKey::generate();
        let t = base();
        let encoded = t.encode();
        let tagless = &encoded[4 + DOMAIN_TAG.len()..];
        let tagless_sig = key.sign(tagless).unwrap();
        assert!(verify_proof(&key.public_key(), &t, &tagless_sig).is_err());
        let proof_sig = sign_proof(&key, &t).unwrap();
        assert!(crate::verify(&key.public_key(), tagless, &proof_sig).is_err());
    }

    #[test]
    fn empty_or_oversized_fields_are_refused() {
        let bad = |a: &str, g: &str| ProofTranscript::new(1, a, g, [0; 32], [0; 32]);
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
    fn signing_failure_is_reported() {
        struct Broken(SigningKey);
        impl Signer for Broken {
            fn public_key(&self) -> PublicKey {
                self.0.public_key()
            }
            fn sign(&self, _: &[u8]) -> Result<Signature, SignError> {
                Err(SignError("store locked".into()))
            }
        }
        let err = sign_proof(&Broken(SigningKey::generate()), &base()).unwrap_err();
        assert!(err.to_string().contains("store locked"));
    }
}

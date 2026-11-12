//! The agent's proof of possession at registration (ADR-463, design 6.4 and 6.7).
//!
//! An agent registers with a single-use registration code, and the request carries its public
//! key. The proof shows that the sender holds the private half of that key, so the server never
//! records a key its owner cannot use. The connection proof ([`crate::proof`]) cannot serve
//! here: it names the agent ID and a server challenge, and neither exists before registration.
//! The freshness comes from the code, which the server redeems once.
//!
//! # Encoding and domain separation
//!
//! The transcript body is the layout version ([`PROTOCOL_VERSION`], 4 bytes big-endian, always
//! written by this crate), then four fields, each a 4-byte big-endian length followed by its
//! bytes: the SHA-256 of the code, the agent key's fingerprint, the mode as UTF-8 text, and the
//! server key ID as UTF-8 text. Every field is length-prefixed, so two different field sets
//! never encode to the same bytes. The code itself is not in the transcript, only its hash,
//! which is also how the server keys its records.
//!
//! The body is signed through [`sign_tagged`] under [`DOMAIN_TAG`], a tag of its own, so a
//! registration signature does not verify as a connection proof or a job instruction, and the
//! reverse. The version lives only in the body.

use sha2::{Digest, Sha256};

use crate::{PublicKey, SignError, Signature, Signer, VerifyError, sign_tagged, verify_tagged};

/// The tag that names this purpose in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma agent registration";

/// The transcript layout version this crate writes into every transcript.
pub const PROTOCOL_VERSION: u32 = 1;

/// Longest mode or server key ID accepted, in bytes.
pub const MAX_FIELD_LEN: usize = 256;

/// A transcript could not be built.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TranscriptError {
    /// The mode or the server key ID is empty, or longer than [`MAX_FIELD_LEN`].
    #[error("{field} must be 1 to {MAX_FIELD_LEN} bytes")]
    BadField {
        /// `mode` or `server_key_id`.
        field: &'static str,
    },
}

/// What a registration proof covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationTranscript {
    code_hash: [u8; 32],
    agent_key_fingerprint: [u8; 32],
    mode: String,
    server_key_id: String,
}

impl RegistrationTranscript {
    /// Builds a transcript. `code` is the registration code as typed (its SHA-256 is taken
    /// here), `agent_key` the key being registered, `mode` the mode string of the request and
    /// `server_key_id` the server key ID the operator gave the agent.
    pub fn new(
        code: &str,
        agent_key: &PublicKey,
        mode: &str,
        server_key_id: &str,
    ) -> Result<Self, TranscriptError> {
        for (field, value) in [("mode", mode), ("server_key_id", server_key_id)] {
            if value.is_empty() || value.len() > MAX_FIELD_LEN {
                return Err(TranscriptError::BadField { field });
            }
        }
        Ok(Self {
            code_hash: Sha256::digest(code.as_bytes()).into(),
            agent_key_fingerprint: agent_key.fingerprint(),
            mode: mode.to_owned(),
            server_key_id: server_key_id.to_owned(),
        })
    }

    /// The body that is signed under [`DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(4 + 4 * 4 + 2 * 32 + self.mode.len() + self.server_key_id.len());
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, &self.code_hash);
        put_prefixed(&mut out, &self.agent_key_fingerprint);
        put_prefixed(&mut out, self.mode.as_bytes());
        put_prefixed(&mut out, self.server_key_id.as_bytes());
        out
    }
}

fn put_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("fields are length-checked");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Signs `transcript` with the key being registered.
pub fn sign_registration<S: Signer + ?Sized>(
    signer: &S,
    transcript: &RegistrationTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, &transcript.encode())
}

/// Verifies a registration proof against the public key the request carried.
pub fn verify_registration(
    key: &PublicKey,
    transcript: &RegistrationTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    verify_tagged(key, DOMAIN_TAG, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SigningKey, instruction, proof};

    /// A code-shaped string that differs on every call.
    fn code() -> String {
        let bytes = crate::random_32().unwrap();
        let hex: String = bytes[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!("ngr1-{hex}")
    }

    fn server_key_id() -> String {
        SigningKey::generate()
            .unwrap()
            .public_key()
            .fingerprint_hex()
    }

    #[test]
    fn a_registration_proof_verifies() {
        let key = SigningKey::generate().unwrap();
        let (code, id) = (code(), server_key_id());
        let t = RegistrationTranscript::new(&code, &key.public_key(), "device", &id).unwrap();
        let sig = sign_registration(&key, &t).unwrap();
        verify_registration(&key.public_key(), &t, &sig).unwrap();
    }

    #[test]
    fn encoding_is_the_documented_layout() {
        let key = SigningKey::generate().unwrap();
        let t = RegistrationTranscript::new("abc", &key.public_key(), "user", "k1").unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(32u32.to_be_bytes());
        want.extend(Sha256::digest(b"abc"));
        want.extend(32u32.to_be_bytes());
        want.extend(key.public_key().fingerprint());
        want.extend(4u32.to_be_bytes());
        want.extend(b"user");
        want.extend(2u32.to_be_bytes());
        want.extend(b"k1");
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_field_fails() {
        let key = SigningKey::generate().unwrap();
        let pk = key.public_key();
        let (code, id) = (code(), server_key_id());
        let t = RegistrationTranscript::new(&code, &pk, "device", &id).unwrap();
        let sig = sign_registration(&key, &t).unwrap();
        let other_key = SigningKey::generate().unwrap().public_key();
        let cases = [
            RegistrationTranscript::new(&self::code(), &pk, "device", &id).unwrap(),
            RegistrationTranscript::new(&code, &other_key, "device", &id).unwrap(),
            RegistrationTranscript::new(&code, &pk, "user", &id).unwrap(),
            RegistrationTranscript::new(&code, &pk, "device", &server_key_id()).unwrap(),
        ];
        for changed in cases {
            assert_eq!(
                verify_registration(&pk, &changed, &sig),
                Err(VerifyError::Mismatch),
                "{changed:?}"
            );
        }
        // Another key's signature over the same transcript fails too.
        let other = SigningKey::generate().unwrap();
        let foreign = sign_registration(&other, &t).unwrap();
        assert_eq!(
            verify_registration(&pk, &t, &foreign),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn a_changed_version_fails() {
        let key = SigningKey::generate().unwrap();
        let t = RegistrationTranscript::new(&code(), &key.public_key(), "device", &server_key_id())
            .unwrap();
        let mut body = t.encode();
        body[..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let sig = sign_tagged(&key, DOMAIN_TAG, &body).unwrap();
        assert!(verify_registration(&key.public_key(), &t, &sig).is_err());
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        let key = SigningKey::generate().unwrap().public_key();
        let a = RegistrationTranscript::new("c", &key, "ab", "c").unwrap();
        let b = RegistrationTranscript::new("c", &key, "a", "bc").unwrap();
        assert_ne!(a.encode(), b.encode());
    }

    #[test]
    fn empty_or_oversized_fields_are_refused() {
        let key = SigningKey::generate().unwrap().public_key();
        let bad = |m: &str, s: &str| RegistrationTranscript::new("c", &key, m, s);
        assert_eq!(
            bad("", "k"),
            Err(TranscriptError::BadField { field: "mode" })
        );
        assert_eq!(
            bad("device", ""),
            Err(TranscriptError::BadField {
                field: "server_key_id"
            })
        );
        let long = "x".repeat(MAX_FIELD_LEN + 1);
        assert!(bad(&long, "k").is_err());
        assert!(bad("device", &long).is_err());
        assert!(bad(&"x".repeat(MAX_FIELD_LEN), "k").is_ok());
    }

    #[test]
    fn tags_are_distinct_and_signatures_do_not_cross() {
        assert_ne!(DOMAIN_TAG, proof::DOMAIN_TAG);
        assert_ne!(DOMAIN_TAG, instruction::DOMAIN_TAG);
        let key = SigningKey::generate().unwrap();
        let pk = key.public_key();
        let t = RegistrationTranscript::new(&code(), &pk, "device", &server_key_id()).unwrap();
        let body = t.encode();
        let registration = sign_registration(&key, &t).unwrap();
        for tag in [proof::DOMAIN_TAG, instruction::DOMAIN_TAG] {
            // A registration signature does not verify under another purpose's tag.
            assert!(verify_tagged(&pk, tag, &body, &registration).is_err());
            // A signature of the same body under another tag is not a registration proof.
            let other = sign_tagged(&key, tag, &body).unwrap();
            assert!(verify_registration(&pk, &t, &other).is_err());
        }
        // The same body signed untagged does not verify either.
        let raw = key.sign(&body).unwrap();
        assert!(verify_registration(&pk, &t, &raw).is_err());
    }
}

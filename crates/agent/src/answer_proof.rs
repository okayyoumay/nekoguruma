//! The check of the server's answer proof (design 6.4, 14.4, ADR-504): whether an answer that is
//! a result carries a signature, under the instruction key the agent took at registration, that
//! binds it to the request it answers.
//!
//! A [`Verifier`] holds the key, the audience and the agent ID of the registration record; one
//! call to [`Verifier::proved`] takes what the agent sent ([`Sent`]), the answer's status and
//! bytes as received and the `Ngr-Server-Proof` header's value, and says only whether the proof
//! holds. The caller treats a result that is not proved as no answer at all and parses nothing
//! of it. No build option and no setting waives the check: the tests' fakes sign instead
//! (`TestProver`).
//!
//! Never logged: a nonce, a proof, a body or a target.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::answer::{self, AnswerTranscript, body_digest};
use shared_crypto::proof::Audience;
use shared_crypto::{PublicKey, Signature};

use crate::registration::record::{Record, RecordError};

/// What every log line about an unproven answer says about its possible causes, in fixed words
/// (`operational-log.md`).
pub(crate) const UNPROVEN_CAUSES: &str = "another process may hold the server's port, or the server's instruction key changed (register again)";

/// The text of a signature in padded base64.
const PROOF_TEXT_LEN: usize = 88;

/// A value of `N` bytes in canonical padded base64 (the text re-encodes to itself), or `None`.
pub(crate) fn canonical_base64<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes: [u8; N] = STANDARD.decode(text).ok()?.try_into().ok()?;
    (STANDARD.encode(bytes) == text).then_some(bytes)
}

/// The request an answer is checked against, as the agent sent it.
pub(crate) struct Sent<'a> {
    /// The nonce the request carried (for the session route, the body's).
    pub(crate) nonce: [u8; 32],
    pub(crate) method: &'a str,
    /// The request target: path and query.
    pub(crate) target: &'a str,
    /// The request body's bytes, empty when there is none.
    pub(crate) body: &'a [u8],
}

/// The server's instruction key and the names its proofs cover.
#[derive(Debug)]
pub(crate) struct Verifier {
    pub(crate) key: PublicKey,
    pub(crate) audience: Audience,
    pub(crate) agent_id: String,
}

impl Verifier {
    /// The verifier of the registered agent `record`: the instruction key and the origin as the
    /// control channel's start checks them.
    pub(crate) fn from_record(record: &Record) -> Result<Self, RecordError> {
        let audience = Audience::parse_canonical(&record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        let key = PublicKey::from_spki_pem(&record.instruction_key_pem)
            .map_err(|_| RecordError::Field("instruction_key_pem"))?;
        Ok(Self {
            key,
            audience,
            agent_id: record.agent_id.clone(),
        })
    }

    /// Whether `proof` is the server's proof of the answer (`status`, `answer` as received) to
    /// `sent`: exactly one value of 88 characters, canonical base64 of 64 bytes and a well-formed
    /// signature, valid for the transcript the answer and the request make. A missing proof is
    /// not proved.
    pub(crate) fn proved(
        &self,
        sent: &Sent<'_>,
        status: u16,
        answer: &[u8],
        proof: Option<&str>,
    ) -> bool {
        let Some(text) = proof.filter(|text| text.len() == PROOF_TEXT_LEN) else {
            return false;
        };
        let Some(bytes) = canonical_base64::<64>(text) else {
            return false;
        };
        let Ok(signature) = Signature::from_bytes(&bytes) else {
            return false;
        };
        let Ok(transcript) = AnswerTranscript::new(
            &self.audience,
            &self.agent_id,
            sent.nonce,
            sent.method,
            sent.target,
            body_digest(sent.body),
            status,
            body_digest(answer),
        ) else {
            return false;
        };
        answer::verify(&self.key, &transcript, &signature).is_ok()
    }
}

/// The server's side of the proof for the tests' fakes: signs what [`Verifier::proved`] checks.
#[cfg(test)]
pub(crate) struct TestProver {
    pub(crate) key: std::sync::Arc<shared_crypto::SigningKey>,
    pub(crate) audience: Audience,
    pub(crate) agent_id: String,
}

/// The nonce of a request as a fake server read it: the `Ngr-Nonce` header's value, or, on the
/// session route, which sends none, the body's.
#[cfg(test)]
pub(crate) fn nonce_of(nonce_header: Option<&str>, request_body: &[u8]) -> [u8; 32] {
    let text = match nonce_header {
        Some(text) => text.to_owned(),
        None => serde_json::from_slice::<serde_json::Value>(request_body)
            .ok()
            .and_then(|body| body["nonce"].as_str().map(str::to_owned))
            .expect("a request with a nonce"),
    };
    canonical_base64::<32>(&text).expect("a canonical nonce")
}

#[cfg(test)]
impl TestProver {
    /// A prover that signs with `key` for the agent `agent_id` of the server at `origin`.
    pub(crate) fn new(
        key: std::sync::Arc<shared_crypto::SigningKey>,
        origin: &str,
        agent_id: &str,
    ) -> Self {
        Self {
            key,
            audience: Audience::parse_canonical(origin).expect("a canonical origin"),
            agent_id: agent_id.to_owned(),
        }
    }

    /// The proof of the answer to a request as a fake server read it: the nonce is the
    /// `Ngr-Nonce` header's value, or, on the session route, which sends none, the body's.
    pub(crate) fn proof_of_request(
        &self,
        method: &str,
        target: &str,
        nonce_header: Option<&str>,
        request_body: &[u8],
        status: u16,
        answer_body: &[u8],
    ) -> String {
        let nonce = nonce_of(nonce_header, request_body);
        self.proof(nonce, method, target, request_body, status, answer_body)
    }

    /// The same prover under another key (an impostor's).
    pub(crate) fn with_key(&self, key: std::sync::Arc<shared_crypto::SigningKey>) -> Self {
        Self {
            key,
            audience: self.audience.clone(),
            agent_id: self.agent_id.clone(),
        }
    }

    /// The `Ngr-Server-Proof` value for the answer (`status`, `answer_body`) to the request
    /// (`method`, `target`, `request_body`) that carried `nonce`.
    pub(crate) fn proof(
        &self,
        nonce: [u8; 32],
        method: &str,
        target: &str,
        request_body: &[u8],
        status: u16,
        answer_body: &[u8],
    ) -> String {
        let transcript = AnswerTranscript::new(
            &self.audience,
            &self.agent_id,
            nonce,
            method,
            target,
            body_digest(request_body),
            status,
            body_digest(answer_body),
        )
        .expect("a transcript of the test's request");
        let signature = answer::sign(&*self.key, &transcript).expect("a signature");
        STANDARD.encode(signature.to_bytes())
    }
}

#[cfg(test)]
mod tests;

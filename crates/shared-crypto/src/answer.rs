//! The server's answer proof (ADR-504, design 6.4 and 14.4).
//!
//! The server proves an answer to the agent by signing a transcript that binds the answer to
//! the request it answers: the server the agent pinned, the agent, a fresh nonce the agent sent
//! with the request, the request's method, target and body digest, and the answer's status and
//! body digest. The transcript has no timestamp, because the device clock is untrusted
//! (design 16.3); the agent's nonce makes the answer belong to one attempt, and the method,
//! target and request digest make it belong to one request. The signing key is the server's
//! instruction key, which the agent took at registration after checking its fingerprint
//! (design 6.7).
//!
//! # Encoding and domain separation
//!
//! The transcript body is the protocol version ([`PROTOCOL_VERSION`], 4 bytes big-endian,
//! always written by this crate). The audience, the agent ID, the method and the request
//! target are each a 4-byte big-endian length, then bytes; the nonce, the SHA-256 of the
//! request body and the SHA-256 of the answer body are 32 bytes each, and the status is 2 bytes
//! big-endian. The order is: version, audience, agent ID, nonce, method,
//! target, request digest, status, answer digest. Variable-length fields are length-prefixed,
//! so two different field sets never encode to the same bytes. The audience is an
//! [`Audience`], so it is the same canonical text the connection proof signs.
//!
//! The body is signed through [`sign_tagged`] under [`DOMAIN_TAG`], which names this purpose.
//! The connection proof, the job instruction and the two registration purposes sign under tags
//! of their own, and a verifier rebuilds its message from its own tag, so an answer proof does
//! not verify as any of them, and none verifies as an answer proof. The version lives only in
//! the body, so a layout change bumps [`PROTOCOL_VERSION`] and the tag stays.

use sha2::{Digest, Sha256};

use crate::proof::{Audience, TranscriptError, check_agent_id, put_prefixed};
use crate::{PublicKey, SignError, Signature, Signer, VerifyError, sign_tagged, verify_tagged};

/// The tag that names this purpose in every signed transcript.
pub const DOMAIN_TAG: &[u8] = b"nekoguruma server answer proof";

/// The transcript layout version this crate writes into every transcript.
pub const PROTOCOL_VERSION: u32 = 1;

/// Length of the agent's nonce and of each body digest.
pub const VALUE_LEN: usize = 32;

/// Longest request method accepted, in bytes.
pub const MAX_METHOD_LEN: usize = 16;

/// Longest request target accepted, in bytes.
pub const MAX_TARGET_LEN: usize = 2048;

/// The SHA-256 of a body, the form the transcript takes both digests in.
pub fn body_digest(body: &[u8]) -> [u8; VALUE_LEN] {
    Sha256::digest(body).into()
}

/// What a server answer proof covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerTranscript {
    audience: String,
    agent_id: String,
    nonce: [u8; VALUE_LEN],
    method: String,
    target: String,
    request_digest: [u8; VALUE_LEN],
    status: u16,
    answer_digest: [u8; VALUE_LEN],
}

/// The rule for a request target in a transcript: 1 to [`MAX_TARGET_LEN`] visible ASCII bytes,
/// beginning with `/`. The target is the path and query as the HTTP parser gives it (an
/// origin-form target byte for byte, an absolute-form target's path and query; no fragment).
/// The server calls it where a route judges the
/// request's shape, so that a request whose answer cannot be proved is refused before anything
/// changes (ADR-504); [`AnswerTranscript::new`] applies the same rule.
pub fn check_target(target: &str) -> Result<(), TranscriptError> {
    if !target.starts_with('/')
        || target.len() > MAX_TARGET_LEN
        || !target.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(TranscriptError::BadField { field: "target" });
    }
    Ok(())
}

impl AnswerTranscript {
    /// Builds a transcript. Both sides build it this way: the server from the request it
    /// served and the answer it is about to send, the agent from the request it made and the
    /// answer it received. `method` is an HTTP method token (1 to [`MAX_METHOD_LEN`] ASCII
    /// token characters), `target` the request target as sent (1 to [`MAX_TARGET_LEN`] visible
    /// ASCII bytes, path and query), and the digests are [`body_digest`] of each body after any
    /// transfer decoding; an absent body (a GET, an upgrade, the 101) is the digest of zero
    /// bytes. The answer's headers are not bound.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per transcript field, in encoding order"
    )]
    pub fn new(
        audience: &Audience,
        agent_id: &str,
        nonce: [u8; VALUE_LEN],
        method: &str,
        target: &str,
        request_digest: [u8; VALUE_LEN],
        status: u16,
        answer_digest: [u8; VALUE_LEN],
    ) -> Result<Self, TranscriptError> {
        check_agent_id(agent_id)?;
        if method.is_empty() || method.len() > MAX_METHOD_LEN || !method.bytes().all(is_token_char)
        {
            return Err(TranscriptError::BadField { field: "method" });
        }
        check_target(target)?;
        Ok(Self {
            audience: audience.as_str().to_owned(),
            agent_id: agent_id.to_owned(),
            nonce,
            method: method.to_owned(),
            target: target.to_owned(),
            request_digest,
            status,
            answer_digest,
        })
    }

    /// The body that is signed under [`DOMAIN_TAG`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            4 + 4 * 4
                + self.audience.len()
                + self.agent_id.len()
                + self.method.len()
                + self.target.len()
                + 3 * VALUE_LEN
                + 2,
        );
        out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        put_prefixed(&mut out, self.audience.as_bytes());
        put_prefixed(&mut out, self.agent_id.as_bytes());
        out.extend_from_slice(&self.nonce);
        put_prefixed(&mut out, self.method.as_bytes());
        put_prefixed(&mut out, self.target.as_bytes());
        out.extend_from_slice(&self.request_digest);
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.answer_digest);
        out
    }
}

/// An HTTP token character, the set a method is written in.
fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Signs `transcript` with the server's instruction key.
pub fn sign<S: Signer + ?Sized>(
    signer: &S,
    transcript: &AnswerTranscript,
) -> Result<Signature, SignError> {
    sign_tagged(signer, DOMAIN_TAG, &transcript.encode())
}

/// Verifies an answer proof against the server's instruction key.
pub fn verify(
    key: &PublicKey,
    transcript: &AnswerTranscript,
    signature: &Signature,
) -> Result<(), VerifyError> {
    verify_tagged(key, DOMAIN_TAG, &transcript.encode(), signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::MAX_FIELD_LEN;
    use crate::{SigningKey, instruction, proof, registration};

    const AGENT: &str = "3f1c2a52-6a0e-4c0d-9d3e-0a1b2c3d4e5f";

    /// A nonce made fresh for the test run, as the agent makes one per request; the same value
    /// for every call within the run, so two transcripts built from it compare equal.
    fn nonce() -> [u8; 32] {
        static NONCE: std::sync::LazyLock<[u8; 32]> =
            std::sync::LazyLock::new(|| crate::random_32().unwrap());
        *NONCE
    }

    /// Another fresh nonce, different from [`nonce`].
    fn other_nonce() -> [u8; 32] {
        loop {
            let n = crate::random_32().unwrap();
            if n != nonce() {
                return n;
            }
        }
    }

    fn aud(text: &str) -> Audience {
        Audience::canonicalise(text).unwrap()
    }

    fn base() -> AnswerTranscript {
        AnswerTranscript::new(
            &aud("http://127.0.0.1:8080"),
            AGENT,
            nonce(),
            "POST",
            "/api/v1/agent/session",
            [2; 32],
            200,
            [3; 32],
        )
        .unwrap()
    }

    /// Every field changed on its own, from `base`.
    fn variants() -> Vec<AnswerTranscript> {
        let a = aud("http://127.0.0.1:8080");
        let n = |aud: &Audience, id, nonce, m, t, rd, st, ad| {
            AnswerTranscript::new(aud, id, nonce, m, t, rd, st, ad).unwrap()
        };
        let t = "/api/v1/agent/session";
        vec![
            n(
                &aud("http://127.0.0.1:8081"),
                AGENT,
                nonce(),
                "POST",
                t,
                [2; 32],
                200,
                [3; 32],
            ),
            n(&a, "other", nonce(), "POST", t, [2; 32], 200, [3; 32]),
            n(&a, AGENT, other_nonce(), "POST", t, [2; 32], 200, [3; 32]),
            n(&a, AGENT, nonce(), "GET", t, [2; 32], 200, [3; 32]),
            n(
                &a,
                AGENT,
                nonce(),
                "POST",
                "/api/v1/agent/uploads",
                [2; 32],
                200,
                [3; 32],
            ),
            n(&a, AGENT, nonce(), "POST", t, [9; 32], 200, [3; 32]),
            n(&a, AGENT, nonce(), "POST", t, [2; 32], 409, [3; 32]),
            n(&a, AGENT, nonce(), "POST", t, [2; 32], 200, [9; 32]),
        ]
    }

    #[test]
    fn an_answer_proof_verifies() {
        let key = SigningKey::generate().unwrap();
        let sig = sign(&key, &base()).unwrap();
        verify(&key.public_key(), &base(), &sig).unwrap();
    }

    #[test]
    fn encoding_is_the_documented_layout() {
        let t = AnswerTranscript::new(
            &aud("http://ab"),
            "c",
            nonce(),
            "GET",
            "/x?y=1",
            [2; 32],
            0x0194,
            [3; 32],
        )
        .unwrap();
        let mut want = Vec::new();
        want.extend(PROTOCOL_VERSION.to_be_bytes());
        want.extend(9u32.to_be_bytes());
        want.extend(b"http://ab");
        want.extend(1u32.to_be_bytes());
        want.extend(b"c");
        want.extend(nonce());
        want.extend(3u32.to_be_bytes());
        want.extend(b"GET");
        want.extend(6u32.to_be_bytes());
        want.extend(b"/x?y=1");
        want.extend([2u8; 32]);
        want.extend([0x01, 0x94]);
        want.extend([3u8; 32]);
        assert_eq!(t.encode(), want);
    }

    #[test]
    fn every_changed_field_fails() {
        let key = SigningKey::generate().unwrap();
        let sig = sign(&key, &base()).unwrap();
        let pk = key.public_key();
        let all = variants();
        assert_eq!(all.len(), 8);
        for t in all {
            assert!(
                matches!(verify(&pk, &t, &sig), Err(VerifyError::Mismatch { .. })),
                "{t:?}"
            );
        }
    }

    #[test]
    fn a_changed_version_fails() {
        let key = SigningKey::generate().unwrap();
        let mut body = base().encode();
        body[..4].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
        let sig = sign_tagged(&key, DOMAIN_TAG, &body).unwrap();
        assert!(verify(&key.public_key(), &base(), &sig).is_err());
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        let a = aud("http://ab");
        let mk = |id: &str, m: &str, t: &str| {
            AnswerTranscript::new(&a, id, nonce(), m, t, [0; 32], 200, [0; 32]).unwrap()
        };
        assert_ne!(
            mk("ab", "GET", "/c").encode(),
            mk("a", "GET", "/bc").encode()
        );
        assert_ne!(
            mk("a", "GET", "/c").encode(),
            mk("a", "GETx", "/c").encode()
        );
    }

    #[test]
    fn the_body_digest_is_sha256() {
        let d = body_digest(b"abc");
        assert_eq!(d[0], 0xba);
        assert_eq!(d[31], 0xad);
        assert_eq!(body_digest(b""), <[u8; 32]>::from(Sha256::digest(b"")));
    }

    #[test]
    fn bounds_are_refused() {
        let a = aud("http://127.0.0.1:8080");
        let mk = |id: &str, m: &str, t: &str| {
            AnswerTranscript::new(&a, id, nonce(), m, t, [0; 32], 200, [0; 32])
        };
        let field = |f| Err(TranscriptError::BadField { field: f });
        assert_eq!(mk("", "GET", "/"), field("agent_id"));
        assert_eq!(
            mk(&"x".repeat(MAX_FIELD_LEN + 1), "GET", "/"),
            field("agent_id")
        );
        mk(&"x".repeat(MAX_FIELD_LEN), "GET", "/").unwrap();
        assert_eq!(mk(AGENT, "", "/"), field("method"));
        assert_eq!(mk(AGENT, "GE T", "/"), field("method"));
        assert_eq!(mk(AGENT, "G\u{e9}T", "/"), field("method"));
        assert_eq!(
            mk(AGENT, &"G".repeat(MAX_METHOD_LEN + 1), "/"),
            field("method")
        );
        mk(AGENT, &"G".repeat(MAX_METHOD_LEN), "/").unwrap();
        assert_eq!(mk(AGENT, "GET", ""), field("target"));
        assert_eq!(mk(AGENT, "GET", "/a b"), field("target"));
        assert_eq!(mk(AGENT, "GET", "/a\r\nb"), field("target"));
        assert_eq!(mk(AGENT, "GET", "/\u{e9}"), field("target"));
        let long = format!("/{}", "a".repeat(MAX_TARGET_LEN));
        assert_eq!(mk(AGENT, "GET", &long), field("target"));
        mk(AGENT, "GET", &long[..MAX_TARGET_LEN]).unwrap();
    }

    #[test]
    fn another_key_fails() {
        let key = SigningKey::generate().unwrap();
        let sig = sign(&key, &base()).unwrap();
        let other = SigningKey::generate().unwrap().public_key();
        assert!(matches!(
            verify(&other, &base(), &sig),
            Err(VerifyError::Mismatch { .. })
        ));
    }

    #[test]
    fn the_tags_are_pairwise_distinct_and_signatures_do_not_cross() {
        let tags: [&[u8]; 5] = [
            DOMAIN_TAG,
            proof::DOMAIN_TAG,
            instruction::DOMAIN_TAG,
            registration::DOMAIN_TAG,
            registration::RESPONSE_DOMAIN_TAG,
        ];
        for (i, a) in tags.iter().enumerate() {
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
        let key = SigningKey::generate().unwrap();
        let pk = key.public_key();
        let t = base();
        let proof_sig = sign(&key, &t).unwrap();
        for tag in &tags[1..] {
            assert!(verify_tagged(&pk, tag, &t.encode(), &proof_sig).is_err());
            let other = sign_tagged(&key, tag, &t.encode()).unwrap();
            assert!(verify(&pk, &t, &other).is_err());
        }
        // The same body signed untagged does not verify either.
        let raw = key.sign(&t.encode()).unwrap();
        assert!(verify(&pk, &t, &raw).is_err());
    }

    #[test]
    fn check_target_takes_visible_ascii_within_the_bound_only() {
        check_target("/api/v1/agent/uploads/0f/parts/3").unwrap();
        check_target(&format!("/{}", "a".repeat(MAX_TARGET_LEN - 1))).unwrap();
        for bad in [
            String::new(),
            "api/v1/x".to_owned(),
            "http://127.0.0.1:1/x".to_owned(),
            format!("/{}", "a".repeat(MAX_TARGET_LEN)),
            "/api/v1/agent/uploads?\u{e9}".to_owned(),
            "/a b".to_owned(),
            "/a\tb".to_owned(),
        ] {
            assert_eq!(
                check_target(&bad),
                Err(TranscriptError::BadField { field: "target" })
            );
        }
    }
}

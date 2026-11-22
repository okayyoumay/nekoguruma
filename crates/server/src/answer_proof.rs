//! The server's answer proof (ADR-504, design 6.4 and 14.4): what a route needs to sign an
//! answer under the instruction key and send the signature in `Ngr-Server-Proof`.
//!
//! One [`Prover`] is built per router and shared by the routes that prove: it holds the key, the
//! audience (the pinned origin of the listener's port, never a value from a request) and the log
//! gate of a failed proof. A route reads what the request asked ([`Asked`], from the
//! `Ngr-Nonce` header, or from the body's nonce on the session route), judges it where it
//! judges the request's shape, and signs once its answer is final ([`Prover::header`], or
//! [`Prover::prove`] for a response already built). A proof that cannot be made is an unproved
//! 503 ([`Unprovable`]), never the promised status.
//!
//! Never logged: a nonce, a signature, the request target or a body.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::answer::{self, AnswerTranscript, VALUE_LEN, body_digest};
use shared_crypto::proof::Audience;
use shared_proto::{AgentId, NONCE_HEADER, PROOF_HEADER};

use crate::health::may_log;
use crate::instruction_key::InstructionKey;
use crate::local_guard::{BAD_TARGET, pinned_origin};
use crate::problem::Problem;
use crate::session::{decode_fixed, now};

/// The longest answer [`Prover::prove`] collects to sign, in bytes. The proved routes' answers
/// are small documents.
const MAX_ANSWER_LEN: usize = 64 * 1024;
/// A failed proof is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// The length of a nonce's base64 text.
const NONCE_TEXT: usize = 44;

/// Why a request's proof cannot be asked for: each is a 400 that consumes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskError {
    /// More than one `Ngr-Nonce` header, or one that is not the canonical padded base64 of 32
    /// bytes.
    Nonce,
    /// A request target the transcript cannot take (`answer::check_target`).
    Target,
}

impl AskError {
    fn title(self) -> &'static str {
        match self {
            AskError::Nonce => "Invalid Ngr-Nonce header",
            AskError::Target => BAD_TARGET,
        }
    }
}

impl IntoResponse for AskError {
    fn into_response(self) -> Response {
        Problem::new(StatusCode::BAD_REQUEST, self.title()).into_response()
    }
}

/// What a request asked for: an answer proved to this nonce, bound to this method and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    nonce: [u8; VALUE_LEN],
    method: String,
    target: String,
}

impl Asked {
    /// The request's `Ngr-Nonce` header: `None` when there is none (the route is served as
    /// before), an error for two headers, a value that is not canonical base64 of 32 bytes or a
    /// target the transcript cannot take. A request with no nonce is not judged by its target.
    pub(crate) fn from_header(parts: &Parts) -> Result<Option<Asked>, AskError> {
        let mut values = parts.headers.get_all(NONCE_HEADER).iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(AskError::Nonce);
        }
        let text = value.to_str().map_err(|_| AskError::Nonce)?;
        let nonce = decode_fixed(text, NONCE_TEXT).ok_or(AskError::Nonce)?;
        Asked::with_nonce(parts, nonce).map(Some)
    }

    /// The request with `nonce` taken from elsewhere: the session route's body.
    pub(crate) fn with_nonce(parts: &Parts, nonce: [u8; VALUE_LEN]) -> Result<Asked, AskError> {
        // The target as the parser gives it (it takes no byte outside ASCII, and the rule the
        // transcript applies refuses whatever else it cannot carry).
        let target = parts.uri.path_and_query().ok_or(AskError::Target)?.as_str();
        if answer::check_target(target).is_err() {
            return Err(AskError::Target);
        }
        Ok(Asked {
            nonce,
            method: parts.method.as_str().to_owned(),
            target: target.to_owned(),
        })
    }
}

/// A proof that could not be made. It answers as an unproved 503.
#[derive(Debug)]
pub(crate) struct Unprovable;

impl IntoResponse for Unprovable {
    fn into_response(self) -> Response {
        Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The answer could not be proved",
        )
        .into_response()
    }
}

/// Signs the answers of one router (ADR-504).
pub(crate) struct Prover {
    key: Arc<InstructionKey>,
    audience: Audience,
    gate: Mutex<Option<std::time::Instant>>,
}

impl Prover {
    /// `port` is the listener's bound port: the audience is the pinned origin for it. A port the
    /// pinned origin cannot be canonical for (0) is a caller's fault.
    pub(crate) fn new(key: Arc<InstructionKey>, port: u16) -> Prover {
        let audience = Audience::parse_canonical(&pinned_origin(port))
            .expect("the pinned origin of a bound port is a canonical audience");
        Prover {
            key,
            audience,
            gate: Mutex::new(None),
        }
    }

    /// The audience of every proof and of the connection proof's check.
    pub(crate) fn audience(&self) -> &Audience {
        &self.audience
    }

    /// The `Ngr-Server-Proof` value for an answer of `status` with the body `answer_bytes`, to
    /// the request `asked` describes whose body digest is `request_digest`, for `agent`. A
    /// failure is logged once a minute, without the target, and is an [`Unprovable`].
    pub(crate) async fn header(
        &self,
        asked: &Asked,
        agent: &AgentId,
        request_digest: [u8; VALUE_LEN],
        status: u16,
        answer_bytes: &[u8],
    ) -> Result<HeaderValue, Unprovable> {
        let transcript = AnswerTranscript::new(
            &self.audience,
            &agent.0,
            asked.nonce,
            &asked.method,
            &asked.target,
            request_digest,
            status,
            body_digest(answer_bytes),
        )
        .map_err(|_| self.failed("the transcript could not be built"))?;
        let signature = self
            .key
            .prove_answer(&transcript)
            .await
            .map_err(|_| self.failed("the signature could not be made"))?;
        Ok(
            HeaderValue::from_str(&STANDARD.encode(signature.to_bytes()))
                .expect("base64 is a header value"),
        )
    }

    /// `response` with its proof, when the request asked for one: the body is collected (at most
    /// [`MAX_ANSWER_LEN`] bytes), signed and put back unchanged. With `None` the response is
    /// returned as it is. A failure answers as [`Unprovable`].
    #[expect(
        dead_code,
        reason = "the data routes (next step) prove their answers through it"
    )]
    pub(crate) async fn prove(
        &self,
        asked: Option<&Asked>,
        agent: &AgentId,
        request_digest: [u8; VALUE_LEN],
        response: Response,
    ) -> Response {
        let Some(asked) = asked else {
            return response;
        };
        let (mut parts, body) = response.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, MAX_ANSWER_LEN).await else {
            return self.failed("the answer could not be read").into_response();
        };
        match self
            .header(asked, agent, request_digest, parts.status.as_u16(), &bytes)
            .await
        {
            Ok(value) => {
                Prover::attach(&mut parts.headers, value);
                Response::from_parts(parts, Body::from(bytes))
            }
            Err(unprovable) => unprovable.into_response(),
        }
    }

    /// Puts a proof from [`Prover::header`] on a response's headers.
    pub(crate) fn attach(headers: &mut HeaderMap, value: HeaderValue) {
        headers.insert(HeaderName::from_static(PROOF_HEADER), value);
    }

    /// Logs a failed proof through the one gate and says it as a value.
    fn failed(&self, what: &'static str) -> Unprovable {
        if may_log(&self.gate, now(), LOG_EVERY) {
            tracing::error!(what, "an answer could not be proved");
        }
        Unprovable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use shared_crypto::{Signer, SigningKey};

    fn parts(method: &str, target: &str, nonces: &[&str]) -> Parts {
        let mut request = Request::builder().method(method).uri(target);
        for nonce in nonces {
            request = request.header(NONCE_HEADER, *nonce);
        }
        request.body(()).unwrap().into_parts().0
    }

    fn fresh() -> [u8; 32] {
        shared_crypto::random_32().unwrap()
    }

    fn nonce_text(nonce: [u8; 32]) -> String {
        STANDARD.encode(nonce)
    }

    #[test]
    fn no_header_is_no_ask_and_one_good_header_is_one() {
        assert_eq!(Asked::from_header(&parts("GET", "/x", &[])).unwrap(), None);
        // Without a nonce the target is not judged.
        assert_eq!(
            Asked::from_header(&parts("GET", "/x?\u{e9}", &[])).unwrap(),
            None
        );
        let nonce = fresh();
        let asked = Asked::from_header(&parts("POST", "/a?b=1", &[&nonce_text(nonce)]))
            .unwrap()
            .unwrap();
        assert_eq!(asked.nonce, nonce);
        assert_eq!(asked.method, "POST");
        assert_eq!(asked.target, "/a?b=1");
    }

    #[test]
    fn a_malformed_or_repeated_nonce_is_an_error() {
        let good = nonce_text(fresh());
        let unpadded = good.trim_end_matches('=').to_owned();
        let short = STANDARD.encode(&fresh()[..31]);
        let long = STANDARD.encode([&fresh()[..], &[0]].concat());
        let spaced = format!("{} {}", &good[..20], &good[21..]);
        // The last data character of 32 bytes carries two padding bits, clear in the canonical
        // text. Setting one gives a text a lenient decoder takes as the same bytes.
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let last = good.as_bytes()[42];
        let value = alphabet.iter().position(|c| *c == last).unwrap();
        assert_eq!(value & 3, 0);
        let loose = format!("{}{}=", &good[..42], alphabet[value | 1] as char);
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode([0xfbu8; 32]);
        for (name, nonces) in [
            ("empty", vec![""]),
            ("unpadded", vec![unpadded.as_str()]),
            ("short", vec![short.as_str()]),
            ("long", vec![long.as_str()]),
            ("url safe", vec![url_safe.as_str()]),
            ("space inside", vec![spaced.as_str()]),
            ("non-zero padding bits", vec![loose.as_str()]),
            ("two values", vec![good.as_str(), good.as_str()]),
        ] {
            assert_eq!(
                Asked::from_header(&parts("GET", "/x", &nonces)),
                Err(AskError::Nonce),
                "{name}"
            );
        }
    }

    #[test]
    fn a_target_the_transcript_cannot_take_is_an_error_after_the_nonce() {
        let good = nonce_text(fresh());
        let long = format!("/{}", "a".repeat(answer::MAX_TARGET_LEN));
        assert_eq!(
            Asked::from_header(&parts("GET", &long, &[&good])),
            Err(AskError::Target)
        );
        // A malformed nonce is the nonce's error even when the target is bad too.
        assert_eq!(
            Asked::from_header(&parts("GET", &long, &["x"])),
            Err(AskError::Nonce)
        );
    }

    #[tokio::test]
    async fn a_header_verifies_as_the_transcript_of_the_request_and_the_answer() {
        let key = SigningKey::generate().unwrap();
        let public = key.public_key();
        let prover = Prover::new(Arc::new(InstructionKey::from_signer(Arc::new(key))), 8080);
        let agent = AgentId("3f1c2a52-6a0e-4c0d-9d3e-0a1b2c3d4e5f".to_owned());
        let nonce = fresh();
        let asked = Asked::with_nonce(&parts("POST", "/p?q", &[]), nonce).unwrap();
        let value = prover
            .header(&asked, &agent, body_digest(b"req"), 201, b"answer")
            .await
            .unwrap();
        let signature =
            shared_crypto::Signature::from_bytes(&STANDARD.decode(value.as_bytes()).unwrap())
                .unwrap();
        let transcript = |status, answer: &[u8]| {
            AnswerTranscript::new(
                prover.audience(),
                &agent.0,
                nonce,
                "POST",
                "/p?q",
                body_digest(b"req"),
                status,
                body_digest(answer),
            )
            .unwrap()
        };
        answer::verify(&public, &transcript(201, b"answer"), &signature).unwrap();
        assert!(answer::verify(&public, &transcript(200, b"answer"), &signature).is_err());
        assert!(answer::verify(&public, &transcript(201, b"other"), &signature).is_err());
    }

    #[tokio::test]
    async fn an_agent_id_the_transcript_refuses_is_unprovable_and_gated_once() {
        let key = SigningKey::generate().unwrap();
        let prover = Prover::new(Arc::new(InstructionKey::from_signer(Arc::new(key))), 8080);
        let asked = Asked::with_nonce(&parts("GET", "/x", &[]), fresh()).unwrap();
        let bad = AgentId(String::new());
        for _ in 0..2 {
            assert!(
                prover
                    .header(&asked, &bad, [0; 32], 200, b"")
                    .await
                    .is_err()
            );
        }
        let response = Unprovable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

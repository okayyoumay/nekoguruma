//! The exchange that comes before every upgrade (design 6.4, 14.4, ADR-466, ADR-467, ADR-471):
//! a challenge from `POST /api/v1/agent/challenge`, then a token from `POST
//! /api/v1/agent/session` for a proof over the challenge, a nonce the agent chooses, the record's
//! audience and the agent ID. A token is used for one upgrade and never kept; a challenge is
//! presented once.
//!
//! Every outcome that is not a token is a [`Fail`], which says how the loop backs off. Nothing
//! here logs the challenge, the nonce, the proof, the token or a body: only fixed words and the
//! status number.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use shared_crypto::proof::{ProofTranscript, sign_proof};
use tokio::time::Instant;

use super::dial::{self, ConnectError};
use super::{Warnings, Wiring};
use crate::server_http::{self, PostError, PostRequest, Reply, ReplyBody, Timeouts};

/// The challenge route (`api/openapi.yaml`).
pub(super) const CHALLENGE_PATH: &str = "/api/v1/agent/challenge";
/// The session route.
pub(super) const SESSION_PATH: &str = "/api/v1/agent/session";

/// The text of a 32-byte value in padded base64.
const VALUE_TEXT_LEN: usize = 44;

/// A session token: used for one upgrade, then dropped. Its text is never shown.
pub(super) struct Token(String);

impl Token {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Why the exchange gave no token, and how the loop backs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fail {
    /// The ordinary failed attempt: wait the delay.
    Ord,
    /// The challenge route asked for a wait of this many seconds (B3).
    RetryAfter(u32),
    /// The session route refused the agent (403, 429): back off to the cap.
    Cap,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeBody {
    challenge: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionBody {
    token: String,
    expires_in_secs: u64,
}

/// A 32-byte value in canonical padded base64, or `None`.
fn value_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != VALUE_TEXT_LEN {
        return None;
    }
    let bytes: [u8; 32] = STANDARD.decode(text).ok()?.try_into().ok()?;
    (STANDARD.encode(bytes) == text).then_some(bytes)
}

/// One POST: the connect within its bound, the whole call within the total (`server_http`).
async fn call(w: &Wiring, path: &str, body: Vec<u8>) -> Result<Reply, PostError> {
    let (connect_by, deadline) = server_http::deadlines(Instant::now(), Timeouts::default());
    let stream = match dial::connect(&*w.dial, connect_by).await {
        Ok(stream) => stream,
        Err(ConnectError::Timeout) => return Err(PostError::ConnectTimeout),
        Err(ConnectError::Failed) => {
            return Err(PostError::NotSent(std::io::ErrorKind::ConnectionRefused));
        }
    };
    let request = PostRequest {
        port: w.port,
        host: &w.host,
        path,
        body,
    };
    server_http::post_over(stream, &request, 200, deadline).await
}

/// The 200 body when the answer is JSON and within the limit.
fn json_body(reply: &Reply) -> Option<&[u8]> {
    match (&reply.body, reply.json_content_type) {
        (Some(ReplyBody::Bytes(bytes)), true) => Some(bytes),
        _ => None,
    }
}

/// Step B: the challenge.
async fn challenge(w: &Wiring, warn: &mut Warnings) -> Result<[u8; 32], Fail> {
    let reply = match call(w, CHALLENGE_PATH, Vec::new()).await {
        Ok(reply) => reply,
        Err(_) => {
            warn.failure("challenge-transport", None);
            return Err(Fail::Ord);
        }
    };
    match reply.status {
        200 => {
            let value = json_body(&reply)
                .and_then(|body| serde_json::from_slice::<ChallengeBody>(body).ok())
                .and_then(|body| value_32(&body.challenge));
            value.ok_or_else(|| {
                warn.failure("challenge-answer", Some(200));
                Fail::Ord
            })
        }
        429 => match server_http::retry_after_secs(reply.retry_after.as_deref()) {
            Some(secs) => {
                warn.failure("challenge-limited", Some(429));
                Err(Fail::RetryAfter(secs))
            }
            None => {
                warn.failure("challenge-answer", Some(429));
                Err(Fail::Ord)
            }
        },
        status => {
            warn.failure("challenge-answer", Some(status));
            Err(Fail::Ord)
        }
    }
}

/// A token from a 200 answer, or `None` when the answer is not exactly `{token, expiresInSecs}`.
fn session_token(reply: &Reply) -> Option<Token> {
    let body: SessionBody = serde_json::from_slice(json_body(reply)?).ok()?;
    value_32(&body.token)?;
    (body.expires_in_secs >= 1).then_some(Token(body.token))
}

/// Steps B and C: the token for one upgrade. A 401 gets one new challenge (C3).
pub(super) async fn token(w: &Wiring, warn: &mut Warnings) -> Result<Token, Fail> {
    let mut retried = false;
    loop {
        let challenge = challenge(w, warn).await?;
        let Ok(nonce) = (w.random)() else {
            warn.failure("random", None);
            return Err(Fail::Ord);
        };
        let Ok(transcript) =
            ProofTranscript::new(&w.audience, &w.record.agent_id, challenge, nonce)
        else {
            warn.failure("transcript", None);
            return Err(Fail::Ord);
        };
        let Ok(proof) = sign_proof(&*w.signer, &transcript) else {
            warn.failure("signing", None);
            return Err(Fail::Ord);
        };
        let body = serde_json::json!({
            "agentId": w.record.agent_id,
            "challenge": STANDARD.encode(challenge),
            "nonce": STANDARD.encode(nonce),
            "proof": STANDARD.encode(proof.to_bytes()),
        })
        .to_string()
        .into_bytes();
        let reply = match call(w, SESSION_PATH, body).await {
            Ok(reply) => reply,
            Err(_) => {
                warn.failure("session-transport", None);
                return Err(Fail::Ord);
            }
        };
        match reply.status {
            200 => {
                return match session_token(&reply) {
                    Some(token) => {
                        warn.accepted();
                        Ok(token)
                    }
                    None => {
                        warn.failure("session-answer", Some(200));
                        Err(Fail::Ord)
                    }
                };
            }
            401 if !retried => {
                retried = true;
                tracing::debug!("control channel challenge was not live; asking for a new one");
            }
            401 => {
                warn.failure("session-unauthorized", Some(401));
                return Err(Fail::Ord);
            }
            status @ (403 | 429) => {
                warn.refusal("session-refused", status);
                return Err(Fail::Cap);
            }
            400 => {
                warn.failure("session-bad-request", Some(400));
                return Err(Fail::Ord);
            }
            status => {
                warn.failure("session-answer", Some(status));
                return Err(Fail::Ord);
            }
        }
    }
}

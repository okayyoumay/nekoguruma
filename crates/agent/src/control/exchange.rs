//! The exchange that comes before every upgrade (design 6.4, 14.4, ADR-466, ADR-467, ADR-471):
//! a challenge from `POST /api/v1/agent/challenge`, then a token from `POST
//! /api/v1/agent/session` for a proof over the challenge, a nonce the agent chooses, the record's
//! audience and the agent ID. A token is used for one upgrade and never kept; a challenge is
//! presented once.
//!
//! The session route's 200 is a token only with the server's answer proof (ADR-504): it is
//! checked against the request's exact bytes before the body is parsed, and a 200 without a valid
//! one is `session-unproven` and backs off to the cap.
//!
//! Every outcome that is not a token is a [`Fail`], which says how the loop backs off. Nothing
//! here logs the challenge, the nonce, the proof, the token or a body: only fixed words and the
//! status number.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use shared_crypto::proof::{ProofTranscript, sign_proof};
use tokio::time::Instant;

use shared_crypto::{RandomError, Signer};
use shared_proto::SESSION_RESULTS;

use super::Warnings;
use super::dial::{self, ConnectError, Dial};
use crate::answer_proof::{Sent, Verifier, canonical_base64};
use crate::server_http::{self, PostError, PostRequest, Reply, ReplyBody, Timeouts};

/// The challenge route (`api/openapi.yaml`).
pub(crate) const CHALLENGE_PATH: &str = "/api/v1/agent/challenge";
/// The session route.
pub(crate) const SESSION_PATH: &str = "/api/v1/agent/session";

/// What the exchange needs and nothing more: the dial, the key, the random source and the
/// verifier, which holds the names the proof covers and the key the server's answer is checked
/// with. The control channel builds it from its wiring and the uploader from its own (ADR-493
/// item 10), each with its own [`Log`].
pub(crate) struct Seams<'a> {
    pub(crate) dial: &'a dyn Dial,
    pub(crate) signer: &'a (dyn Signer + Send + Sync),
    pub(crate) random: fn() -> Result<[u8; 32], RandomError>,
    pub(crate) verifier: &'a Verifier,
    /// The `Host` header's value.
    pub(crate) host: &'a str,
}

/// Where the exchange reports what went wrong, in the caller's own words. The classes are fixed
/// words and the status a number: no implementation is given anything else.
pub(crate) trait Log: Send {
    /// A failed attempt of this class.
    fn failure(&mut self, class: &'static str, status: Option<u16>);
    /// A refusal of this class: the server will not have this agent for now.
    fn refusal(&mut self, class: &'static str, status: u16);
    /// An answer that must carry the server's proof did not (ADR-504): treated as no answer.
    fn unproven(&mut self, class: &'static str, status: u16);
    /// A token was issued.
    fn accepted(&mut self);
    /// The challenge was not live (a 401) and a new one is asked for (debug level).
    fn stale_challenge(&mut self);
}

/// The control channel's texts.
impl Log for Warnings {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        Warnings::failure(self, class, status);
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        Warnings::refusal(self, class, status);
    }

    fn unproven(&mut self, class: &'static str, status: u16) {
        Warnings::unproven(self, class, status);
    }

    fn accepted(&mut self) {
        Warnings::accepted(self);
    }

    fn stale_challenge(&mut self) {
        tracing::debug!("control channel challenge was not live; asking for a new one");
    }
}

/// A session token: used for one upgrade, then dropped. Its text is never shown.
pub(crate) struct Token(String);

impl Token {
    pub(crate) fn as_str(&self) -> &str {
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
pub(crate) enum Fail {
    /// The ordinary failed attempt: wait the delay.
    Ord,
    /// The challenge route asked for a wait of this many seconds (B3).
    RetryAfter(u32),
    /// The session route refused the agent (403, 429), or an answer that had to be proved was
    /// not: back off to the cap.
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
    canonical_base64(text)
}

/// One POST: the connect within its bound, the whole call within the total (`server_http`).
/// The body is read when the status is one of `body_statuses`.
async fn call(
    w: &Seams<'_>,
    path: &str,
    body: &[u8],
    body_statuses: &[u16],
) -> Result<Reply, PostError> {
    let (connect_by, deadline) = server_http::deadlines(Instant::now(), Timeouts::default());
    let stream = match dial::connect(w.dial, connect_by).await {
        Ok(stream) => stream,
        Err(ConnectError::Timeout) => return Err(PostError::ConnectTimeout),
        Err(ConnectError::Failed) => {
            return Err(PostError::NotSent(std::io::ErrorKind::ConnectionRefused));
        }
    };
    let request = PostRequest {
        // The stream is already connected: the port is not used (`post_over`).
        port: 0,
        host: w.host,
        path,
        body: body.to_vec(),
    };
    server_http::post_over(stream, &request, body_statuses, deadline).await
}

/// The 200 body when the answer is JSON and within the limit.
fn json_body(reply: &Reply) -> Option<&[u8]> {
    match (&reply.body, reply.json_content_type) {
        (Some(ReplyBody::Bytes(bytes)), true) => Some(bytes),
        _ => None,
    }
}

/// Step B: the challenge.
async fn challenge(w: &Seams<'_>, warn: &mut dyn Log) -> Result<[u8; 32], Fail> {
    let reply = match call(w, CHALLENGE_PATH, &[], &[200]).await {
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
pub(crate) async fn token(w: &Seams<'_>, warn: &mut dyn Log) -> Result<Token, Fail> {
    let mut retried = false;
    loop {
        let challenge = challenge(w, warn).await?;
        let Ok(nonce) = (w.random)() else {
            warn.failure("random", None);
            return Err(Fail::Ord);
        };
        let Ok(transcript) =
            ProofTranscript::new(&w.verifier.audience, &w.verifier.agent_id, challenge, nonce)
        else {
            warn.failure("transcript", None);
            return Err(Fail::Ord);
        };
        let Ok(proof) = sign_proof(w.signer, &transcript) else {
            warn.failure("signing", None);
            return Err(Fail::Ord);
        };
        let body = serde_json::json!({
            "agentId": w.verifier.agent_id,
            "challenge": STANDARD.encode(challenge),
            "nonce": STANDARD.encode(nonce),
            "proof": STANDARD.encode(proof.to_bytes()),
        })
        .to_string()
        .into_bytes();
        let reply = match call(w, SESSION_PATH, &body, SESSION_RESULTS).await {
            Ok(reply) => reply,
            Err(_) => {
                warn.failure("session-transport", None);
                return Err(Fail::Ord);
            }
        };
        match reply.status {
            status if SESSION_RESULTS.contains(&status) => {
                // The proof comes before anything of the body is read (ADR-504 item 2).
                let sent = Sent {
                    nonce,
                    method: "POST",
                    target: SESSION_PATH,
                    body: &body,
                };
                // A body over the limit or cut is unproven, even if the server proved it.
                let proved = match &reply.body {
                    Some(ReplyBody::Bytes(answer)) => {
                        w.verifier
                            .proved(&sent, status, answer, reply.proof.as_deref())
                    }
                    Some(ReplyBody::TooLarge | ReplyBody::Cut) | None => false,
                };
                if !proved {
                    warn.unproven("session-unproven", status);
                    return Err(Fail::Cap);
                }
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
                warn.stale_challenge();
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

#[cfg(test)]
mod tests;

//! The data channel's four routes: what the agent sends and how it judges each answer (design
//! 5.2, 13.3, ADR-493 items 1 to 9 and 15 to 23; the classes are ADR-495 item 1). The server's side is `crates/server/src/
//! uploads.rs`; the shapes are `api/openapi.yaml`'s.
//!
//! - begin: `POST /api/v1/agent/uploads`
//! - part URL: `POST /api/v1/agent/uploads/{uploadId}/parts/{part}`
//! - status: `GET /api/v1/agent/uploads/{uploadId}`
//! - complete: `POST /api/v1/agent/uploads/{uploadId}/complete`
//!
//! The session token goes to these four as a bearer and nowhere else, with a fresh `Ngr-Nonce`.
//! An answer whose status is one the route promises (`shared_proto`'s tables) is verified, with the
//! server's proof of it ([`Verifier`]), before any of its body is read; one that is not proved is
//! [`Class::Unproven`], however its body looks. Any other status is classed by the status alone
//! and not checked (ADR-504). Answers are strict: an
//! unknown member, a wrong type, an `uploadId` that is not a canonical UUID (it goes into a
//! path), a `partCount` that is not the agent's own `max(1, ceil(length / partBytes))` or is over
//! 10000, a `partsHeld` above the count, and a part-URL answer whose `certificatePin` is missing or
//! is not 64 lowercase hexadecimal digits are all answers the route does not promise.
//!
//! Every outcome that is not the route's result is a [`Failure`] whose [`Class`] tells the loop
//! what to do with the entry:
//!
//! - [`Class::Reauthenticate`]: a 401. The token is no longer good: exchange a new one.
//! - [`Class::Unproven`]: an answer that was to be a result and is not proved by the server: the
//!   token is dropped, the wait is the back-off's cap, and nothing of the answer is acted on.
//! - [`Class::Retry`]: no answer, a cut, no progress, a time out, a 3xx, 408, 429, 502, 503, 504,
//!   any failure of the part's PUT (a refused TLS handshake with the object server included, which
//!   sends nothing of the part), a part URL answered 409 (the next begin reads the row), and a part URL that breaks the host rule. The server (or the
//!   network) may be better soon.
//! - [`Class::PassOver`]: a refusal the same request will meet again: 400, 404, 409 of begin,
//!   413, 500, any other 4xx, and an answer the route does not promise (a status it never
//!   gives, a body that is not the shape, over 64 KiB or not JSON). The entry is left for this
//!   pass.

use std::sync::Arc;

use hyper::Method;
use ngr_pinned_tls::{CertPin, Refused};
use serde::Deserialize;
use shared_crypto::RandomError;
use shared_proto::{
    BEGIN_RESULTS, COMPLETE_RESULTS, PART_URL_RESULTS, STATUS_RESULTS, is_canonical_uuid,
};

use super::http::{Answer, AnswerBody, Client, Error, JsonRequest, reads_body};
use super::part_url::{self, PartTarget};
use crate::answer_proof::{Sent, Verifier};
use crate::registration::origin::ServerOrigin;
use crate::server_http::retry_after_secs;

/// The begin route's path.
pub const BEGIN_PATH: &str = "/api/v1/agent/uploads";
/// The most parts an upload may have (ADR-493 item 18).
pub const MAX_PARTS: u64 = 10_000;

/// What the loop does with the entry after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A 401: exchange a new token.
    Reauthenticate,
    /// An answer that was to be a result carries no valid proof: drop the token and wait the
    /// cap, with no repeat.
    Unproven,
    /// Try again after the back-off.
    Retry,
    /// Leave the entry for this pass.
    PassOver,
}

/// Why a route gave no result: the class and fixed words, never a body, a token or a URL.
#[derive(Debug, Clone)]
pub struct Failure {
    pub class: Class,
    /// A fixed word naming the reason, for the loop's log line.
    pub why: &'static str,
    pub status: Option<u16>,
    /// The usable `Retry-After` of a 429 or 503 (1 to 60 seconds).
    pub retry_after_secs: Option<u32>,
    /// The error that led to the failure, when there is one. It is not in the fixed words and
    /// may quote what the server sent, so a log line does not print it.
    pub source: Option<Arc<dyn std::error::Error + Send + Sync>>,
}

impl Failure {
    pub(crate) fn new(class: Class, why: &'static str, status: Option<u16>) -> Self {
        Self {
            class,
            why,
            status,
            retry_after_secs: None,
            source: None,
        }
    }

    /// The failure with `source` as its cause.
    pub(crate) fn caused_by(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    pub(crate) fn unpromised(status: u16) -> Self {
        Self::new(Class::PassOver, "answer", Some(status))
    }
}

/// An upload's ID: a canonical UUID, so that it is safe in a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadId(String);

impl UploadId {
    pub fn parse(text: &str) -> Option<Self> {
        is_canonical_uuid(text).then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What an object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Result,
    CommLog,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Result => "result",
            Kind::CommLog => "commLog",
        }
    }
}

/// What begin declares (ADR-493 item 3).
#[derive(Debug, Clone, Copy)]
pub struct Declaration<'a> {
    pub job_id: &'a str,
    pub generation: u64,
    pub kind: Kind,
    pub size_bytes: u64,
    /// 64 lowercase hexadecimal digits.
    pub sha256: &'a str,
}

/// An upload row's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Completing,
    Finished,
    Failed,
}

impl State {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "open" => Some(State::Open),
            "completing" => Some(State::Completing),
            "finished" => Some(State::Finished),
            "failed" => Some(State::Failed),
            _ => None,
        }
    }
}

/// Begin's answer: the row the declaration is (new, 201, or the same declaration's, 200).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begun {
    pub id: UploadId,
    pub part_bytes: u64,
    pub part_count: u64,
    /// `Open`, `Completing` or `Finished`.
    pub state: State,
}

/// Status's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub part_bytes: u64,
    pub part_count: u64,
    /// Present exactly while the row is `open`: the parts from 1 the store holds.
    pub parts_held: Option<u64>,
}

/// A part URL that met the host rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartUrl {
    target: PartTarget,
    expires_in_secs: u64,
}

impl PartUrl {
    pub fn target(&self) -> &PartTarget {
        &self.target
    }

    pub fn expires_in_secs(&self) -> u64 {
        self.expires_in_secs
    }
}

/// Complete's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// 200: the object was read back and matches.
    Finished,
    /// 202: still verifying; repeat the request.
    Completing,
    /// 409 `open`: the parts did not make the object; read the status and send again.
    Open,
    /// 409 `failed`: the upload is over; begin again.
    Failed,
    /// 409 with no state the agent knows.
    Unstated,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UploadAnswer {
    upload_id: String,
    part_bytes: u64,
    part_count: u64,
    state: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct StatusAnswer {
    state: String,
    part_bytes: u64,
    part_count: u64,
    parts_held: Option<u64>,
}

/// What the part-URL answer and the download answer both are: an object URL, the pin of the
/// certificate its server presents, and the URL's life.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ObjectAnswer {
    url: String,
    certificate_pin: String,
    expires_in_secs: u64,
}

/// The target and life of a proved part-URL or download answer, judged in one order for both
/// routes: the body's shape (the pin is a required member), a life above zero, the pin's
/// spelling, then the host rule. A shape, a zero life or a pin that is not 64 lowercase
/// hexadecimal digits is an answer the route does not promise ([`Failure::unpromised`]), and a
/// URL that breaks the rule is a retry named `host_why`. Nothing is dialled for any of them.
pub(crate) fn object_target(
    origin: &ServerOrigin,
    status: u16,
    body: &[u8],
    host_why: &'static str,
) -> Result<(PartTarget, u64), Failure> {
    let parsed: ObjectAnswer = parse(status, body)?;
    if parsed.expires_in_secs == 0 {
        return Err(Failure::unpromised(status));
    }
    let pin = CertPin::from_hex(&parsed.certificate_pin)
        .map_err(|error| Failure::unpromised(status).caused_by(error))?;
    let target = part_url::check(origin, &parsed.url, pin)
        .map_err(|error| Failure::new(Class::Retry, host_why, Some(status)).caused_by(error))?;
    Ok((target, parsed.expires_in_secs))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateAnswer {
    state: String,
}

/// Whether `part_count` is the count `length` makes at `part_bytes`: at least 1, and within
/// [`MAX_PARTS`] (ADR-493 item 18).
fn counts_match(length: u64, part_bytes: u64, part_count: u64) -> bool {
    part_bytes > 0 && part_count <= MAX_PARTS && part_count == length.div_ceil(part_bytes).max(1)
}

/// What the data channel's JSON routes share: the client, the server's origin, the verifier of
/// its proofs and the source of nonces. The upload routes ([`Routes`]) and the download route
/// (`fetch::route::Downloads`) make their requests and judge their answers through it.
#[derive(Clone, Debug)]
pub(crate) struct Requester {
    pub(crate) client: Client,
    pub(crate) origin: ServerOrigin,
    verifier: Arc<Verifier>,
    random: fn() -> Result<[u8; 32], RandomError>,
}

/// An answer with what the request was.
pub(crate) struct Called {
    answer: Answer,
    nonce: [u8; 32],
    method: Method,
    target: String,
    body: Vec<u8>,
}

impl Requester {
    pub(crate) fn new(
        client: Client,
        origin: ServerOrigin,
        verifier: Arc<Verifier>,
        random: fn() -> Result<[u8; 32], RandomError>,
    ) -> Self {
        Self {
            client,
            origin,
            verifier,
            random,
        }
    }

    /// One request with the bearer and a fresh nonce. The body of an answer is read for the
    /// statuses `reads` names.
    pub(crate) async fn call(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        reads: fn(u16) -> bool,
    ) -> Result<Called, Failure> {
        let nonce = (self.random)()
            .map_err(|error| Failure::new(Class::Retry, "random", None).caused_by(error))?;
        let host = self.origin.host_header();
        let request = JsonRequest {
            method,
            target: path,
            host: &host,
            bearer: token,
            body,
            nonce,
            reads,
        };
        let answer = self
            .client
            .json(self.origin.port(), &request)
            .await
            .map_err(transport)?;
        Ok(Called {
            answer,
            nonce,
            method: request.method,
            target: path.to_owned(),
            body: request.body.unwrap_or_default(),
        })
    }

    /// Sorts an answer by status: the status and body when it is one the route promises and the
    /// server proved it, else the failure that status is. The statuses `checked` names (a
    /// superset of `promised`) are verified first: one that is not proved is
    /// [`Class::Unproven`], whatever it says, and a body that is over the limit or cut is not
    /// proved. The rest are classed by status alone.
    pub(crate) fn judge(
        &self,
        called: Called,
        promised: &[u16],
        checked: impl Fn(u16) -> bool,
    ) -> Result<(u16, Vec<u8>), Failure> {
        let Called {
            answer,
            nonce,
            method,
            target,
            body,
        } = called;
        let status = answer.status;
        if checked(status) {
            let sent = Sent {
                nonce,
                method: method.as_str(),
                target: &target,
                body: &body,
            };
            let proved = match &answer.body {
                AnswerBody::Bytes(bytes) => {
                    self.verifier
                        .proved(&sent, status, bytes, answer.proof.as_deref())
                }
                AnswerBody::TooLarge | AnswerBody::Cut | AnswerBody::Unread => false,
            };
            if !proved {
                return Err(Failure::new(Class::Unproven, "unproven", Some(status)));
            }
        }
        if promised.contains(&status) {
            return match answer.body {
                AnswerBody::Bytes(bytes) => Ok((status, bytes)),
                AnswerBody::TooLarge | AnswerBody::Cut | AnswerBody::Unread => {
                    Err(Failure::unpromised(status))
                }
            };
        }
        if answer.body == AnswerBody::Cut {
            // An answer that is no result and whose body failed is no answer.
            return Err(Failure::new(Class::Retry, "transport", None));
        }
        Err(by_status(&answer))
    }
}

/// The four routes of one server.
#[derive(Clone, Debug)]
pub struct Routes {
    requester: Requester,
}

impl Routes {
    pub(crate) fn new(
        client: Client,
        origin: ServerOrigin,
        verifier: Arc<Verifier>,
        random: fn() -> Result<[u8; 32], RandomError>,
    ) -> Self {
        Self {
            requester: Requester::new(client, origin, verifier, random),
        }
    }

    /// Begin: declares the object. `201` is a new `open` row; `200` is the row of the same
    /// declaration, in any state but `failed`.
    pub async fn begin(&self, token: &str, declared: &Declaration<'_>) -> Result<Begun, Failure> {
        let body = serde_json::json!({
            "jobId": declared.job_id,
            "generation": declared.generation,
            "kind": declared.kind.as_str(),
            "sizeBytes": declared.size_bytes,
            "sha256": declared.sha256,
        })
        .to_string()
        .into_bytes();
        let called = self
            .call(token, Method::POST, BEGIN_PATH, Some(body))
            .await?;
        let (status, body) = self.judge(called, BEGIN_RESULTS)?;
        let parsed: UploadAnswer = parse(status, &body)?;
        let state = State::parse(&parsed.state).filter(|s| {
            // A new row is `open`; the same declaration's row is never `failed`.
            if status == 201 {
                *s == State::Open
            } else {
                *s != State::Failed
            }
        });
        match (UploadId::parse(&parsed.upload_id), state) {
            (Some(id), Some(state))
                if counts_match(declared.size_bytes, parsed.part_bytes, parsed.part_count) =>
            {
                Ok(Begun {
                    id,
                    part_bytes: parsed.part_bytes,
                    part_count: parsed.part_count,
                    state,
                })
            }
            _ => Err(Failure::unpromised(status)),
        }
    }

    /// One part's URL, already judged by the host rule. A URL that breaks it is a retry, and the
    /// agent sends nothing to it.
    pub async fn part_url(
        &self,
        token: &str,
        id: &UploadId,
        part: u32,
    ) -> Result<PartUrl, Failure> {
        let path = format!("{BEGIN_PATH}/{}/parts/{part}", id.as_str());
        let called = self
            .call(token, Method::POST, &path, Some(Vec::new()))
            .await?;
        // A 409 (the row is not open) is a retry: the next attempt's begin reads the row's state.
        // It is not a result and is not checked.
        if called.answer.status == 409 {
            return Err(Failure::new(Class::Retry, "not-open", Some(409)));
        }
        let (status, body) = self.judge(called, PART_URL_RESULTS)?;
        let (target, expires_in_secs) =
            object_target(&self.requester.origin, status, &body, "part-url-host")?;
        Ok(PartUrl {
            target,
            expires_in_secs,
        })
    }

    /// Status of an upload of `length` bytes.
    pub async fn status(&self, token: &str, id: &UploadId, length: u64) -> Result<Status, Failure> {
        let path = format!("{BEGIN_PATH}/{}", id.as_str());
        let called = self.call(token, Method::GET, &path, None).await?;
        let (status, body) = self.judge(called, STATUS_RESULTS)?;
        let parsed: StatusAnswer = parse(status, &body)?;
        let state = State::parse(&parsed.state);
        let held_ok = match (state, parsed.parts_held) {
            (Some(State::Open), Some(held)) => held <= parsed.part_count,
            (Some(State::Open), None) | (_, Some(_)) => false,
            (Some(_), None) => true,
            (None, _) => false,
        };
        match state {
            Some(state)
                if held_ok && counts_match(length, parsed.part_bytes, parsed.part_count) =>
            {
                Ok(Status {
                    state,
                    part_bytes: parsed.part_bytes,
                    part_count: parsed.part_count,
                    parts_held: parsed.parts_held,
                })
            }
            _ => Err(Failure::unpromised(status)),
        }
    }

    /// Complete: asks the server to verify the object.
    pub async fn complete(&self, token: &str, id: &UploadId) -> Result<Completion, Failure> {
        let path = format!("{BEGIN_PATH}/{}/complete", id.as_str());
        let called = self
            .call(token, Method::POST, &path, Some(Vec::new()))
            .await?;
        let (status, body) = self.judge(called, COMPLETE_RESULTS)?;
        if status == 409 {
            // The problem document's `state` member; any other shape is no state.
            let state = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("state")?.as_str().and_then(State::parse));
            return Ok(match state {
                Some(State::Open) => Completion::Open,
                Some(State::Failed) => Completion::Failed,
                _ => Completion::Unstated,
            });
        }
        let parsed: StateAnswer = parse(status, &body)?;
        match (status, parsed.state.as_str()) {
            (200, "finished") => Ok(Completion::Finished),
            (202, "completing") => Ok(Completion::Completing),
            _ => Err(Failure::unpromised(status)),
        }
    }

    /// PUTs a part of `length` bytes read from `source` to a URL that met the host rule. No
    /// token is sent. Any failure is a retry: the URL is only good for a while and the loop asks
    /// for the status again.
    pub async fn put_part(
        &self,
        url: &PartUrl,
        length: u64,
        source: Box<dyn std::io::Read + Send>,
    ) -> Result<(), Failure> {
        let target = &url.target;
        match self
            .requester
            .client
            .put(
                target.port(),
                target.pin(),
                target.target(),
                target.host(),
                length,
                source,
            )
            .await
        {
            Ok(answer) if (200..300).contains(&answer.status) => Ok(()),
            Ok(answer) => Err(Failure::new(
                Class::Retry,
                "put-refused",
                Some(answer.status),
            )),
            Err(error) => Err(transport(error)),
        }
    }

    async fn call(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Called, Failure> {
        self.requester
            .call(token, method, path, body, reads_body)
            .await
    }

    /// [`Requester::judge`] for a route whose results are `promised`: those statuses are verified.
    fn judge(&self, called: Called, promised: &[u16]) -> Result<(u16, Vec<u8>), Failure> {
        self.requester
            .judge(called, promised, |status| promised.contains(&status))
    }
}

/// A request that got no answer is always a retry.
pub(crate) fn transport(error: Error) -> Failure {
    let why = match &error {
        Error::NotSent(_) => "not-sent",
        Error::ConnectTimeout => "connect-timeout",
        Error::Tls(Refused::Certificate) => "object-tls-certificate",
        Error::Tls(Refused::Signature) => "object-tls-signature",
        Error::Tls(Refused::Handshake(_)) => "object-tls-handshake",
        Error::NoProgress => "no-progress",
        Error::Timeout => "timeout",
        Error::Failed(_) => "transport",
        Error::Write(_) => "write",
    };
    Failure::new(Class::Retry, why, None).caused_by(error)
}

/// The failure a status is when it is not a result of the route.
fn by_status(answer: &Answer) -> Failure {
    let status = answer.status;
    let mut failure = match status {
        401 => Failure::new(Class::Reauthenticate, "unauthorized", Some(401)),
        300..=399 => Failure::new(Class::Retry, "redirect", Some(status)),
        408 | 429 | 502 | 503 | 504 => Failure::new(Class::Retry, "unavailable", Some(status)),
        // Any 4xx but those above, and 500.
        400..=500 => Failure::new(Class::PassOver, "refused", Some(status)),
        _ => Failure::unpromised(status),
    };
    if matches!(status, 429 | 503) {
        failure.retry_after_secs = retry_after_secs(answer.retry_after.as_deref());
    }
    failure
}

/// A body that is exactly the document's shape, or the answer is not promised.
pub(crate) fn parse<'a, T: Deserialize<'a>>(status: u16, body: &'a [u8]) -> Result<T, Failure> {
    serde_json::from_slice(body).map_err(|error| Failure::unpromised(status).caused_by(error))
}

#[cfg(test)]
mod tests;

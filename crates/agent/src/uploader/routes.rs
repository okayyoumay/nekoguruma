//! The data channel's four routes: what the agent sends and how it judges each answer (design
//! 5.2, 13.3, ADR-493 items 1 to 9 and 15 to 23; the classes are ADR-495 item 1). The server's side is `crates/server/src/
//! uploads.rs`; the shapes are `api/openapi.yaml`'s.
//!
//! - begin: `POST /api/v1/agent/uploads`
//! - part URL: `POST /api/v1/agent/uploads/{uploadId}/parts/{part}`
//! - status: `GET /api/v1/agent/uploads/{uploadId}`
//! - complete: `POST /api/v1/agent/uploads/{uploadId}/complete`
//!
//! The session token goes to these four as a bearer and nowhere else. Answers are strict: an
//! unknown member, a wrong type, an `uploadId` that is not a canonical UUID (it goes into a
//! path), a `partCount` that is not the agent's own `max(1, ceil(length / partBytes))` or is over
//! 10000, and a `partsHeld` above the count are all answers the route does not promise.
//!
//! Every outcome that is not the route's result is a [`Failure`] whose [`Class`] tells the loop
//! what to do with the entry:
//!
//! - [`Class::Reauthenticate`]: a 401. The token is no longer good: exchange a new one.
//! - [`Class::Retry`]: no answer, a cut, no progress, a time out, a 3xx, 408, 429, 502, 503, 504,
//!   any failure of the part's PUT, a part URL answered 409 (the next begin reads the row), and a part URL that breaks the host rule. The server (or the
//!   network) may be better soon.
//! - [`Class::PassOver`]: a refusal the same request will meet again: 400, 404, 409 of begin,
//!   413, 500, any other 4xx, and an answer the route does not promise (a status it never
//!   gives, a body that is not the shape, over 64 KiB or not JSON). The entry is left for this
//!   pass.

use hyper::Method;
use serde::Deserialize;
use shared_proto::is_canonical_uuid;

use super::http::{Answer, AnswerBody, Client, Error, JsonRequest};
use super::part_url::{self, PartTarget};
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
    /// Try again after the back-off.
    Retry,
    /// Leave the entry for this pass.
    PassOver,
}

/// Why a route gave no result: the class and fixed words, never a body, a token or a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub class: Class,
    /// A fixed word naming the reason, for the loop's log line.
    pub why: &'static str,
    pub status: Option<u16>,
    /// The usable `Retry-After` of a 429 or 503 (1 to 60 seconds).
    pub retry_after_secs: Option<u32>,
}

impl Failure {
    pub(crate) fn new(class: Class, why: &'static str, status: Option<u16>) -> Self {
        Self {
            class,
            why,
            status,
            retry_after_secs: None,
        }
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PartAnswer {
    url: String,
    expires_in_secs: u64,
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

/// The four routes of one server.
#[derive(Clone, Debug)]
pub struct Routes {
    client: Client,
    origin: ServerOrigin,
}

impl Routes {
    pub fn new(client: Client, origin: ServerOrigin) -> Self {
        Self { client, origin }
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
        let answer = self
            .call(token, Method::POST, BEGIN_PATH, Some(body))
            .await?;
        let (status, body) = judge(answer, &[200, 201])?;
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
        let answer = self
            .call(token, Method::POST, &path, Some(Vec::new()))
            .await?;
        // A 409 (the row is not open) is a retry: the next attempt's begin reads the row's state.
        if answer.status == 409 {
            return Err(Failure::new(Class::Retry, "not-open", Some(409)));
        }
        let (status, body) = judge(answer, &[200])?;
        let parsed: PartAnswer = parse(status, &body)?;
        if parsed.expires_in_secs == 0 {
            return Err(Failure::unpromised(status));
        }
        let target = part_url::check(&self.origin, &parsed.url)
            .map_err(|_| Failure::new(Class::Retry, "part-url-host", Some(status)))?;
        Ok(PartUrl {
            target,
            expires_in_secs: parsed.expires_in_secs,
        })
    }

    /// Status of an upload of `length` bytes.
    pub async fn status(&self, token: &str, id: &UploadId, length: u64) -> Result<Status, Failure> {
        let path = format!("{BEGIN_PATH}/{}", id.as_str());
        let answer = self.call(token, Method::GET, &path, None).await?;
        let (status, body) = judge(answer, &[200])?;
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
        let answer = self
            .call(token, Method::POST, &path, Some(Vec::new()))
            .await?;
        let (status, body) = judge(answer, &[200, 202, 409])?;
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
            .client
            .put(
                target.port(),
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
    ) -> Result<Answer, Failure> {
        let host = self.origin.host_header();
        let request = JsonRequest {
            method,
            target: path,
            host: &host,
            bearer: token,
            body,
        };
        self.client
            .json(self.origin.port(), &request)
            .await
            .map_err(transport)
    }
}

/// A request that got no answer is always a retry.
pub(crate) fn transport(error: Error) -> Failure {
    let why = match error {
        Error::NotSent => "not-sent",
        Error::ConnectTimeout => "connect-timeout",
        Error::NoProgress => "no-progress",
        Error::Timeout => "timeout",
        Error::Failed => "transport",
        Error::Write(_) => "write",
    };
    Failure::new(Class::Retry, why, None)
}

/// Sorts an answer by status: the status and body when it is one the route promises, else the
/// failure that class of status is.
pub(crate) fn judge(answer: Answer, promised: &[u16]) -> Result<(u16, Vec<u8>), Failure> {
    let status = answer.status;
    if promised.contains(&status) {
        return match answer.body {
            AnswerBody::Bytes(bytes) => Ok((status, bytes)),
            AnswerBody::TooLarge | AnswerBody::Unread => Err(Failure::unpromised(status)),
        };
    }
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
    Err(failure)
}

/// A body that is exactly the document's shape, or the answer is not promised.
pub(crate) fn parse<'a, T: Deserialize<'a>>(status: u16, body: &'a [u8]) -> Result<T, Failure> {
    serde_json::from_slice(body).map_err(|_| Failure::unpromised(status))
}

#[cfg(test)]
mod tests;

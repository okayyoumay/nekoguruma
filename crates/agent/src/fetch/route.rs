//! The download route: what the agent sends and how it judges the answer (design 14.3, ADR-497
//! item 1; the classes are ADR-495 item 1, shared with the upload routes). The server's side is
//! `crates/server/src/downloads.rs`.
//!
//! `POST /api/v1/agent/downloads` carries the job, the ownership generation, the kind and the
//! SHA-256 as JSON and the session token as a bearer; the answer is a presigned GET URL and its
//! life in seconds. The answer is strict: an unknown member, a missing `url`, a zero life, a body
//! over 64 KiB and anything that is not that JSON object are answers the route does not promise.
//! The URL goes through the part URL's host rule before any byte is sent to it, and a URL that
//! breaks the rule is a retry.
//!
//! The classes are the upload routes' ([`Class`]): a 401 is [`Class::Reauthenticate`]; no answer,
//! a cut, no progress, a time out, a 3xx, 408, 429, 502, 503 and 504 are [`Class::Retry`] (a
//! `Retry-After` of 1 to 60 seconds is kept); 400, 404, 413, 500, any other 4xx and an answer
//! that is not promised are [`Class::PassOver`], which here ends the fetch.

use hyper::Method;
use serde::Deserialize;

use super::Kind;
use crate::registration::origin::ServerOrigin;
use crate::uploader::http::{Client, JsonRequest};
use crate::uploader::part_url::{self, PartTarget};
use crate::uploader::routes::{Class, Failure, judge, parse, transport};

/// The route's path.
pub const PATH: &str = "/api/v1/agent/downloads";

/// What is asked for: one object of one job.
#[derive(Debug, Clone, Copy)]
pub struct Want<'a> {
    pub job_id: &'a str,
    pub generation: u64,
    pub kind: Kind,
    /// 64 lowercase hexadecimal digits.
    pub sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Answer {
    url: String,
    expires_in_secs: u64,
}

/// The download route of one server.
#[derive(Clone, Debug)]
pub struct Downloads {
    client: Client,
    origin: ServerOrigin,
}

impl Downloads {
    pub fn new(client: Client, origin: ServerOrigin) -> Self {
        Self { client, origin }
    }

    /// The GET URL for `want`, already judged by the host rule.
    pub async fn url(&self, token: &str, want: &Want<'_>) -> Result<PartTarget, Failure> {
        let body = serde_json::json!({
            "jobId": want.job_id,
            "generation": want.generation,
            "kind": want.kind.as_str(),
            "sha256": want.sha256,
        })
        .to_string()
        .into_bytes();
        let host = self.origin.host_header();
        let request = JsonRequest {
            method: Method::POST,
            target: PATH,
            host: &host,
            bearer: token,
            body: Some(body),
        };
        let answer = self
            .client
            .json(self.origin.port(), &request)
            .await
            .map_err(transport)?;
        let (status, body) = judge(answer, &[200])?;
        let parsed: Answer = parse(status, &body)?;
        if parsed.expires_in_secs == 0 {
            return Err(Failure::unpromised(status));
        }
        part_url::check(&self.origin, &parsed.url)
            .map_err(|_| Failure::new(Class::Retry, "download-url-host", Some(status)))
    }
}

#[cfg(test)]
mod tests;

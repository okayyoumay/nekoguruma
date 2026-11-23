//! The download route: what the agent sends and how it judges the answer (design 14.3, ADR-497
//! item 1; the classes are ADR-495 item 1, shared with the upload routes). The server's side is
//! `crates/server/src/downloads.rs`.
//!
//! `POST /api/v1/agent/downloads` carries the job, the ownership generation, the kind and the
//! SHA-256 as JSON and the session token as a bearer; the answer is a presigned GET URL, the pin of
//! the certificate the object server presents, and the URL's life in seconds. The answer is
//! strict, and judged as the part-URL answer is (`routes::object_target`): an unknown member, a
//! missing `url` or `certificatePin`, a pin that is not 64 lowercase hexadecimal digits, a zero
//! life, a body over 64 KiB and anything that is not that JSON object are answers the route does
//! not promise. The URL goes through the object URL's rule (`https` and the registered host)
//! before any byte is sent to it, and a URL that breaks the rule is a retry.
//!
//! The request carries a fresh `Ngr-Nonce`. The answer is verified with the server's proof before
//! any of it is read, for the 200 and for every status that would end the fetch (400, 404, 413,
//! 500, any other 4xx and a status the route never gives): a refusal ends the fetch only when the
//! server proved it, and one that is not proved is [`Class::Unproven`] (ADR-504 item 3). Their
//! bodies are read for that (up to 64 KiB; a body that is over the limit or cut is not proved). A
//! 401, a 3xx, 408, 429, 502, 503 and 504 are not checked.
//!
//! The classes are the upload routes' ([`Class`]): a 401 is [`Class::Reauthenticate`]; no answer,
//! a cut, no progress, a time out, a 3xx, 408, 429, 502, 503 and 504 are [`Class::Retry`] (a
//! `Retry-After` of 1 to 60 seconds is kept); 400, 404, 413, 500, any other 4xx and an answer
//! that is not promised are [`Class::PassOver`], which here ends the fetch.

use std::sync::Arc;

use hyper::Method;
use shared_crypto::RandomError;
use shared_proto::DOWNLOAD_RESULTS;

use super::Kind;
use crate::answer_proof::Verifier;
use crate::registration::origin::ServerOrigin;
use crate::uploader::http::Client;
use crate::uploader::part_url::PartTarget;
use crate::uploader::routes::{Failure, Requester, object_target};

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

/// Whether the answer with this status is verified before it is used: all but the statuses the
/// agent retries or answers by asking for a new token. Its body is read for the same statuses.
fn verified(status: u16) -> bool {
    !matches!(status, 300..=399 | 401 | 408 | 429 | 502..=504)
}

/// The download route of one server.
#[derive(Clone, Debug)]
pub struct Downloads {
    requester: Requester,
}

impl Downloads {
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
        let called = self
            .requester
            .call(token, Method::POST, PATH, Some(body), verified)
            .await?;
        let (status, body) = self.requester.judge(called, DOWNLOAD_RESULTS, verified)?;
        object_target(&self.requester.origin, status, &body, "download-url-host")
            .map(|(target, _)| target)
    }
}

#[cfg(test)]
mod tests;

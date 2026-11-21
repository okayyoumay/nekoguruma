//! The agent's download leg (design 4.1, 8.2.6, 14.3, ADR-497 items 6 to 8): fetching what a
//! verified job instruction names, a document by its hash or the artifact by its hash, from the
//! object store through a presigned GET URL the server answers.
//!
//! - [`route`]: `POST /api/v1/agent/downloads`, its strict answer and its classes
//! - [`store`]: the cache under `<state>/objects/`: the directory lock, the temporary area, the
//!   rename into a name that is the digest, and the check at every load
//!
//! One attempt ([`Fetcher::fetch`]) is: the cache, the token (exchanged only now that there is
//! work, as the uploader does), the route, the host rule, the GET into a temporary file while
//! hashing, the digest check and the rename. It ends in a [`Fetched`] or one [`FetchError`],
//! whose variant is the class:
//!
//! - [`FetchError::Retry`]: no answer, a cut, no progress, a refused or lapsed URL, a status of
//!   the GET that is not 200, a URL that breaks the host rule, a failed exchange: try again after
//!   the back-off
//! - [`FetchError::Refused`]: the server will not give this object (400, 404, 413, 500, another
//!   4xx, an answer that is not promised) or the object server's answer has no usable length: the
//!   fetch ends and the job cannot start
//! - [`FetchError::Local`]: the disk (or a cached name that is not a regular file): not retried by
//!   the library, and nothing is deleted or evicted
//! - [`FetchError::Mismatch`]: the full download's digest differs from the instruction's hash; the
//!   temporary file is deleted and one more attempt is allowed. A second mismatch in a row for
//!   the object is [`FetchError::MismatchTwice`], and the object fails until a restart
//! - [`FetchError::NotNamed`]: the instruction does not name the digest; nothing is sent
//!
//! The library classes and the caller bounds: [`Fetcher::fetch_retrying`] is the simple bounded
//! loop with the control channel's back-off ([`Backoff`]). A stop is dropping the future: the
//! temporary file is deleted best effort and nothing is reported (the sweep at the next open
//! takes what this cannot). One fetch runs at a time per [`Fetcher`]; a second request for the
//! same digest then finds the cache. The job-start wiring (fetch before the run's first step)
//! belongs to the intake item; nothing in `connect` calls this yet.
//!
//! No interface here returns bytes that were not hashed against the instruction's hash:
//! [`Fetcher::read_document`] returns only bytes that matched, and [`Fetched`] names a file that
//! matched when it was returned (a reader of the path hashes it again, as `Artifact` does).
//!
//! Nothing here logs a URL, a token, a digest or a body: an event holds the kind, the class, a
//! fixed word and the status.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use shared_crypto::proof::Audience;
use shared_crypto::{RandomError, Signer};
use shared_proto::{MAX_ARTIFACT_BYTES, MAX_DOCUMENT_BYTES};
use tokio::sync::Mutex;

use crate::control::ControlError;
use crate::control::backoff::Backoff;
use crate::control::dial::{AtPort, LoopbackPorts, PortDial};
use crate::control::exchange::{self, Fail, Token};
use crate::identity::AgentKey;
use crate::instruction::VerifiedInstruction;
use crate::registration::origin::ServerOrigin;
use crate::registration::record::{Record, RecordError};
use crate::server_http::retry_after_secs;
use crate::uploader::http::{Client, Error as HttpError, GetBody};
use crate::uploader::routes::{Class, Failure, transport};
use route::{Downloads, Want};
use store::{HashingWriter, Load, ObjectStore, StoreError, hex};

#[cfg(debug_assertions)]
pub mod command;
pub mod route;
pub mod store;

/// What an object is: the instruction names documents and an artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Document,
    Artifact,
}

impl Kind {
    /// The route's `kind` and the log's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Document => "document",
            Kind::Artifact => "artifact",
        }
    }

    /// The cache's sub-directory.
    pub(crate) fn dir(self) -> &'static str {
        match self {
            Kind::Document => "documents",
            Kind::Artifact => "artifacts",
        }
    }
}

/// An object in the cache whose bytes hashed to its digest when this was returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub kind: Kind,
    pub path: PathBuf,
    pub length: u64,
}

/// Why an attempt gave no object. The class is the variant; no text holds a URL, a token, a
/// digest or a body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("the instruction does not name this object")]
    NotNamed,
    #[error("try again: {why}")]
    Retry {
        why: &'static str,
        status: Option<u16>,
        /// A usable `Retry-After` (1 to 60 seconds), which lengthens the wait.
        retry_after_secs: Option<u32>,
    },
    #[error("refused: {why}")]
    Refused {
        why: &'static str,
        status: Option<u16>,
    },
    #[error("local failure: {why}")]
    Local { why: &'static str },
    #[error("the download's digest differs from the instruction's hash")]
    Mismatch,
    #[error("two downloads in a row did not match the instruction's hash; no more until a restart")]
    MismatchTwice,
}

impl FetchError {
    /// The class's fixed word.
    pub fn class(&self) -> &'static str {
        match self {
            FetchError::NotNamed => "not-named",
            FetchError::Retry { .. } => "retry",
            FetchError::Refused { .. } => "refused",
            FetchError::Local { .. } => "local",
            FetchError::Mismatch => "mismatch",
            FetchError::MismatchTwice => "mismatch-twice",
        }
    }

    /// Whether another attempt may come out differently.
    pub fn may_retry(&self) -> bool {
        matches!(self, FetchError::Retry { .. } | FetchError::Mismatch)
    }

    fn status(&self) -> Option<u16> {
        match self {
            FetchError::Retry { status, .. } | FetchError::Refused { status, .. } => *status,
            _ => None,
        }
    }

    fn retry(why: &'static str, status: Option<u16>) -> Self {
        FetchError::Retry {
            why,
            status,
            retry_after_secs: None,
        }
    }

    fn of(failure: Failure) -> Self {
        match failure.class {
            Class::PassOver => FetchError::Refused {
                why: failure.why,
                status: failure.status,
            },
            Class::Retry | Class::Reauthenticate => FetchError::Retry {
                why: failure.why,
                status: failure.status,
                retry_after_secs: failure.retry_after_secs,
            },
        }
    }
}

impl From<StoreError> for FetchError {
    fn from(error: StoreError) -> Self {
        FetchError::Local {
            why: match error {
                StoreError::NotRegular => "not-regular",
                StoreError::InUse => "in-use",
                StoreError::Dir(_) => "directory",
                StoreError::Io(_) => "io",
            },
        }
    }
}

/// The bounds of a download, in bytes.
#[derive(Debug, Clone, Copy)]
struct Limits {
    document: u64,
    artifact: u64,
}

impl Limits {
    fn of(self, kind: Kind) -> u64 {
        match kind {
            Kind::Document => self.document,
            Kind::Artifact => self.artifact,
        }
    }
}

/// The exchange's log lines in the fetcher's words: one warning per class until a token is
/// issued, debug after that, and one per refusal until a token is issued again.
#[derive(Default)]
struct ExchangeLog {
    seen: HashSet<&'static str>,
    refused: bool,
}

impl exchange::Log for ExchangeLog {
    fn failure(&mut self, class: &'static str, status: Option<u16>) {
        if self.seen.insert(class) {
            tracing::warn!(class, status = ?status, "fetch exchange attempt failed");
        } else {
            tracing::debug!(class, status = ?status, "fetch exchange attempt failed");
        }
    }

    fn refusal(&mut self, class: &'static str, status: u16) {
        if self.refused {
            tracing::debug!(class, status, "fetch exchange refused again");
        } else {
            self.refused = true;
            tracing::warn!(class, status, "fetch exchange refused");
        }
    }

    fn accepted(&mut self) {
        self.seen.clear();
        self.refused = false;
    }

    fn stale_challenge(&mut self) {
        tracing::debug!("fetch challenge was not live; asking for a new one");
    }
}

/// What is shared between attempts and changes: only ever touched with the lock held, which is
/// also what makes one fetch run at a time.
#[derive(Default)]
struct State {
    /// Held only while a call has work; dropped at its end and by a 401.
    token: Option<Token>,
    /// Digest mismatches in a row, per object.
    mismatches: HashMap<(Kind, [u8; 32]), u8>,
    /// A cached file that failed its check is warned about once per kind.
    warned: HashSet<Kind>,
    log: ExchangeLog,
}

/// What the fetcher is wired to; tests replace the client, the dial, the signer and the random
/// source.
pub(crate) struct Seams {
    pub(crate) client: Client,
    pub(crate) dial: Arc<dyn PortDial>,
    pub(crate) signer: Arc<dyn Signer + Send + Sync>,
    pub(crate) random: fn() -> Result<[u8; 32], RandomError>,
}

/// The download leg of one registered agent.
pub struct Fetcher {
    store: Arc<ObjectStore>,
    downloads: Downloads,
    client: Client,
    dial: Arc<dyn PortDial>,
    signer: Arc<dyn Signer + Send + Sync>,
    random: fn() -> Result<[u8; 32], RandomError>,
    agent_id: String,
    audience: Audience,
    host: String,
    port: u16,
    limits: Limits,
    state: Mutex<State>,
}

impl Fetcher {
    /// The fetcher of the registered agent `record` (whose key is `key`) over the open `store`.
    /// It sends nothing until an object is asked for.
    pub fn new(record: &Record, key: &AgentKey, store: ObjectStore) -> Result<Self, ControlError> {
        let seams = Seams {
            client: Client::loopback(),
            dial: Arc::new(LoopbackPorts),
            signer: key.signer(),
            random: shared_crypto::random_32,
        };
        Self::with_seams(record, &key.public_key(), store, seams)
    }

    pub(crate) fn with_seams(
        record: &Record,
        key: &shared_crypto::PublicKey,
        store: ObjectStore,
        seams: Seams,
    ) -> Result<Self, ControlError> {
        if key.fingerprint_hex() != record.agent_key_fingerprint {
            return Err(ControlError::KeyMismatch);
        }
        let origin = ServerOrigin::parse(&record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        let audience = Audience::parse_canonical(&record.server_origin)
            .map_err(|_| RecordError::Field("server_origin"))?;
        Ok(Self {
            store: Arc::new(store),
            downloads: Downloads::new(seams.client.clone(), origin.clone()),
            client: seams.client,
            dial: seams.dial,
            signer: seams.signer,
            random: seams.random,
            agent_id: record.agent_id.clone(),
            audience,
            host: origin.host_header(),
            port: origin.port(),
            limits: Limits {
                document: MAX_DOCUMENT_BYTES,
                artifact: MAX_ARTIFACT_BYTES,
            },
            state: Mutex::new(State::default()),
        })
    }

    /// Other bounds, so that a test need not move 16 MiB.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, document: u64, artifact: u64) -> Self {
        self.limits = Limits { document, artifact };
        self
    }

    /// One attempt to have the object `digest` of `kind` in the cache. Only a digest the
    /// instruction names is fetched.
    pub async fn fetch(
        &self,
        instruction: &VerifiedInstruction,
        kind: Kind,
        digest: &[u8; 32],
    ) -> Result<Fetched, FetchError> {
        let mut state = self.state.lock().await;
        state.token = None;
        let got = self.attempt(&mut state, instruction, kind, digest).await;
        state.token = None;
        got
    }

    /// [`Fetcher::fetch`] up to `attempts` times (at least one) while the error may retry, with
    /// the control channel's back-off between attempts (a longer `Retry-After` the server named
    /// lengthens a wait). The token is kept across the attempts and dropped at the end.
    pub async fn fetch_retrying(
        &self,
        instruction: &VerifiedInstruction,
        kind: Kind,
        digest: &[u8; 32],
        attempts: u32,
    ) -> Result<Fetched, FetchError> {
        let mut state = self.state.lock().await;
        state.token = None;
        let got = self
            .retrying(&mut state, instruction, kind, digest, attempts)
            .await;
        state.token = None;
        got
    }

    /// Every document the instruction names, then its artifact, each as [`Fetcher::fetch_retrying`]
    /// with `attempts`; the first object that fails ends it. The order is the instruction's.
    pub async fn fetch_all(
        &self,
        instruction: &VerifiedInstruction,
        attempts: u32,
    ) -> Result<Vec<Fetched>, FetchError> {
        let named = instruction.instruction();
        let wanted = named
            .document_hashes
            .iter()
            .map(|hash| (Kind::Document, hash))
            .chain(
                named
                    .artifact_hash
                    .iter()
                    .map(|hash| (Kind::Artifact, hash)),
            );
        let mut state = self.state.lock().await;
        state.token = None;
        let mut fetched = Vec::new();
        let mut outcome = Ok(());
        for (kind, digest) in wanted {
            match self
                .retrying(&mut state, instruction, kind, digest, attempts)
                .await
            {
                Ok(object) => fetched.push(object),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        state.token = None;
        outcome.map(|()| fetched)
    }

    /// The bytes of a document the instruction names, fetched when the cache lacks it: only bytes
    /// that hashed to the instruction's hash, read within the document bound.
    pub async fn read_document(
        &self,
        instruction: &VerifiedInstruction,
        digest: &[u8; 32],
        attempts: u32,
    ) -> Result<Vec<u8>, FetchError> {
        // A cached file is hashed again when it is read, so a file that went bad after the fetch
        // is a miss and fetched again, a bounded number of times.
        for _ in 0..2 {
            self.fetch_retrying(instruction, Kind::Document, digest, attempts)
                .await?;
            let store = self.store.clone();
            let (digest, max) = (*digest, self.limits.document);
            let read =
                tokio::task::spawn_blocking(move || store.read(Kind::Document, &digest, max))
                    .await
                    .map_err(|_| FetchError::Local { why: "task" })??;
            if let Some(bytes) = read {
                return Ok(bytes);
            }
        }
        Err(FetchError::Local { why: "changed" })
    }

    async fn retrying(
        &self,
        state: &mut State,
        instruction: &VerifiedInstruction,
        kind: Kind,
        digest: &[u8; 32],
        attempts: u32,
    ) -> Result<Fetched, FetchError> {
        let attempts = attempts.max(1);
        let mut backoff = Backoff::new();
        let mut attempt = 1;
        loop {
            let error = match self.attempt(state, instruction, kind, digest).await {
                Ok(object) => return Ok(object),
                Err(error) => error,
            };
            if !error.may_retry() || attempt >= attempts {
                tracing::warn!(
                    kind = kind.as_str(),
                    class = error.class(),
                    status = ?error.status(),
                    attempts = attempt,
                    "an object was not fetched"
                );
                return Err(error);
            }
            tracing::debug!(
                kind = kind.as_str(),
                class = error.class(),
                status = ?error.status(),
                attempt,
                "a fetch attempt will be retried"
            );
            let hint = match error {
                FetchError::Retry {
                    retry_after_secs, ..
                } => retry_after_secs,
                _ => None,
            };
            tokio::time::sleep(backoff.wait(hint)).await;
            backoff.after_wait();
            attempt += 1;
        }
    }

    async fn attempt(
        &self,
        state: &mut State,
        instruction: &VerifiedInstruction,
        kind: Kind,
        digest: &[u8; 32],
    ) -> Result<Fetched, FetchError> {
        let named = instruction.instruction();
        let is_named = match kind {
            Kind::Document => named.document_hashes.contains(digest),
            Kind::Artifact => named.artifact_hash.as_ref() == Some(digest),
        };
        if !is_named {
            return Err(FetchError::NotNamed);
        }
        let max = self.limits.of(kind);
        // A cached object is used with no request and no exchange.
        let store = self.store.clone();
        let wanted = *digest;
        let loaded = tokio::task::spawn_blocking(move || store.load(kind, &wanted, max))
            .await
            .map_err(|_| FetchError::Local { why: "task" })??;
        match loaded {
            Load::Hit { path, length } => return Ok(Fetched { kind, path, length }),
            Load::Deleted => {
                if state.warned.insert(kind) {
                    tracing::warn!(
                        kind = kind.as_str(),
                        "a cached object did not match its hash and was deleted"
                    );
                }
            }
            Load::Miss => {}
        }
        if state.mismatches.get(&(kind, *digest)).copied().unwrap_or(0) >= 2 {
            return Err(FetchError::MismatchTwice);
        }
        let sha = hex(digest);
        let want = Want {
            job_id: &named.job_id.0,
            generation: named.ownership_generation,
            kind,
            sha256: &sha,
        };
        let target = self.target(state, &want).await?;
        let store = self.store.clone();
        let (tmp, file) = tokio::task::spawn_blocking(move || store.begin())
            .await
            .map_err(|_| FetchError::Local { why: "task" })??;
        let got = self
            .client
            .get(
                target.port(),
                target.target(),
                target.host(),
                max,
                HashingWriter::new(file),
            )
            .await;
        let got = match got {
            Ok(got) => got,
            Err(HttpError::Write(_)) => return Err(FetchError::Local { why: "write" }),
            Err(error) => return Err(FetchError::of(transport(error))),
        };
        let (writer, length) = match got.body {
            GetBody::Complete { length, writer } => (writer, length),
            GetBody::BadLength => {
                return Err(FetchError::Refused {
                    why: "get-length",
                    status: Some(got.status),
                });
            }
            GetBody::Unread => {
                let why = if (300..400).contains(&got.status) {
                    "get-redirect"
                } else {
                    "get-status"
                };
                return Err(FetchError::Retry {
                    why,
                    status: Some(got.status),
                    retry_after_secs: retry_after_secs(got.retry_after.as_deref()),
                });
            }
        };
        let (file, got_digest, written) = writer.finish();
        debug_assert_eq!(written, length);
        if got_digest != *digest {
            // `tmp` is deleted as it drops.
            let streak = state.mismatches.entry((kind, *digest)).or_default();
            *streak += 1;
            return Err(if *streak >= 2 {
                FetchError::MismatchTwice
            } else {
                FetchError::Mismatch
            });
        }
        let store = self.store.clone();
        let wanted = *digest;
        let path = tokio::task::spawn_blocking(move || store.commit(tmp, file, kind, &wanted))
            .await
            .map_err(|_| FetchError::Local { why: "task" })??;
        state.mismatches.remove(&(kind, *digest));
        Ok(Fetched { kind, path, length })
    }

    /// The GET URL: the token (exchanged now when there is none), the route, and after a 401 one
    /// new exchange and one repeat; a second 401 is a retry.
    async fn target(
        &self,
        state: &mut State,
        want: &Want<'_>,
    ) -> Result<crate::uploader::part_url::PartTarget, FetchError> {
        let mut repeated = false;
        loop {
            let token = self.ensure_token(state).await?;
            match self.downloads.url(&token, want).await {
                Ok(target) => return Ok(target),
                Err(failure) if failure.class == Class::Reauthenticate => {
                    state.token = None;
                    if repeated {
                        return Err(FetchError::retry("unauthorized", Some(401)));
                    }
                    repeated = true;
                }
                Err(failure) => return Err(FetchError::of(failure)),
            }
        }
    }

    async fn ensure_token(&self, state: &mut State) -> Result<String, FetchError> {
        if let Some(token) = &state.token {
            return Ok(token.as_str().to_owned());
        }
        let at = AtPort {
            dial: &*self.dial,
            port: self.port,
        };
        let seams = exchange::Seams {
            dial: &at,
            signer: &*self.signer,
            random: self.random,
            agent_id: &self.agent_id,
            audience: &self.audience,
            host: &self.host,
        };
        match exchange::token(&seams, &mut state.log).await {
            Ok(token) => {
                let text = token.as_str().to_owned();
                state.token = Some(token);
                Ok(text)
            }
            Err(how) => Err(FetchError::Retry {
                why: "exchange",
                status: None,
                retry_after_secs: match how {
                    Fail::Ord => None,
                    Fail::RetryAfter(secs) => Some(secs),
                    // The server refuses this agent for now: the cap's wait.
                    Fail::Cap => Some(60),
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests;

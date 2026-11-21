//! The S3 object store (design 13, 14.3; ADR-492): a bucket on the bundled object server
//! (ADR-491), reached over HTTP on `127.0.0.1` with presigned requests.
//!
//! Every request is a presigned URL built by rusty-s3 (SigV4, path style, region `us-east-1`)
//! and sent by [`http`], one request per connection. The rules the code keeps:
//!
//! - [`S3Store::open`] makes the bucket ready: HeadBucket, and CreateBucket when it is absent
//!   (an answer that the bucket exists and is ours counts as present). A refused or unusable
//!   answer ends it at once; a connection failure or a 5xx is retried every 100 ms until the
//!   ready budget (30 s) is spent.
//! - Parts are checked by the store, never trusted to the object server: before a completion the
//!   listing must be numbered consecutively from 1, every part but the last at least 5 MiB, the
//!   sizes must add up to the length the caller names, and the total may not pass 4 GiB. The
//!   completed object is then read back, streamed through SHA-256, and compared with the length
//!   and digest the caller names (the data channel's end-to-end check, design 14.3). A mismatch
//!   deletes the object first and is answered only once nothing is left.
//! - Complete and delete on one key take the key's lock (in this process only); the other calls
//!   take none. The read-back of a retry reads whatever object the key holds, so a completion
//!   whose answer was lost is finished by calling it again.
//! - Deleting a key lists the uploads whose key starts with it, aborts those whose key is exactly
//!   it, and repeats (at most three rounds, since a writer may start another upload meanwhile),
//!   then deletes the object. Deleting what is not there succeeds.
//! - No request is retried, none follows a redirect, no connection is reused, and every wait is
//!   bounded: control requests by 30 s, a body being read by 30 s of silence and no total.
//!   Responses are bounded too: XML by 4 MiB, an error body by 64 KiB (the rest is not read).
//! - Nothing is logged or put into an error from a URL, a key, an upload ID or a message the
//!   object server sent: an error is an operation, an HTTP status or an `ErrorKind`.

mod http;
#[cfg(test)]
mod tests;
mod xml;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hyper::Method;
use jiff::Timestamp;
use rusty_s3::actions::{
    CompleteMultipartUpload, CompleteMultipartUploadResponse, CreateMultipartUpload, ListParts,
};
use rusty_s3::{Bucket, Credentials as S3Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use url::Url;

use super::{
    GET_URL_EXPIRY, GetUrl, MAX_PARTS, MAX_UPLOAD_ID_LEN, MultipartStore, ObjectBody, ObjectKey,
    PART_URL_EXPIRY, Part, PartUrl, StoreError, StoreFuture, UnfinishedUpload, UnfinishedUploads,
    Upload, UploadId,
};
use crate::config::is_valid_bucket;
use crate::object_server::Credentials;
use xml::ErrorCode;

/// The region the requests are signed for (the object server's default).
const REGION: &str = "us-east-1";
/// The most rounds of listing and aborting a delete makes before it deletes the object.
const DELETE_ROUNDS: usize = 3;
/// The most pages a listing may take, so that a server that never stops truncating ends it.
const MAX_PAGES: usize = 1000;
/// The most bytes of a key or marker the store passes back to the object server.
const MAX_MARKER_LEN: usize = 1024;

/// The bounds of the store. The defaults are the constants of the module's documentation; tests
/// shorten them.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct Tuning {
    /// The budget of [`S3Store::open`] for the bucket to become ready.
    pub ready_timeout: Duration,
    /// The pause between two attempts at the bucket.
    pub retry_interval: Duration,
    /// The bound of a control request: connect, send, head and body together.
    pub request_timeout: Duration,
    /// The longest silence in a body being read.
    pub read_idle_timeout: Duration,
    /// How long the store's own presigned requests are valid.
    pub url_expiry: Duration,
    /// The page size of a listing (parts, uploads).
    pub page_size: u16,
    /// The longest XML answer read.
    pub max_xml_bytes: usize,
    /// The longest error body read.
    pub max_error_bytes: usize,
    /// The smallest part that is not the last.
    pub min_part_bytes: u64,
    /// The largest object.
    pub max_object_bytes: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            ready_timeout: Duration::from_secs(30),
            retry_interval: Duration::from_millis(100),
            request_timeout: Duration::from_secs(30),
            read_idle_timeout: Duration::from_secs(30),
            url_expiry: Duration::from_secs(60),
            page_size: 1000,
            max_xml_bytes: 4 * 1024 * 1024,
            max_error_bytes: 64 * 1024,
            min_part_bytes: 5 * 1024 * 1024,
            max_object_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// The S3 object store. Cheap to clone behind an [`Arc`].
pub struct S3Store {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S3Store")
    }
}

struct Inner {
    bucket: Bucket,
    credentials: S3Credentials,
    client: http::Client,
    tuning: Tuning,
    /// One mutex per key in use; an entry nobody holds is dropped at the next call.
    locks: Mutex<HashMap<ObjectKey, Arc<tokio::sync::Mutex<()>>>>,
}

/// What a request answered: the status, the head and the body read (bounded).
struct Reply {
    status: u16,
    body: Vec<u8>,
    /// The body was longer than the bound; the rest was not read.
    truncated: bool,
}

/// A listing, or that the upload it asked about is gone.
enum Listing<T> {
    Found(T),
    Gone,
}

/// An upload of the listing; the start time is `None` if it did not read.
struct ListedUpload {
    key: ObjectKey,
    id: UploadId,
    initiated: Option<std::time::SystemTime>,
}

/// A part with the entity tag that completing the upload names; the tag never leaves the store.
struct RawPart {
    number: u16,
    size: u64,
    etag: String,
}

impl S3Store {
    /// Makes the bucket `bucket` ready on the object server at `127.0.0.1:port` and returns the
    /// store: HeadBucket, then CreateBucket if it is absent, retried as the module's
    /// documentation says. The future owns what it needs (the credentials are copied at the
    /// call), so the caller may race it against the object server's child ending.
    pub fn open(
        port: u16,
        credentials: &Credentials,
        bucket: &str,
        tuning: Tuning,
    ) -> StoreFuture<'static, S3Store> {
        let made = Inner::new(port, credentials, bucket, tuning);
        Box::pin(async move {
            let inner = Arc::new(made?);
            inner.ready().await?;
            tracing::info!("object store ready");
            Ok(S3Store { inner })
        })
    }
}

impl Inner {
    fn new(
        port: u16,
        credentials: &Credentials,
        bucket: &str,
        tuning: Tuning,
    ) -> Result<Inner, StoreError> {
        if !is_valid_bucket(bucket) {
            return Err(StoreError::Refused {
                op: "use the bucket name",
            });
        }
        let endpoint: Url =
            format!("http://127.0.0.1:{port}")
                .parse()
                .map_err(|_| StoreError::Refused {
                    op: "build the endpoint",
                })?;
        let bucket =
            Bucket::new(endpoint, UrlStyle::Path, bucket.to_owned(), REGION).map_err(|_| {
                StoreError::Refused {
                    op: "build the bucket's URL",
                }
            })?;
        Ok(Inner {
            bucket,
            credentials: S3Credentials::new(credentials.access_key(), credentials.expose_secret()),
            client: http::Client::new(port),
            tuning,
            locks: Mutex::new(HashMap::new()),
        })
    }

    async fn lock_key(&self, key: &ObjectKey) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
            Arc::clone(locks.entry(key.clone()).or_default())
        };
        mutex.lock_owned().await
    }

    /// Sends a request and reads its whole answer (bounded by `limit` bytes), all within the
    /// control timeout.
    async fn exchange(
        &self,
        op: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<Reply, StoreError> {
        let work = async {
            let response = self.client.send(op, method, &url, body).await?;
            let status = response.status;
            // An error answer is read up to the error bound, whatever the success bound.
            let limit = if status == 200 {
                limit
            } else {
                limit.min(self.tuning.max_error_bytes)
            };
            let (body, truncated) = response.collect(limit).await?;
            Ok(Reply {
                status,
                body,
                truncated,
            })
        };
        tokio::time::timeout(self.tuning.request_timeout, work)
            .await
            .map_err(|_| StoreError::Io {
                op,
                kind: std::io::ErrorKind::TimedOut,
            })?
    }

    /// `exchange` for a request whose answer on success is XML.
    async fn exchange_xml(
        &self,
        op: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<Reply, StoreError> {
        let limit = self.tuning.max_xml_bytes;
        self.exchange(op, method, url, body, limit).await
    }

    /// `exchange` for a request whose answer is only judged by its status, and by the code of an
    /// error body.
    async fn exchange_status(
        &self,
        op: &'static str,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<Reply, StoreError> {
        let limit = self.tuning.max_error_bytes;
        self.exchange(op, method, url, body, limit).await
    }

    /// The error for a status the operation does not expect: a refusal for 401 and 403, else the
    /// status.
    fn unexpected(op: &'static str, status: u16) -> StoreError {
        match status {
            401 | 403 => StoreError::Refused { op },
            _ => StoreError::Server { op, status },
        }
    }

    fn expiry(&self) -> Duration {
        self.tuning.url_expiry
    }

    // --- the bucket ---

    /// Waits until the bucket exists (the module's first rule).
    async fn ready(&self) -> Result<(), StoreError> {
        let deadline = Instant::now() + self.tuning.ready_timeout;
        let mut last = StoreError::Io {
            op: "reach the object server",
            kind: std::io::ErrorKind::TimedOut,
        };
        loop {
            match tokio::time::timeout_at(deadline, self.ensure_bucket()).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) if is_transient(&error) => last = error,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(last),
            }
            if tokio::time::timeout_at(deadline, tokio::time::sleep(self.tuning.retry_interval))
                .await
                .is_err()
            {
                return Err(last);
            }
        }
    }

    async fn ensure_bucket(&self) -> Result<(), StoreError> {
        const OP: &str = "check the bucket";
        let url = self
            .bucket
            .head_bucket(Some(&self.credentials))
            .sign(self.expiry());
        let reply = self.exchange_status(OP, Method::HEAD, url, None).await?;
        match reply.status {
            200 => Ok(()),
            404 => self.create_bucket().await,
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    async fn create_bucket(&self) -> Result<(), StoreError> {
        const OP: &str = "create the bucket";
        let url = self
            .bucket
            .create_bucket(&self.credentials)
            .sign(self.expiry());
        let reply = self.exchange_status(OP, Method::PUT, url, None).await?;
        match reply.status {
            200 => Ok(()),
            // Another start made it between the check and the create.
            409 if xml::error_code(&reply.body) == ErrorCode::BucketAlreadyOwnedByYou => Ok(()),
            409 => Err(StoreError::Refused { op: OP }),
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    // --- begin ---

    async fn begin(&self) -> Result<Upload, StoreError> {
        const OP: &str = "start an upload";
        let key = ObjectKey::generate()?;
        let url = self
            .bucket
            .create_multipart_upload(Some(&self.credentials), key.as_str())
            .sign(self.expiry());
        let reply = self.exchange_xml(OP, Method::POST, url, None).await?;
        if reply.status != 200 {
            return Err(Inner::unexpected(OP, reply.status));
        }
        let bad = || StoreError::Server {
            op: OP,
            status: reply.status,
        };
        let text = std::str::from_utf8(&reply.body).map_err(|_| bad())?;
        if reply.truncated {
            return Err(bad());
        }
        let parsed = CreateMultipartUpload::parse_response(text).map_err(|_| bad())?;
        let id = UploadId::parse(parsed.upload_id()).map_err(|_| bad())?;
        Ok(Upload { key, id })
    }

    // --- parts ---

    fn presign_part(&self, upload: &Upload, part: u16) -> Result<PartUrl, StoreError> {
        if !(1..=MAX_PARTS).contains(&part) {
            return Err(StoreError::InvalidPartNumber);
        }
        let url = self
            .bucket
            .upload_part(
                Some(&self.credentials),
                upload.key.as_str(),
                part,
                upload.id.expose(),
            )
            .sign(PART_URL_EXPIRY);
        Ok(PartUrl::new(url.to_string()))
    }

    fn presign_get(&self, key: &ObjectKey) -> Result<GetUrl, StoreError> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key.as_str())
            .sign(GET_URL_EXPIRY);
        Ok(GetUrl::new(url.to_string()))
    }

    /// The parts of `upload` (paged), or that it is gone. The numbers are ascending, within
    /// 1 to 10000 and at most 10000 in all, or the listing is [`StoreError::PartsChanged`].
    async fn list_parts_raw(&self, upload: &Upload) -> Result<Listing<Vec<RawPart>>, StoreError> {
        const OP: &str = "list the parts";
        let mut parts: Vec<RawPart> = Vec::new();
        let mut marker: Option<u16> = None;
        for _ in 0..MAX_PAGES {
            let mut action = ListParts::new(
                &self.bucket,
                Some(&self.credentials),
                upload.key.as_str(),
                upload.id.expose(),
            );
            action.set_max_parts(self.tuning.page_size);
            if let Some(marker) = marker {
                action.set_part_number_marker(marker);
            }
            let url = action.sign(self.expiry());
            let reply = self.exchange_xml(OP, Method::GET, url, None).await?;
            match reply.status {
                200 => {}
                404 if xml::error_code(&reply.body) == ErrorCode::NoSuchUpload => {
                    return Ok(Listing::Gone);
                }
                status => return Err(Inner::unexpected(OP, status)),
            }
            let bad = || StoreError::Server {
                op: OP,
                status: 200,
            };
            if reply.truncated {
                return Err(bad());
            }
            let text = std::str::from_utf8(&reply.body).map_err(|_| bad())?;
            let page = ListParts::parse_response(text).map_err(|_| bad())?;
            for part in page.parts {
                let ascending = parts.last().is_none_or(|last| part.number > last.number);
                if !(1..=MAX_PARTS).contains(&part.number)
                    || !ascending
                    || parts.len() >= usize::from(MAX_PARTS)
                {
                    return Err(StoreError::PartsChanged);
                }
                parts.push(RawPart {
                    number: part.number,
                    size: part.size,
                    etag: part.etag,
                });
            }
            match page.next_part_number_marker {
                None => return Ok(Listing::Found(parts)),
                Some(next) => {
                    // The marker is the last part listed; one that does not move past the
                    // previous marker would repeat a page for ever.
                    if marker.is_some_and(|previous| next <= previous) {
                        return Err(StoreError::PartsChanged);
                    }
                    marker = Some(next);
                }
            }
        }
        Err(StoreError::Server {
            op: OP,
            status: 200,
        })
    }

    async fn list_parts(&self, upload: &Upload) -> Result<Vec<Part>, StoreError> {
        match self.list_parts_raw(upload).await? {
            Listing::Found(parts) => Ok(parts
                .into_iter()
                .map(|part| Part {
                    number: part.number,
                    size: part.size,
                })
                .collect()),
            Listing::Gone => {
                if self.head_object(&upload.key).await? {
                    Err(StoreError::AlreadyFinished)
                } else {
                    Err(StoreError::NotFound)
                }
            }
        }
    }

    /// Whether the object exists.
    async fn head_object(&self, key: &ObjectKey) -> Result<bool, StoreError> {
        const OP: &str = "look for the object";
        let url = self
            .bucket
            .head_object(Some(&self.credentials), key.as_str())
            .sign(self.expiry());
        let reply = self.exchange_status(OP, Method::HEAD, url, None).await?;
        match reply.status {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    // --- complete ---

    /// The store's own checks of the listing (the module's second rule).
    fn check_parts(&self, parts: &[RawPart], length: u64) -> Result<(), StoreError> {
        if parts.is_empty() {
            return Err(StoreError::NoParts);
        }
        if parts.len() > usize::from(MAX_PARTS) {
            return Err(StoreError::PartsChanged);
        }
        for (index, part) in parts.iter().enumerate() {
            let expected = u16::try_from(index + 1).map_err(|_| StoreError::PartsChanged)?;
            if part.number != expected {
                return Err(StoreError::MissingPart { part: expected });
            }
        }
        for part in &parts[..parts.len() - 1] {
            if part.size < self.tuning.min_part_bytes {
                return Err(StoreError::PartTooSmall { part: part.number });
            }
        }
        let sum = parts
            .iter()
            .try_fold(0u64, |sum, part| sum.checked_add(part.size))
            .ok_or(StoreError::ObjectTooLarge)?;
        if sum != length {
            return Err(StoreError::PartsLengthMismatch { stored: sum });
        }
        if sum > self.tuning.max_object_bytes {
            return Err(StoreError::ObjectTooLarge);
        }
        Ok(())
    }

    async fn complete(
        &self,
        upload: &Upload,
        sha256: [u8; 32],
        length: u64,
    ) -> Result<(), StoreError> {
        let _guard = self.lock_key(&upload.key).await;
        match self.list_parts_raw(upload).await? {
            Listing::Gone => {
                // A retry: the upload was completed (or aborted) already.
                if !self.head_object(&upload.key).await? {
                    return Err(StoreError::NotFound);
                }
            }
            Listing::Found(parts) => {
                self.check_parts(&parts, length)?;
                if !self.complete_request(upload, &parts).await? {
                    // The upload went away between the listing and the completion.
                    if !self.head_object(&upload.key).await? {
                        return Err(StoreError::NotFound);
                    }
                }
            }
        }
        self.verify(&upload.key, sha256, length).await
    }

    /// Sends CompleteMultipartUpload; `false` if the upload is gone.
    async fn complete_request(
        &self,
        upload: &Upload,
        parts: &[RawPart],
    ) -> Result<bool, StoreError> {
        const OP: &str = "complete the upload";
        let action = CompleteMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            upload.key.as_str(),
            upload.id.expose(),
            parts.iter().map(|part| part.etag.as_str()),
        );
        let url = action.sign(self.expiry());
        let body = action.body();
        let reply = self
            .exchange_xml(OP, Method::POST, url, Some(body.into_bytes()))
            .await?;
        match reply.status {
            200 => {
                // The object server may answer 200 and then report a failure in the body.
                let text = std::str::from_utf8(&reply.body)
                    .ok()
                    .filter(|_| !reply.truncated);
                if text.is_some_and(|text| CompleteMultipartUploadResponse::parse(text).is_ok()) {
                    Ok(true)
                } else if xml::error_code(&reply.body) == ErrorCode::InvalidPart {
                    Err(StoreError::PartsChanged)
                } else {
                    Err(StoreError::Server {
                        op: OP,
                        status: 200,
                    })
                }
            }
            404 if xml::error_code(&reply.body) == ErrorCode::NoSuchUpload => Ok(false),
            400 if xml::error_code(&reply.body) == ErrorCode::InvalidPart => {
                Err(StoreError::PartsChanged)
            }
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    /// Reads the object back and compares it with `length` and `sha256`; a mismatch deletes it
    /// and is answered once the delete has succeeded. The key's lock is held by the caller.
    async fn verify(
        &self,
        key: &ObjectKey,
        sha256: [u8; 32],
        length: u64,
    ) -> Result<(), StoreError> {
        let mismatch = {
            let body = self.get(key).await?;
            let mut reader = body.reader;
            if body.length != length {
                Some(StoreError::LengthMismatch {
                    stored: body.length,
                })
            } else {
                let mut hasher = Sha256::new();
                let mut seen = 0u64;
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = reader
                        .read(&mut buf)
                        .await
                        .map_err(|error| StoreError::Io {
                            op: "read the object back",
                            kind: error.kind(),
                        })?;
                    if n == 0 {
                        break;
                    }
                    seen += n as u64;
                    if seen > length {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
                if seen != length {
                    Some(StoreError::LengthMismatch { stored: seen })
                } else if hasher.finalize().as_slice() != sha256 {
                    Some(StoreError::HashMismatch)
                } else {
                    None
                }
            }
        };
        match mismatch {
            None => Ok(()),
            Some(error) => {
                self.delete_locked(key).await?;
                Err(error)
            }
        }
    }

    // --- get ---

    async fn get(&self, key: &ObjectKey) -> Result<ObjectBody, StoreError> {
        const OP: &str = "read the object";
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key.as_str())
            .sign(self.expiry());
        let sending = self.client.send(OP, Method::GET, &url, None);
        let response = tokio::time::timeout(self.tuning.request_timeout, sending)
            .await
            .map_err(|_| StoreError::Io {
                op: OP,
                kind: std::io::ErrorKind::TimedOut,
            })??;
        match response.status {
            200 => {}
            404 => {
                let read = tokio::time::timeout(
                    self.tuning.request_timeout,
                    response.collect(self.tuning.max_error_bytes),
                )
                .await;
                return match read {
                    Ok(Ok((body, _))) if xml::error_code(&body) == ErrorCode::NoSuchKey => {
                        Err(StoreError::NotFound)
                    }
                    Ok(Ok(_)) => Err(StoreError::Server {
                        op: OP,
                        status: 404,
                    }),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(StoreError::Io {
                        op: OP,
                        kind: std::io::ErrorKind::TimedOut,
                    }),
                };
            }
            status => return Err(Inner::unexpected(OP, status)),
        }
        let length = response.content_length().ok_or(StoreError::Server {
            op: OP,
            status: 200,
        })?;
        Ok(ObjectBody {
            length,
            reader: Box::pin(response.into_reader(self.tuning.read_idle_timeout)),
        })
    }

    // --- delete ---

    async fn delete(&self, key: &ObjectKey) -> Result<(), StoreError> {
        let _guard = self.lock_key(key).await;
        self.delete_locked(key).await
    }

    /// The delete order of the module's documentation, with the key's lock held by the caller.
    async fn delete_locked(&self, key: &ObjectKey) -> Result<(), StoreError> {
        for _ in 0..DELETE_ROUNDS {
            let (listed, _) = self.list_uploads_raw(Some(key.as_str())).await?;
            let mine: Vec<ListedUpload> = listed
                .into_iter()
                .filter(|upload| upload.key == *key)
                .collect();
            if mine.is_empty() {
                break;
            }
            for upload in mine {
                self.abort(key, &upload.id).await?;
            }
        }
        self.delete_object(key).await
    }

    async fn abort(&self, key: &ObjectKey, id: &UploadId) -> Result<(), StoreError> {
        const OP: &str = "abort an upload";
        let url = self
            .bucket
            .abort_multipart_upload(Some(&self.credentials), key.as_str(), id.expose())
            .sign(self.expiry());
        let reply = self.exchange_status(OP, Method::DELETE, url, None).await?;
        match reply.status {
            200 | 204 => Ok(()),
            404 if xml::error_code(&reply.body) == ErrorCode::NoSuchUpload => Ok(()),
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    async fn delete_object(&self, key: &ObjectKey) -> Result<(), StoreError> {
        const OP: &str = "delete the object";
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), key.as_str())
            .sign(self.expiry());
        let reply = self.exchange_status(OP, Method::DELETE, url, None).await?;
        match reply.status {
            200 | 204 => Ok(()),
            404 if xml::error_code(&reply.body) == ErrorCode::NoSuchKey => Ok(()),
            status => Err(Inner::unexpected(OP, status)),
        }
    }

    // --- uploads ---

    /// ListMultipartUploads, paged, with the request built by hand (rusty-s3 has no action for
    /// it): the signed query is `key-marker`, `max-uploads`, `prefix`, `upload-id-marker` and
    /// `uploads`, in that order, which is sorted by name.
    async fn list_uploads(&self, prefix: Option<&str>) -> Result<UnfinishedUploads, StoreError> {
        let (listed, mut skipped) = self.list_uploads_raw(prefix).await?;
        let mut uploads = Vec::new();
        for upload in listed {
            match upload.initiated {
                Some(initiated) => uploads.push(UnfinishedUpload {
                    key: upload.key,
                    id: upload.id,
                    initiated,
                }),
                None => skipped += 1,
            }
        }
        Ok(UnfinishedUploads { uploads, skipped })
    }

    /// The listing as the object server gives it: every upload whose key and ID this store could
    /// have issued, with its start time if that reads, and the count of the others. The delete
    /// goes by key and ID only, so an upload with an unreadable time is still aborted.
    async fn list_uploads_raw(
        &self,
        prefix: Option<&str>,
    ) -> Result<(Vec<ListedUpload>, usize), StoreError> {
        const OP: &str = "list the uploads";
        let bad = |status| StoreError::Server { op: OP, status };
        let max_uploads = self.tuning.page_size.to_string();
        let mut found = Vec::new();
        let mut skipped = 0usize;
        let mut markers: Option<(String, String)> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, &str)> = Vec::new();
            if let Some((key, _)) = &markers {
                query.push(("key-marker", key));
            }
            query.push(("max-uploads", &max_uploads));
            if let Some(prefix) = prefix {
                query.push(("prefix", prefix));
            }
            if let Some((_, id)) = &markers
                && !id.is_empty()
            {
                query.push(("upload-id-marker", id));
            }
            query.push(("uploads", ""));
            let url = rusty_s3::signing::sign(
                &Timestamp::now(),
                rusty_s3::Method::Get,
                self.bucket.base_url().clone(),
                self.credentials.key(),
                self.credentials.secret(),
                None,
                REGION,
                self.expiry().as_secs(),
                query.iter().copied(),
                std::iter::empty::<(&str, &str)>(),
            );
            let reply = self.exchange_xml(OP, Method::GET, url, None).await?;
            if reply.status != 200 {
                return Err(Inner::unexpected(OP, reply.status));
            }
            if reply.truncated {
                return Err(bad(200));
            }
            let text = std::str::from_utf8(&reply.body).map_err(|_| bad(200))?;
            let page = xml::parse_uploads(text).ok_or_else(|| bad(200))?;
            for entry in page.uploads {
                let initiated = entry.initiated.parse::<Timestamp>().ok();
                let key = ObjectKey::parse(&entry.key).ok();
                let id = UploadId::parse(&entry.upload_id).ok();
                match (key, id) {
                    (Some(key), Some(id)) => found.push(ListedUpload {
                        key,
                        id,
                        initiated: initiated.map(Into::into),
                    }),
                    _ => skipped += 1,
                }
            }
            if !page.is_truncated {
                return Ok((found, skipped));
            }
            let next_key = page.next_key_marker.filter(|key| !key.is_empty());
            let next_id = page.next_upload_id_marker.unwrap_or_default();
            let Some(next_key) = next_key else {
                return Err(bad(200));
            };
            if next_key.len() > MAX_MARKER_LEN || next_id.len() > MAX_UPLOAD_ID_LEN {
                return Err(bad(200));
            }
            let next = (next_key, next_id);
            // A marker that does not move would repeat a page for ever.
            if markers.as_ref() == Some(&next) {
                return Err(bad(200));
            }
            markers = Some(next);
        }
        Err(bad(200))
    }
}

/// Whether a failed attempt at the bucket may succeed when repeated: a connection that failed, a
/// request that timed out, a server that is not up yet.
fn is_transient(error: &StoreError) -> bool {
    matches!(error, StoreError::Io { .. })
        || matches!(error, StoreError::Server { status, .. } if *status >= 500)
}

impl MultipartStore for S3Store {
    fn begin(&self) -> StoreFuture<'_, Upload> {
        Box::pin(self.inner.begin())
    }

    fn presign_part(&self, upload: &Upload, part: u16) -> Result<PartUrl, StoreError> {
        self.inner.presign_part(upload, part)
    }

    fn presign_get(&self, key: &ObjectKey) -> Result<GetUrl, StoreError> {
        self.inner.presign_get(key)
    }

    fn list_parts<'a>(&'a self, upload: &'a Upload) -> StoreFuture<'a, Vec<Part>> {
        Box::pin(self.inner.list_parts(upload))
    }

    fn complete<'a>(
        &'a self,
        upload: &'a Upload,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()> {
        Box::pin(self.inner.complete(upload, sha256, length))
    }

    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody> {
        Box::pin(self.inner.get(key))
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()> {
        Box::pin(self.inner.delete(key))
    }

    fn list_unfinished(&self) -> StoreFuture<'_, UnfinishedUploads> {
        Box::pin(self.inner.list_uploads(None))
    }
}

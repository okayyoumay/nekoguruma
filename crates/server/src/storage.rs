//! The server's object storage (design 13, 14.3; ADR-474, ADR-492): large bodies (artifacts,
//! data-channel chunks, job results) that the database holds only a key for. The `storage_key`
//! columns hold the key this module issued.
//!
//! [`MultipartStore`] is the contract the server uses; [`S3Store`] is its implementation, a bucket
//! on the bundled S3-compatible object server (ADR-491). The caller does not send the bytes
//! through the store: it gets a presigned part URL ([`MultipartStore::presign_part`]) that the
//! writer, the agent, PUTs a part to, and the store completes the upload and verifies the result
//! ([`MultipartStore::complete`]). The steps:
//!
//! 1. [`MultipartStore::begin`] issues a key and starts a multipart upload; the caller keeps both
//!    in its row ([`Upload`]).
//! 2. [`MultipartStore::presign_part`] gives the URL for one part, valid for a few minutes.
//!    Nothing is sent to the object server.
//! 3. [`MultipartStore::list_parts`] tells what the object server holds, so that a writer that
//!    lost its place resumes.
//! 4. [`MultipartStore::complete`] checks the parts, completes the upload, reads the object back
//!    and compares its length and SHA-256; a mismatch deletes the object.
//!
//! [`MultipartStore::get`] reads a finished object, [`MultipartStore::delete`] removes an object in
//! either state and [`MultipartStore::list_unfinished`] lists the uploads a writer left behind.
//!
//! [`ObjectStore`] and [`LocalStore`], a directory of files with resumable append by offset, are
//! the earlier contract (ADR-474). The server does not use them any more; they stay for their
//! tests and as the owner of the directory-locking code the object server's data directory shares.
//!
//! Errors are fixed text, an HTTP status and, for a failed operation, the [`std::io::ErrorKind`]:
//! never a path, a URL, a key, an upload ID or text the object server sent.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::SystemTime;

use tokio::io::AsyncRead;

pub(crate) mod local;
pub mod s3;

pub use local::{LocalStore, StorageConfig};
pub use s3::{S3Store, Tuning as S3Tuning};

/// The boxed future of an [`ObjectStore`] or [`MultipartStore`] method. Written by hand so that the trait stays object
/// safe without a macro crate.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// The key of an object: a version 4 UUID in canonical lowercase text, issued by
/// [`ObjectStore::begin`] or [`MultipartStore::begin`]. It is the only text a store builds a path
/// or a URL from, and it is validated before any file is touched or request is made.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// A key from its text: 36 ASCII characters, hyphens at 8, 13, 18 and 23, lowercase
    /// hexadecimal elsewhere, version 4 and an RFC 4122 variant. Anything else is refused.
    pub fn parse(text: &str) -> Result<ObjectKey, StoreError> {
        let bytes = text.as_bytes();
        let shaped = bytes.len() == 36
            && bytes.iter().enumerate().all(|(i, &b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
            })
            && bytes[14] == b'4'
            && matches!(bytes[19], b'8' | b'9' | b'a' | b'b');
        if shaped {
            Ok(ObjectKey(text.to_owned()))
        } else {
            Err(StoreError::InvalidKey)
        }
    }

    /// A new random key (the generator is the operating system's).
    pub(crate) fn generate() -> Result<ObjectKey, StoreError> {
        let random = shared_crypto::random_32().map_err(|_| StoreError::Random)?;
        let mut b = [0u8; 16];
        b.copy_from_slice(&random[..16]);
        b[6] = b[6] & 0x0f | 0x40;
        b[8] = b[8] & 0x3f | 0x80;
        let mut text = String::with_capacity(36);
        for (i, byte) in b.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                text.push('-');
            }
            text.push_str(&format!("{byte:02x}"));
        }
        Ok(ObjectKey(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an object stands: what [`ObjectStore::status`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectStatus {
    /// Writable; `stored` bytes are durable.
    Unfinished { stored: u64 },
    /// Finished, visible and immutable.
    Finished,
}

/// An unfinished object, as [`ObjectStore::list_unfinished`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedObject {
    pub key: ObjectKey,
    /// The durable length.
    pub stored: u64,
    /// When the object last changed.
    pub modified: SystemTime,
}

/// A finished object being read.
pub struct ObjectBody {
    /// The object's length in bytes.
    pub length: u64,
    /// The content. It stays complete if the object is deleted while it is read (Unix). On
    /// Windows, std's removal asks for POSIX delete semantics where the file system has them
    /// (NTFS), which also frees the name at once, but on a file system without them the name
    /// may persist until the reader's handle closes, and `status` of the key may then answer
    /// finished or an I/O error until then. The store does not special-case that state.
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
}

impl std::fmt::Debug for ObjectBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectBody")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// Why an object store call failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the object key is not a canonical version 4 UUID")]
    InvalidKey,
    #[error("no such object")]
    NotFound,
    #[error("the object is not finished")]
    NotFinished,
    #[error("the object is finished")]
    AlreadyFinished,
    /// The offset is not the stored length; the writer resumes at `stored`.
    #[error("the offset is not the stored length ({stored} bytes are stored)")]
    OffsetMismatch { stored: u64 },
    #[error("the append would pass the size limit of {max} bytes ({stored} bytes are stored)")]
    TooLarge { stored: u64, max: u64 },
    #[error("the length is not the stored length ({stored} bytes are stored)")]
    LengthMismatch { stored: u64 },
    #[error("the SHA-256 is not that of the stored content")]
    HashMismatch,
    /// The parts' sizes do not add up to the declared length. Answered before the upload is
    /// completed: the upload and its parts are still there, so the writer can correct a part
    /// and complete again.
    #[error("the parts hold {stored} bytes, not the declared length")]
    PartsLengthMismatch { stored: u64 },
    #[error("the upload ID is not 1 to 1024 printable ASCII characters")]
    InvalidUploadId,
    #[error("the part number is not from 1 to 10000")]
    InvalidPartNumber,
    #[error("the upload has no parts")]
    NoParts,
    #[error("part {part} is missing: the parts are not numbered consecutively from 1")]
    MissingPart { part: u16 },
    #[error("part {part} is smaller than the minimum and is not the last")]
    PartTooSmall { part: u16 },
    #[error("the parts changed while the upload was being completed")]
    PartsChanged,
    #[error("the object would be larger than the size limit")]
    ObjectTooLarge,
    /// The object server turned the request down: the credentials are not accepted, or the
    /// request is not allowed.
    #[error("the object server refused to {op}")]
    Refused { op: &'static str },
    /// The object server answered with a status the operation does not expect, or with an answer
    /// that could not be read.
    #[error("the object server answered {status} to {op}")]
    Server { op: &'static str, status: u16 },
    /// The store's directory, or one of its areas, is gone.
    #[error("the object store's directory is unavailable")]
    Unavailable,
    #[error("another process uses the object store's directory")]
    InUse,
    #[error("cannot {op} in the object store: {kind}")]
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    #[error("the object store's directory must be an absolute path")]
    DirectoryNotAbsolute,
    #[error("the object store's directory is not a directory")]
    NotADirectory,
    #[error(
        "the object store's directory is owned by neither the account the server runs as nor root"
    )]
    DirectoryNotOwned,
    #[error(
        "the object store's directory is writable by group or others; it must be writable by the \
         server's account only"
    )]
    DirectoryWritable,
    #[error("an object store file is not a regular file (a symbolic link is refused)")]
    NotRegularFile,
    #[error("operating-system randomness is unavailable")]
    Random,
}

/// The ID the object server gave a multipart upload. It is a bearer of access to the upload, so
/// `Debug` shows nothing of it and there is no `Display`; the caller keeps it in its row
/// ([`UploadId::expose`]) and never logs it.
#[derive(Clone, PartialEq, Eq)]
pub struct UploadId(String);

/// The longest upload ID the store accepts, in characters.
pub(crate) const MAX_UPLOAD_ID_LEN: usize = 1024;

impl UploadId {
    /// An ID from its text (a row read back): 1 to 1024 printable ASCII characters.
    pub fn parse(text: &str) -> Result<UploadId, StoreError> {
        if text.is_empty()
            || text.len() > MAX_UPLOAD_ID_LEN
            || !text.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Err(StoreError::InvalidUploadId);
        }
        Ok(UploadId(text.to_owned()))
    }

    /// The ID as written, for the caller's row. It must not be logged.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for UploadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadId(..)")
    }
}

/// A multipart upload: the key the object will have and the ID the object server gave the upload.
/// [`MultipartStore::begin`] returns it and the caller's row keeps both; the store itself keeps
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub key: ObjectKey,
    pub id: UploadId,
}

/// A presigned URL a part is PUT to. It carries its signature in the query, so `Debug` shows
/// nothing of it and there is no `Display`; [`PartUrl::expose`] gives the text to hand to the
/// writer, and it must not be logged.
#[derive(Clone, PartialEq, Eq)]
pub struct PartUrl(String);

impl PartUrl {
    pub(crate) fn new(url: String) -> PartUrl {
        PartUrl(url)
    }

    /// The URL as signed.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PartUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PartUrl(..)")
    }
}

/// A part the object server holds. The entity tags it also holds do not leave the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    /// From 1 to 10000.
    pub number: u16,
    /// The part's length in bytes.
    pub size: u64,
}

/// An upload [`MultipartStore::list_unfinished`] lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedUpload {
    pub key: ObjectKey,
    pub id: UploadId,
    /// When the upload was started.
    pub initiated: SystemTime,
}

/// The answer of [`MultipartStore::list_unfinished`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfinishedUploads {
    pub uploads: Vec<UnfinishedUpload>,
    /// How many listed uploads were left out because their key or ID is not one this store issues
    /// or their time cannot be read (the bucket may hold others' uploads).
    pub skipped: usize,
}

/// The most parts an upload may have.
pub const MAX_PARTS: u16 = 10_000;

/// How long a presigned part URL is valid.
pub const PART_URL_EXPIRY: std::time::Duration = std::time::Duration::from_secs(300);

/// An object store whose writer sends the bytes itself, in parts, to presigned URLs. Calls on one
/// key that complete or delete it are serialised (in this process only); the others run
/// concurrently. A call whose future is dropped may have done part of its work: every step is
/// safe to repeat.
pub trait MultipartStore: Send + Sync {
    /// Issues a key and starts an upload. The store records nothing: the caller's row keeps the
    /// result.
    fn begin(&self) -> StoreFuture<'_, Upload>;

    /// The URL to PUT part `part` (from 1 to 10000, else [`StoreError::InvalidPartNumber`]) of
    /// `upload` to, valid for [`PART_URL_EXPIRY`]. Computes a signature and does no I/O. The URL
    /// does not bind the part's length.
    fn presign_part(&self, upload: &Upload, part: u16) -> Result<PartUrl, StoreError>;

    /// The parts the object server holds for `upload`, in part order. An upload that is gone
    /// while its object exists is [`StoreError::AlreadyFinished`]; gone with no object,
    /// [`StoreError::NotFound`].
    fn list_parts<'a>(&'a self, upload: &'a Upload) -> StoreFuture<'a, Vec<Part>>;

    /// Completes `upload` as an object of `length` bytes whose SHA-256 is `sha256`: the parts are
    /// listed and checked (numbered consecutively from 1, each but the last at least the minimum
    /// size, their sizes adding up to `length`, at most 4 GiB and 10000 parts; a refusal of the
    /// listing, including [`StoreError::PartsLengthMismatch`], leaves the upload as it is), the
    /// upload is completed and the object is read back and compared. A mismatch deletes the
    /// object, and [`StoreError::HashMismatch`] or [`StoreError::LengthMismatch`] (the
    /// read-back's length) is answered only once nothing is left; if the delete fails, its error
    /// is answered. A retry after the upload was
    /// completed (the answer lost, the read-back cut) reads the object back again.
    fn complete<'a>(
        &'a self,
        upload: &'a Upload,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()>;

    /// Opens an object for reading. Nothing is verified here: the content is what the object
    /// server holds, and a read that stalls for the idle timeout fails.
    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody>;

    /// Removes an object, finished or not: the uploads of the key are aborted, then the object is
    /// deleted. Deleting what is not there is `Ok`.
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()>;

    /// The uploads that were started and not completed or aborted.
    fn list_unfinished(&self) -> StoreFuture<'_, UnfinishedUploads>;
}

/// An object store. Every method is durable before it answers: a successful `append` has synced
/// its bytes, a successful `finish` has synced the move.
///
/// Calls on one key are serialised; calls on different keys run concurrently. A call whose future
/// is dropped still runs to its end.
pub trait ObjectStore: Send + Sync {
    /// Issues a key and an empty unfinished object.
    fn begin(&self) -> StoreFuture<'_, ObjectKey>;

    /// Appends `bytes` when `offset` is the stored length, and returns the new length. A different
    /// offset is [`StoreError::OffsetMismatch`] with the stored length and writes nothing. An
    /// append that would pass the size limit is [`StoreError::TooLarge`]. Appending nothing at
    /// the stored length returns it.
    fn append<'a>(
        &'a self,
        key: &'a ObjectKey,
        offset: u64,
        bytes: Vec<u8>,
    ) -> StoreFuture<'a, u64>;

    fn status<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectStatus>;

    /// Finishes an unfinished object whose stored length is `length` and whose content hashes to
    /// `sha256`. A mismatch leaves the object unfinished.
    fn finish<'a>(
        &'a self,
        key: &'a ObjectKey,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()>;

    /// Opens a finished object for reading.
    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody>;

    /// Removes an object, finished or not; a key in both areas loses both copies. An I/O error
    /// that comes after the unlink (the directory sync) leaves the object gone, so a retry
    /// answers `NotFound`, which then means it is gone.
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()>;

    /// The unfinished objects.
    fn list_unfinished(&self) -> StoreFuture<'_, Vec<UnfinishedObject>>;
}

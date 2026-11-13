//! The server's object storage (design 13, 14.3; ADR-474): large bodies (artifacts, data-channel
//! chunks, job results) that the database holds only a key for. The `storage_key` columns hold the
//! key this module issued.
//!
//! [`ObjectStore`] is the contract; [`LocalStore`] is the local profile's implementation, a
//! directory of files. An S3-compatible store for the standard profile implements the same trait
//! later.
//!
//! Writing is a resumable upload by offset:
//!
//! 1. [`ObjectStore::begin`] issues a new key and an empty, unfinished object.
//! 2. [`ObjectStore::append`] adds bytes at an offset. It succeeds only when the offset is the
//!    length the store holds, and answers with the new length; any other offset is refused with
//!    the stored length, so a writer whose connection was cut (or whose answer was lost) asks
//!    [`ObjectStore::status`] or reads the refusal, and resumes from there.
//! 3. [`ObjectStore::finish`] checks the length and the SHA-256 of the whole content and makes the
//!    object visible and immutable.
//!
//! Only finished objects are readable ([`ObjectStore::get`]). [`ObjectStore::delete`] removes an
//! object in either state. [`ObjectStore::list_unfinished`] lists what a writer left behind.
//!
//! Errors are fixed text and, for a failed file operation, the [`std::io::ErrorKind`]: never a
//! path.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::time::SystemTime;

use tokio::io::AsyncRead;

mod local;

pub use local::LocalStore;

/// The boxed future of an [`ObjectStore`] method. Written by hand so that the trait stays object
/// safe without a macro crate.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// The key of an object: a version 4 UUID in canonical lowercase text, issued by
/// [`ObjectStore::begin`]. It is the only text a store builds a path from, and it is validated
/// before any file is touched.
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
    /// The content. It stays complete if the object is deleted while it is read (Unix).
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
    #[error("storage.dir must be an absolute path")]
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

    /// Removes an object, finished or not.
    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()>;

    /// The unfinished objects.
    fn list_unfinished(&self) -> StoreFuture<'_, Vec<UnfinishedObject>>;
}

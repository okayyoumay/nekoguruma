//! The object cache on disk (design 4.1, 8.2.6, ADR-497 item 7): `<state>/objects/documents/<hex>`,
//! `<state>/objects/artifacts/<hex>` and the download's working area `<state>/objects/tmp/`.
//!
//! - **One owner.** [`ObjectStore::open`] takes the directory's lock (one per state root), so two
//!   processes never write the cache or sweep `tmp` under each other. The sweep of `tmp` runs
//!   under that lock, so it never removes a live download's file.
//! - **A file is never written in place.** A download is written to `tmp/<random>` while it is
//!   hashed. Only a match is synced, renamed to its digest's name and made durable by syncing the
//!   directory.
//! - **A cached file is trusted only for the bytes just hashed.** [`ObjectStore::load`] and
//!   [`ObjectStore::read`] hash the file against the expected digest on every call and delete one
//!   that differs, which they then report as a miss. A name that is not a regular file, or a file
//!   that cannot be read, is a local failure and nothing is deleted.
//!
//! Directories and files are owner-only. Nothing here logs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use worker_host::service::OperatingMode;

use super::Kind;
use crate::journal::{JournalError, WriterLock, sync_dir};
use crate::state::{self, StateDirError};

/// The state sub-directory the cache lives in.
pub const DIR_NAME: &str = "objects";
const LOCK_FILE: &str = "objects.lock";
const TMP_DIR: &str = "tmp";

/// Why the cache cannot be used. The text names no path.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the object cache is open in another process")]
    InUse,
    #[error("the object cache's directory is unusable: {0}")]
    Dir(#[from] StateDirError),
    #[error("the object cache cannot be used: {0:?}")]
    Io(io::ErrorKind),
    #[error("a cached object's name is not a regular file")]
    NotRegular,
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io(error.kind())
    }
}

/// What a lookup found.
#[derive(Debug, PartialEq, Eq)]
pub enum Load {
    /// The file's bytes hash to the digest; its length is as given.
    Hit { path: PathBuf, length: u64 },
    /// No file.
    Miss,
    /// A file whose bytes did not hash to the digest, or were over the bound: it was deleted.
    Deleted,
}

/// A download in progress: the file under `tmp/`, deleted on drop unless it was committed.
#[derive(Debug)]
pub struct Tmp {
    path: PathBuf,
    keep: bool,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        if !self.keep {
            // Best effort: a stop drops a download mid-way, and the sweep at the next open
            // removes what this cannot.
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The open cache and its lock.
pub struct ObjectStore {
    root: PathBuf,
    _lock: WriterLock,
}

impl ObjectStore {
    /// Opens the cache of `mode`'s state root, creating its directories (owner-only), taking the
    /// lock and sweeping `tmp`.
    pub fn open(mode: OperatingMode) -> Result<Self, StoreError> {
        Self::open_in(&state::private_subdir(mode, DIR_NAME)?)
    }

    /// [`ObjectStore::open`] in an existing `objects` directory.
    pub fn open_in(root: &Path) -> Result<Self, StoreError> {
        let lock = WriterLock::at(&root.join(LOCK_FILE)).map_err(|error| match error {
            JournalError::InUse => StoreError::InUse,
            JournalError::Io(error) => StoreError::Io(error.kind()),
            _ => StoreError::Io(io::ErrorKind::Other),
        })?;
        for name in [Kind::Document.dir(), Kind::Artifact.dir(), TMP_DIR] {
            state::prepare_subdir(root, false, name)?;
        }
        let store = Self {
            root: root.to_owned(),
            _lock: lock,
        };
        store.sweep()?;
        Ok(store)
    }

    /// Removes what an earlier process left in `tmp/`. Under the lock, so nothing live is there.
    fn sweep(&self) -> Result<(), StoreError> {
        for entry in fs::read_dir(self.root.join(TMP_DIR))? {
            let path = entry?.path();
            // Whatever is there is removed, a directory too: only this agent writes there.
            if fs::remove_file(&path).is_err() {
                fs::remove_dir_all(&path)?;
            }
        }
        Ok(())
    }

    fn path(&self, kind: Kind, digest: &[u8; 32]) -> PathBuf {
        self.root.join(kind.dir()).join(hex(digest))
    }

    /// A new file under `tmp/` (created with `create_new`, owner-only) and its guard.
    pub fn begin(&self) -> Result<(Tmp, File), StoreError> {
        let name = shared_crypto::random_32()
            .map_err(|_| StoreError::Io(io::ErrorKind::Other))
            .map(|bytes| hex(&bytes))?;
        let path = self.root.join(TMP_DIR).join(name);
        let file = create_private(&path)?;
        Ok((Tmp { path, keep: false }, file))
    }

    /// Makes the finished download `tmp` (written through `file`, whose bytes hashed to `digest`)
    /// the cached object: sync the file, rename it to its name, sync the directory.
    pub fn commit(
        &self,
        mut tmp: Tmp,
        file: File,
        kind: Kind,
        digest: &[u8; 32],
    ) -> Result<PathBuf, StoreError> {
        file.sync_all()?;
        drop(file);
        let target = self.path(kind, digest);
        fs::rename(&tmp.path, &target)?;
        tmp.keep = true;
        sync_dir(&self.root.join(kind.dir()))?;
        Ok(target)
    }

    /// Looks for the object and hashes it (in chunks, keeping nothing) against `digest`, within
    /// `max` bytes.
    pub fn load(&self, kind: Kind, digest: &[u8; 32], max: u64) -> Result<Load, StoreError> {
        let path = self.path(kind, digest);
        let Some(mut file) = open_regular(&path)? else {
            return Ok(Load::Miss);
        };
        let mut hasher = Sha256::new();
        let mut length = 0u64;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            length += n as u64;
            if length > max {
                return Ok(self.delete(&path));
            }
            hasher.update(&chunk[..n]);
        }
        if hasher.finalize().as_slice() == digest {
            Ok(Load::Hit { path, length })
        } else {
            Ok(self.delete(&path))
        }
    }

    /// The bytes of a cached object read within `max` and hashed against `digest`: the only
    /// interface that returns bytes, and it returns only bytes that matched. `Ok(None)` for a
    /// miss; a file that does not match is deleted and is a miss too.
    pub fn read(
        &self,
        kind: Kind,
        digest: &[u8; 32],
        max: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path(kind, digest);
        let Some(file) = open_regular(&path)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 <= max && Sha256::digest(&bytes).as_slice() == digest {
            Ok(Some(bytes))
        } else {
            self.delete(&path);
            Ok(None)
        }
    }

    fn delete(&self, path: &Path) -> Load {
        // A file that cannot be removed is still not served; the download renames over it.
        let _ = fs::remove_file(path);
        Load::Deleted
    }
}

impl std::fmt::Debug for ObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStore").finish_non_exhaustive()
    }
}

/// The file at `path` opened for reading, `None` when there is none, and an error when the name
/// is not a regular file (a link, a directory) or cannot be read. Nothing is deleted.
fn open_regular(path: &Path) -> Result<Option<File>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(StoreError::NotRegular),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(File::open(path)?))
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// 64 lowercase hexadecimal digits.
pub fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A writer that hashes and counts what passes through it.
pub struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    length: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            length: 0,
        }
    }

    /// The inner writer, the digest of everything written and its length.
    pub fn finish(self) -> (W, [u8; 32], u64) {
        (self.inner, self.hasher.finalize().into(), self.length)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.length += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W> std::fmt::Debug for HashingWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashingWriter")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;

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
//! Directories and files are owner-only. The only line written is one warning when the sweep
//! cannot remove something.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use worker_host::service::OperatingMode;

use super::Kind;
use crate::journal::{JournalError, WriterLock, sync_after_rename};
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
    /// The random source gave nothing for a temporary file's name.
    #[error("the object cache cannot name a temporary file")]
    Random(#[source] shared_crypto::RandomError),
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io(error.kind())
    }
}

/// What finishing a download came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    /// The digest matched: the file is in the cache.
    Done { path: PathBuf, length: u64 },
    /// The digest differs from the expected one: nothing was stored.
    Mismatch,
}

/// What a lookup found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Load {
    /// The file's bytes hash to the digest; its length is as given.
    Hit { path: PathBuf, length: u64 },
    /// No file.
    Miss,
    /// A file whose bytes did not hash to the digest, or were over the bound: it was deleted.
    Deleted,
}

/// A download in progress: the file under `tmp/`, deleted on drop unless it was committed.
#[derive(Debug)]
pub(crate) struct Tmp {
    path: PathBuf,
    keep: bool,
}

impl Drop for Tmp {
    fn drop(&mut self) {
        if !self.keep && fs::remove_file(&self.path).is_err() && self.path.exists() {
            // A stop drops a download mid-way while the writer thread may still hold the file
            // (Windows does not delete an open file): try again for a moment on a thread of
            // its own, and leave what remains to the sweep at the next open.
            let path = self.path.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    if fs::remove_file(&path).is_ok() || !path.exists() {
                        return;
                    }
                }
            });
        }
    }
}

/// The open cache and its lock.
pub struct ObjectStore {
    root: PathBuf,
    _lock: WriterLock,
    /// Tests only: the bytes a download may write before its writes fail.
    #[cfg(test)]
    fail_writes_after: std::sync::Mutex<Option<u64>>,
}

/// The file a download is written to. Tests can make it fail as a full disk does.
pub(crate) struct Sink {
    file: File,
    #[cfg(test)]
    fail_after: Option<u64>,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        if let Some(left) = self.fail_after.as_mut() {
            if *left == 0 {
                return Err(io::ErrorKind::StorageFull.into());
            }
            let n = buf.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
            *left -= n as u64;
            return self.file.write(&buf[..n]);
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
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
            #[cfg(test)]
            fail_writes_after: std::sync::Mutex::new(None),
        };
        store.sweep(|path| fs::remove_file(path).or_else(|_| fs::remove_dir_all(path)));
        Ok(store)
    }

    /// Removes what an earlier process left in `tmp/`, with `remove`. Under the lock, so nothing
    /// live is there. Best effort: an entry that cannot be removed is warned about once and the
    /// sweep goes on, since a leftover file costs disk and must not keep the agent from fetching.
    fn sweep(&self, remove: impl Fn(&Path) -> io::Result<()>) {
        let mut failed = false;
        match fs::read_dir(self.root.join(TMP_DIR)) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => failed |= remove(&entry.path()).is_err(),
                        Err(_) => failed = true,
                    }
                }
            }
            Err(_) => failed = true,
        }
        if failed {
            tracing::warn!("the object cache's temporary directory could not be swept clean");
        }
    }

    fn path(&self, kind: Kind, digest: &[u8; 32]) -> PathBuf {
        self.root.join(kind.dir()).join(hex(digest))
    }

    /// Tests only: downloads begun from now on fail their writes after `bytes` bytes.
    #[cfg(test)]
    pub(crate) fn fail_writes_after(&self, bytes: u64) {
        *self.fail_writes_after.lock().unwrap() = Some(bytes);
    }

    /// A new file under `tmp/` (created with `create_new`, owner-only) and its guard.
    pub(crate) fn begin(&self) -> Result<(Tmp, Sink), StoreError> {
        let name = shared_crypto::random_32()
            .map_err(StoreError::Random)
            .map(|bytes| hex(&bytes))?;
        let path = self.root.join(TMP_DIR).join(name);
        let file = create_private(&path)?;
        let sink = Sink {
            file,
            #[cfg(test)]
            fail_after: *self.fail_writes_after.lock().unwrap(),
        };
        Ok((Tmp { path, keep: false }, sink))
    }

    /// Finishes the download `tmp` written through `writer`: when the digest the writer
    /// computed is `expected`, syncs the file, renames it to that digest and makes the rename
    /// durable; otherwise the download is dropped (and so deleted).
    pub(crate) fn commit(
        &self,
        mut tmp: Tmp,
        writer: HashingWriter<Sink>,
        kind: Kind,
        expected: &[u8; 32],
    ) -> Result<Stored, StoreError> {
        let (sink, digest, length) = writer.finish();
        if digest != *expected {
            return Ok(Stored::Mismatch);
        }
        sink.file.sync_all()?;
        drop(sink);
        let target = self.path(kind, &digest);
        fs::rename(&tmp.path, &target)?;
        tmp.keep = true;
        sync_after_rename(&self.root.join(kind.dir()), &target)?;
        Ok(Stored::Done {
            path: target,
            length,
        })
    }

    /// Looks for the object and hashes it (in chunks, keeping nothing) against `digest`, within
    /// `max` bytes.
    pub(crate) fn load(&self, kind: Kind, digest: &[u8; 32], max: u64) -> Result<Load, StoreError> {
        self.check(kind, digest, max, false).map(|(load, _)| load)
    }

    /// [`ObjectStore::load`] that also returns the bytes: the only interface that does, and it
    /// returns only bytes that matched, read once and hashed once. A file that does not match is
    /// deleted ([`Load::Deleted`]).
    pub(crate) fn read(
        &self,
        kind: Kind,
        digest: &[u8; 32],
        max: u64,
    ) -> Result<(Load, Vec<u8>), StoreError> {
        self.check(kind, digest, max, true)
    }

    fn check(
        &self,
        kind: Kind,
        digest: &[u8; 32],
        max: u64,
        keep: bool,
    ) -> Result<(Load, Vec<u8>), StoreError> {
        let path = self.path(kind, digest);
        let Some(mut file) = open_regular(&path)? else {
            return Ok((Load::Miss, Vec::new()));
        };
        let mut hasher = Sha256::new();
        let mut bytes = Vec::new();
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
                return Ok((self.delete(&path), Vec::new()));
            }
            hasher.update(&chunk[..n]);
            if keep {
                bytes.extend_from_slice(&chunk[..n]);
            }
        }
        if hasher.finalize().as_slice() == digest {
            Ok((Load::Hit { path, length }, bytes))
        } else {
            Ok((self.delete(&path), Vec::new()))
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
/// is not a regular file (a link, a directory, a device) or cannot be read. Nothing is deleted.
/// The kind is judged on the opened file itself, so it cannot change between a check and the
/// open: on Unix the open does not follow a link and does not block on a pipe, and on Windows it
/// opens a link itself rather than its target.
fn open_regular(path: &Path) -> Result<Option<File>, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT, and FILE_FLAG_BACKUP_SEMANTICS so a directory opens
        // (and is then refused as not a file) instead of failing with an access error.
        options.custom_flags(0x0020_0000 | 0x0200_0000);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(StoreError::NotRegular);
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(StoreError::NotRegular);
    }
    Ok(Some(file))
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

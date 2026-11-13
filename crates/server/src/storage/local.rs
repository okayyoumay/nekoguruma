//! The local profile's object store (design 13, 14.3; ADR-474): files under `storage.dir`.
//!
//! ```text
//! <dir>/staging/<key>   an unfinished object, one file
//! <dir>/objects/<key>   a finished object, moved here by rename
//! <dir>/.lock           the lock that makes one process the directory's only user
//! ```
//!
//! The rules the code keeps:
//!
//! - The stored length of an unfinished object is the length of its staging file after one
//!   `sync_all`, read under the key's lock by [`Inner::stored_length`], which every reader uses
//!   (the append's offset check and its refusal value, the limit check, `status`, the listing and
//!   `finish`'s length check). Nothing records a length, and nothing truncates or preallocates a
//!   file, so after a crash the length is what reached the disk.
//! - A key whose `sync_all` failed answers an I/O error to everything but `delete` until the
//!   process restarts: a failed sync can drop data the operating system had accepted, so a later
//!   length could claim what was lost (the journal of the agent has the same rule).
//! - The state of a key is one function ([`Inner::state`]): finished if `objects/<key>` exists,
//!   else unfinished if `staging/<key>` exists, else not found. A missing area directory is
//!   [`StoreError::Unavailable`], never not found, and nothing is recreated while running.
//! - Every call takes the key's own async mutex, then reads the state, then acts. The guard moves
//!   into the one blocking task that does the work, so a dropped caller cannot free the key while
//!   its write runs.
//! - Durable before answered: `begin` syncs the file and the staging directory, `append` the
//!   file, `finish` the rename's two directories, `delete` the directory it removed from, and
//!   `open` both areas.
//! - One process per directory: an exclusive lock on `.lock`, kept for the store's life and never
//!   deleted, as the agent's journal does (ADR-255).

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use sha2::{Digest, Sha256};

use super::{
    ObjectBody, ObjectKey, ObjectStatus, ObjectStore, StoreError, StoreFuture, UnfinishedObject,
};
use crate::config::StorageConfig;
use crate::fs_guard::{self, DirectoryFault, check_directory, sync_directory};

/// How many keys `begin` draws before it gives up on names that are taken.
const BEGIN_ATTEMPTS: usize = 8;

/// The local-file object store. Cheap to clone behind an [`Arc`]; it holds the directory's lock
/// until the last clone and the last running call are gone.
pub struct LocalStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LocalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalStore")
    }
}

struct Inner {
    staging: PathBuf,
    objects: PathBuf,
    max: u64,
    /// One mutex per key in use; an entry nobody holds is dropped at the next call.
    locks: Mutex<HashMap<ObjectKey, Arc<tokio::sync::Mutex<()>>>>,
    /// Keys whose `sync_all` failed (the second rule above).
    failed: Mutex<HashSet<ObjectKey>>,
    _process_lock: ProcessLock,
    #[cfg(test)]
    hooks: hooks::Hooks,
}

/// Where a key stands (the third rule above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    NotFound,
    Unfinished,
    Finished,
}

/// The exclusive lock on `.lock`. Unlocking before the close releases it for every handle to the
/// open file description, which a child process spawned meanwhile may share.
struct ProcessLock(File);

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn io_error(op: &'static str) -> impl FnOnce(io::Error) -> StoreError {
    move |error| StoreError::Io {
        op,
        kind: error.kind(),
    }
}

fn directory_error(fault: DirectoryFault) -> StoreError {
    match fault {
        DirectoryFault::Io(kind) => StoreError::Io {
            op: "inspect the directory",
            kind,
        },
        DirectoryFault::NotADirectory => StoreError::NotADirectory,
        DirectoryFault::NotOwned => StoreError::DirectoryNotOwned,
        DirectoryFault::Writable => StoreError::DirectoryWritable,
    }
}

/// Whether `error` says that a path or one of its parents is not there.
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Creates a directory, mode 0700 on Unix; its parent must exist.
fn create_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

/// Opens (creating) the file whose lock makes the store's process its directory's only user.
fn take_process_lock(path: &Path) -> Result<ProcessLock, StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = fs_guard::no_follow(&mut options)
        .open(path)
        .map_err(|error| {
            if fs_guard::is_link_refusal(&error) {
                StoreError::NotRegularFile
            } else {
                io_error("open the lock file")(error)
            }
        })?;
    let meta = file.metadata().map_err(io_error("inspect the lock file"))?;
    if !fs_guard::is_plain_file(&meta) {
        return Err(StoreError::NotRegularFile);
    }
    match file.try_lock() {
        Ok(()) => Ok(ProcessLock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::InUse),
        Err(std::fs::TryLockError::Error(error)) => Err(io_error("lock the directory")(error)),
    }
}

/// Makes sure the area at `path` exists as a directory the server may trust.
fn ensure_area(path: &Path) -> Result<(), StoreError> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_directory(path).map_err(io_error("create an area"))?;
        }
        Err(error) => return Err(io_error("inspect an area")(error)),
    }
    check_directory(path).map_err(directory_error)
}

impl LocalStore {
    /// Opens the store at `config.dir`, creating what is missing: the directory itself (its
    /// parent must exist), the two areas and the lock file. The directory and the areas must not
    /// be writable by group or others and must belong to the server's account or root (Unix). A
    /// second store on the directory, in this process or another, is [`StoreError::InUse`].
    pub fn open(config: &StorageConfig) -> Result<LocalStore, StoreError> {
        let dir = config.dir.as_path();
        if !dir.is_absolute() {
            return Err(StoreError::DirectoryNotAbsolute);
        }
        match std::fs::metadata(dir) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_directory(dir).map_err(io_error("create the directory"))?;
                if let Some(parent) = dir.parent() {
                    sync_directory(parent).map_err(io_error("sync the parent directory"))?;
                }
            }
            Err(error) => return Err(io_error("inspect the directory")(error)),
        }
        check_directory(dir).map_err(directory_error)?;
        let process_lock = take_process_lock(&dir.join(".lock"))?;
        let staging = dir.join("staging");
        let objects = dir.join("objects");
        ensure_area(&staging)?;
        ensure_area(&objects)?;
        for synced in [&staging, &objects, &dir.to_owned()] {
            sync_directory(synced).map_err(io_error("sync a directory"))?;
        }
        tracing::info!("object store ready");
        Ok(LocalStore {
            inner: Arc::new(Inner {
                staging,
                objects,
                max: config.max_object_bytes,
                locks: Mutex::new(HashMap::new()),
                failed: Mutex::new(HashSet::new()),
                _process_lock: process_lock,
                #[cfg(test)]
                hooks: hooks::Hooks::default(),
            }),
        })
    }

    /// Runs `work` on the blocking pool with `key`'s lock held. The guard moves into the task, so
    /// dropping the returned future does not release the key until `work` has returned.
    async fn locked<T, F>(&self, key: &ObjectKey, work: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Inner, &ObjectKey) -> Result<T, StoreError> + Send + 'static,
    {
        let guard = self.inner.lock_key(key).await;
        let inner = Arc::clone(&self.inner);
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work(&inner, &key)
        })
        .await
        .map_err(|_| StoreError::Io {
            op: "run a task",
            kind: io::ErrorKind::Other,
        })?
    }

    async fn list(&self) -> Result<Vec<UnfinishedObject>, StoreError> {
        let inner = Arc::clone(&self.inner);
        let keys = tokio::task::spawn_blocking(move || inner.staged_keys())
            .await
            .map_err(|_| StoreError::Io {
                op: "run a task",
                kind: io::ErrorKind::Other,
            })??;
        let mut listed = Vec::new();
        for key in keys {
            if let Some(object) = self.locked(&key, |inner, key| inner.list_one(key)).await? {
                listed.push(object);
            }
        }
        Ok(listed)
    }
}

impl Inner {
    async fn lock_key(&self, key: &ObjectKey) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
            Arc::clone(locks.entry(key.clone()).or_default())
        };
        mutex.lock_owned().await
    }

    fn staging_path(&self, key: &ObjectKey) -> PathBuf {
        self.staging.join(key.as_str())
    }

    fn object_path(&self, key: &ObjectKey) -> PathBuf {
        self.objects.join(key.as_str())
    }

    /// Both areas are directories, or [`StoreError::Unavailable`].
    fn areas(&self) -> Result<(), StoreError> {
        for area in [&self.staging, &self.objects] {
            match std::fs::metadata(area) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(StoreError::Unavailable),
                Err(error) if is_missing(&error) => return Err(StoreError::Unavailable),
                Err(error) => return Err(io_error("inspect an area")(error)),
            }
        }
        Ok(())
    }

    fn exists(path: &Path) -> Result<bool, StoreError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if is_missing(&error) => Ok(false),
            Err(error) => Err(io_error("inspect an object")(error)),
        }
    }

    /// The state of `key` (the third rule). The areas are checked first, so that a removed area
    /// is never read as an absent key.
    fn state(&self, key: &ObjectKey) -> Result<State, StoreError> {
        self.areas()?;
        if Inner::exists(&self.object_path(key))? {
            Ok(State::Finished)
        } else if Inner::exists(&self.staging_path(key))? {
            Ok(State::Unfinished)
        } else {
            Ok(State::NotFound)
        }
    }

    /// The error for a failed open: a vanished directory is unavailable, a vanished file is not
    /// found.
    fn open_error(&self, op: &'static str, error: io::Error) -> StoreError {
        if fs_guard::is_link_refusal(&error) {
            return StoreError::NotRegularFile;
        }
        if is_missing(&error) {
            return match self.areas() {
                Ok(()) => StoreError::NotFound,
                Err(unavailable) => unavailable,
            };
        }
        io_error(op)(error)
    }

    /// Opens the staging file for reading and writing (Windows may refuse a sync on a read-only
    /// handle), without following a link, and checks the handle.
    fn open_staging(&self, key: &ObjectKey) -> Result<File, StoreError> {
        let file = fs_guard::no_follow(OpenOptions::new().read(true).write(true))
            .open(self.staging_path(key))
            .map_err(|error| self.open_error("open an object", error))?;
        let meta = file.metadata().map_err(io_error("inspect an object"))?;
        if !fs_guard::is_plain_file(&meta) {
            return Err(StoreError::NotRegularFile);
        }
        Ok(file)
    }

    fn poison(&self, key: &ObjectKey) {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone());
    }

    fn check_not_poisoned(&self, key: &ObjectKey) -> Result<(), StoreError> {
        if self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(key)
        {
            return Err(StoreError::Io {
                op: "use an object after a failed sync",
                kind: io::ErrorKind::Other,
            });
        }
        Ok(())
    }

    /// The stored length (the first rule): one `sync_all`, then the file's length. A failed sync
    /// poisons the key.
    fn stored_length(&self, key: &ObjectKey, file: &File) -> Result<u64, StoreError> {
        self.sync_file(file).map_err(|error| {
            self.poison(key);
            io_error("sync an object")(error)
        })?;
        Ok(file
            .metadata()
            .map_err(io_error("inspect an object"))?
            .len())
    }

    fn begin(&self, key: &ObjectKey) -> Result<bool, StoreError> {
        self.areas()?;
        if Inner::exists(&self.object_path(key))? {
            return Ok(false);
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let path = self.staging_path(key);
        let file = match fs_guard::no_follow(&mut options).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(self.open_error("create an object", error)),
        };
        if let Err(error) = self.sync_file(&file) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(io_error("sync an object")(error));
        }
        drop(file);
        sync_directory(&self.staging).map_err(io_error("sync a directory"))?;
        Ok(true)
    }

    fn append(&self, key: &ObjectKey, offset: u64, bytes: &[u8]) -> Result<u64, StoreError> {
        self.check_not_poisoned(key)?;
        match self.state(key)? {
            State::Finished => return Err(StoreError::AlreadyFinished),
            State::NotFound => return Err(StoreError::NotFound),
            State::Unfinished => {}
        }
        let mut file = self.open_staging(key)?;
        let stored = self.stored_length(key, &file)?;
        if offset != stored {
            return Err(StoreError::OffsetMismatch { stored });
        }
        if bytes.is_empty() {
            return Ok(stored);
        }
        let end = u64::try_from(bytes.len())
            .ok()
            .and_then(|len| stored.checked_add(len))
            .filter(|end| *end <= self.max)
            .ok_or(StoreError::TooLarge {
                stored,
                max: self.max,
            })?;
        file.seek(SeekFrom::Start(stored))
            .map_err(io_error("write an object"))?;
        self.write_chunk(&mut file, bytes)
            .map_err(io_error("write an object"))?;
        self.sync_file(&file).map_err(|error| {
            self.poison(key);
            io_error("sync an object")(error)
        })?;
        Ok(end)
    }

    fn status(&self, key: &ObjectKey) -> Result<ObjectStatus, StoreError> {
        self.check_not_poisoned(key)?;
        match self.state(key)? {
            State::Finished => Ok(ObjectStatus::Finished),
            State::NotFound => Err(StoreError::NotFound),
            State::Unfinished => {
                let file = self.open_staging(key)?;
                Ok(ObjectStatus::Unfinished {
                    stored: self.stored_length(key, &file)?,
                })
            }
        }
    }

    fn finish(&self, key: &ObjectKey, sha256: [u8; 32], length: u64) -> Result<(), StoreError> {
        self.check_not_poisoned(key)?;
        match self.state(key)? {
            State::Finished => return Err(StoreError::AlreadyFinished),
            State::NotFound => return Err(StoreError::NotFound),
            State::Unfinished => {}
        }
        let mut file = self.open_staging(key)?;
        let stored = self.stored_length(key, &file)?;
        if length != stored {
            return Err(StoreError::LengthMismatch { stored });
        }
        file.seek(SeekFrom::Start(0))
            .map_err(io_error("read an object"))?;
        let mut hasher = Sha256::new();
        let mut reader = (&mut file).take(stored);
        let mut buffer = vec![0u8; 64 * 1024];
        let mut read = 0u64;
        loop {
            let n = reader
                .read(&mut buffer)
                .map_err(io_error("read an object"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            read += n as u64;
        }
        if read != stored {
            return Err(StoreError::Io {
                op: "read an object",
                kind: io::ErrorKind::UnexpectedEof,
            });
        }
        if hasher.finalize().as_slice() != sha256.as_slice() {
            return Err(StoreError::HashMismatch);
        }
        drop(file);
        // A finished object is never replaced; the state was read under this key's lock.
        let target = self.object_path(key);
        if Inner::exists(&target)? {
            return Err(StoreError::AlreadyFinished);
        }
        self.rename(&self.staging_path(key), &target)
            .map_err(|error| self.open_error("move an object", error))?;
        // The object is finished from here on, whatever the syncs say.
        self.sync_directory_after_rename(&self.objects)
            .map_err(io_error("sync a directory"))?;
        self.sync_directory_after_rename(&self.staging)
            .map_err(io_error("sync a directory"))?;
        Ok(())
    }

    fn get(&self, key: &ObjectKey) -> Result<(File, u64), StoreError> {
        self.check_not_poisoned(key)?;
        match self.state(key)? {
            State::NotFound => return Err(StoreError::NotFound),
            State::Unfinished => return Err(StoreError::NotFinished),
            State::Finished => {}
        }
        let file = fs_guard::no_follow(OpenOptions::new().read(true))
            .open(self.object_path(key))
            .map_err(|error| self.open_error("open an object", error))?;
        let meta = file.metadata().map_err(io_error("inspect an object"))?;
        if !fs_guard::is_plain_file(&meta) {
            return Err(StoreError::NotRegularFile);
        }
        Ok((file, meta.len()))
    }

    fn delete(&self, key: &ObjectKey) -> Result<(), StoreError> {
        self.areas()?;
        let mut removed = false;
        for area in [&self.objects, &self.staging] {
            match std::fs::remove_file(area.join(key.as_str())) {
                Ok(()) => {
                    removed = true;
                    sync_directory(area).map_err(io_error("sync a directory"))?;
                }
                Err(error) if is_missing(&error) => {}
                Err(error) => return Err(io_error("remove an object")(error)),
            }
        }
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        if removed {
            Ok(())
        } else {
            Err(StoreError::NotFound)
        }
    }

    /// The names in the staging area that are keys, in order. A name that is not a key is
    /// ignored; an entry that is not a regular file is skipped with a warning.
    fn staged_keys(&self) -> Result<Vec<ObjectKey>, StoreError> {
        self.areas()?;
        let mut keys = Vec::new();
        let entries = std::fs::read_dir(&self.staging).map_err(|error| {
            if is_missing(&error) {
                StoreError::Unavailable
            } else {
                io_error("list the objects")(error)
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(io_error("list the objects"))?;
            let Some(key) = entry
                .file_name()
                .to_str()
                .and_then(|name| ObjectKey::parse(name).ok())
            else {
                continue;
            };
            match entry.file_type() {
                Ok(kind) if kind.is_file() => keys.push(key),
                Ok(_) => tracing::warn!(%key, "the staging area holds an entry that is not a file"),
                Err(error) => return Err(io_error("list the objects")(error)),
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// One listing entry, under the key's lock: `None` for a key that is gone, finished (the
    /// finished copy wins) or unusable.
    fn list_one(&self, key: &ObjectKey) -> Result<Option<UnfinishedObject>, StoreError> {
        if self.check_not_poisoned(key).is_err() {
            tracing::warn!(%key, "an object whose sync failed is not listed");
            return Ok(None);
        }
        match self.state(key)? {
            State::NotFound | State::Finished => return Ok(None),
            State::Unfinished => {}
        }
        let file = match self.open_staging(key) {
            Ok(file) => file,
            Err(StoreError::NotRegularFile) => {
                tracing::warn!(%key, "the staging area holds an entry that is not a file");
                return Ok(None);
            }
            Err(StoreError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let stored = self.stored_length(key, &file)?;
        let modified = file
            .metadata()
            .and_then(|meta| meta.modified())
            .map_err(io_error("inspect an object"))?;
        Ok(Some(UnfinishedObject {
            key: key.clone(),
            stored,
            modified,
        }))
    }

    // The next four are the points a test can make fail or hold (see `hooks`).

    fn write_chunk(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        self.hooks.before_write(file, bytes)?;
        file.write_all(bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        #[cfg(test)]
        self.hooks.before_file_sync()?;
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        #[cfg(test)]
        self.hooks.before_rename()?;
        std::fs::rename(from, to)
    }

    fn sync_directory_after_rename(&self, dir: &Path) -> io::Result<()> {
        #[cfg(test)]
        self.hooks.before_directory_sync()?;
        sync_directory(dir)
    }
}

impl ObjectStore for LocalStore {
    fn begin(&self) -> StoreFuture<'_, ObjectKey> {
        Box::pin(async move {
            for _ in 0..BEGIN_ATTEMPTS {
                let key = ObjectKey::generate()?;
                if self.locked(&key, |inner, key| inner.begin(key)).await? {
                    return Ok(key);
                }
            }
            Err(StoreError::Io {
                op: "create an object",
                kind: io::ErrorKind::AlreadyExists,
            })
        })
    }

    fn append<'a>(
        &'a self,
        key: &'a ObjectKey,
        offset: u64,
        bytes: Vec<u8>,
    ) -> StoreFuture<'a, u64> {
        Box::pin(async move {
            self.locked(key, move |inner, key| inner.append(key, offset, &bytes))
                .await
        })
    }

    fn status<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectStatus> {
        Box::pin(async move { self.locked(key, |inner, key| inner.status(key)).await })
    }

    fn finish<'a>(
        &'a self,
        key: &'a ObjectKey,
        sha256: [u8; 32],
        length: u64,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.locked(key, move |inner, key| inner.finish(key, sha256, length))
                .await
        })
    }

    fn get<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ObjectBody> {
        Box::pin(async move {
            let (file, length) = self.locked(key, |inner, key| inner.get(key)).await?;
            Ok(ObjectBody {
                length,
                reader: Box::pin(tokio::fs::File::from_std(file)),
            })
        })
    }

    fn delete<'a>(&'a self, key: &'a ObjectKey) -> StoreFuture<'a, ()> {
        Box::pin(async move { self.locked(key, |inner, key| inner.delete(key)).await })
    }

    fn list_unfinished(&self) -> StoreFuture<'_, Vec<UnfinishedObject>> {
        Box::pin(self.list())
    }
}

/// What a test can make fail or hold. Compiled in tests only.
#[cfg(test)]
pub(super) mod hooks {
    use std::fs::File;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::time::Duration;

    #[derive(Default)]
    pub(in crate::storage) struct Hooks {
        /// The next writes put this many bytes and then fail.
        pub write_fails_after: Mutex<Option<usize>>,
        pub file_sync_fails: AtomicBool,
        /// How many file syncs pass before `file_sync_fails` takes effect.
        pub file_sync_skips: AtomicUsize,
        pub rename_fails: AtomicBool,
        pub directory_sync_after_rename_fails: AtomicBool,
        /// The next write waits here before it writes anything.
        pub park: Mutex<Option<Arc<Parker>>>,
    }

    /// A blocking task that stops at a point until the test lets it go.
    #[derive(Default)]
    pub(in crate::storage) struct Parker {
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    impl Parker {
        pub fn new() -> Arc<Parker> {
            Arc::new(Parker::default())
        }

        /// Called by the parked task: reports that it arrived, then waits to be released.
        fn enter(&self) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                let (next, timeout) = self
                    .changed
                    .wait_timeout(state, Duration::from_secs(30))
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
                assert!(!timeout.timed_out(), "the parked task was never released");
            }
        }

        /// Blocks until the task has arrived; call it from `spawn_blocking`.
        pub fn wait_entered(&self) {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            while !state.0 {
                let (next, timeout) = self
                    .changed
                    .wait_timeout(state, Duration::from_secs(30))
                    .unwrap_or_else(PoisonError::into_inner);
                state = next;
                assert!(!timeout.timed_out(), "the task never arrived");
            }
        }

        pub fn release(&self) {
            self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
            self.changed.notify_all();
        }
    }

    fn injected() -> io::Error {
        io::Error::other("injected failure")
    }

    impl Hooks {
        pub(in crate::storage) fn before_write(
            &self,
            file: &mut File,
            bytes: &[u8],
        ) -> io::Result<()> {
            let parked = self
                .park
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(parker) = parked {
                parker.enter();
            }
            let after = *self
                .write_fails_after
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(after) = after {
                file.write_all(&bytes[..after.min(bytes.len())])?;
                return Err(injected());
            }
            Ok(())
        }

        pub(in crate::storage) fn before_file_sync(&self) -> io::Result<()> {
            if self.file_sync_fails.load(Ordering::SeqCst) {
                // Take one skip if any is left (a compare-and-swap loop, which works on the
                // minimum supported Rust as on the current one).
                let mut left = self.file_sync_skips.load(Ordering::SeqCst);
                loop {
                    let Some(next) = left.checked_sub(1) else {
                        return Err(injected());
                    };
                    match self.file_sync_skips.compare_exchange(
                        left,
                        next,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(current) => left = current,
                    }
                }
            }
            Ok(())
        }

        pub(in crate::storage) fn before_rename(&self) -> io::Result<()> {
            if self.rename_fails.load(Ordering::SeqCst) {
                return Err(injected());
            }
            Ok(())
        }

        pub(in crate::storage) fn before_directory_sync(&self) -> io::Result<()> {
            if self
                .directory_sync_after_rename_fails
                .load(Ordering::SeqCst)
            {
                return Err(injected());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;

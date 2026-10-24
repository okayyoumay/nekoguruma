//! A log file of its own for each running process (ADR-348 item 5).

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::private;

/// The most slots tried; more processes than this sharing one log directory get none.
const MAX_SLOTS: u32 = 1000;

/// A slot claimed in a log directory: the smallest number whose lock file, `{stem}.{number}.lock`,
/// this process could lock exclusively. The OS releases the lock when the process ends, so no two
/// running processes share a number, and the numbers stay bounded by how many run at once.
#[derive(Debug)]
pub struct Slot {
    number: u32,
    path: PathBuf,
    /// Held for the slot's lifetime; dropping it releases the number.
    _lock: File,
}

impl Slot {
    /// Claims the smallest free slot for `stem` in `dir`, which is created private when missing.
    /// A lock file that cannot be used as one (it is not a regular file, or this account cannot
    /// open it, such as one another account created) is skipped like a held one.
    pub fn claim(dir: &Path, stem: &str) -> io::Result<Self> {
        private::create_dir(dir)?;
        let mut unusable = None;
        for number in 0..MAX_SLOTS {
            let lock_path = dir.join(format!("{stem}.{number}.lock"));
            let lock = match open_lock(&lock_path) {
                Ok(lock) => lock,
                Err(error) => {
                    unusable = Some(error);
                    continue;
                }
            };
            match lock.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        number,
                        path: dir.join(format!("{stem}.{number}.log")),
                        _lock: lock,
                    });
                }
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(error)) => unusable = Some(error),
            }
        }
        Err(unusable.unwrap_or_else(|| {
            io::Error::other(format!(
                "every one of the {MAX_SLOTS} log slots is held by another process"
            ))
        }))
    }

    /// The slot's number.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The log file the slot gives its process, `{stem}.{number}.log` in the directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Opens a lock file as the log file is opened: never through a symbolic link or reparse point,
/// never blocking on a FIFO, and only a regular file.
fn open_lock(path: &Path) -> io::Result<File> {
    let not_a_file = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    if std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file()) {
        return Err(not_a_file());
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let lock = options.open(path)?;
    let meta = lock.metadata()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(not_a_file());
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held slot is skipped; a released one is claimed again.
    #[test]
    fn each_holder_gets_the_smallest_free_slot() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("logs");
        let first = Slot::claim(&dir, "agent").unwrap();
        let second = Slot::claim(&dir, "agent").unwrap();
        assert_eq!((first.number(), second.number()), (0, 1));
        assert_eq!(first.path(), dir.join("agent.0.log"));
        assert_eq!(second.path(), dir.join("agent.1.log"));
        drop(first);
        let third = Slot::claim(&dir, "agent").unwrap();
        assert_eq!(third.number(), 0);
        // Another stem has slots of its own.
        assert_eq!(Slot::claim(&dir, "worker").unwrap().number(), 0);
    }

    /// A lock file that is a FIFO, a symbolic link or a directory is skipped, not opened.
    #[cfg(unix)]
    #[test]
    fn an_unusable_lock_file_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let fifo = std::ffi::CString::new(dir.join("agent.0.lock").to_str().unwrap()).unwrap();
        // SAFETY: `fifo` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("agent.1.lock")).unwrap();
        std::fs::create_dir(dir.join("agent.2.lock")).unwrap();
        let slot = Slot::claim(dir, "agent").unwrap();
        assert_eq!(slot.number(), 3);
        assert!(!dir.join("elsewhere").exists());
    }
}

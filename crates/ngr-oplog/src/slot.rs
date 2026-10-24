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
    pub fn claim(dir: &Path, stem: &str) -> io::Result<Self> {
        private::create_dir(dir)?;
        for number in 0..MAX_SLOTS {
            let lock_path = dir.join(format!("{stem}.{number}.lock"));
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?;
            match lock.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        number,
                        path: dir.join(format!("{stem}.{number}.log")),
                        _lock: lock,
                    });
                }
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        Err(io::Error::other(format!(
            "every one of the {MAX_SLOTS} log slots is held by another process"
        )))
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
}

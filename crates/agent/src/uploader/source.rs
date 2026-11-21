//! The source of an entry's bytes as the loop reads it (ADR-495 items 7 and 8): the check that
//! the bytes are what the entry recorded, and the reader a part's PUT streams from.
//!
//! The check hashes the first `length` bytes of the stored object or of the communication log's
//! file. Only what the agent can prove makes the source broken: the file itself is gone, it is
//! shorter than the entry's length, or its first `length` bytes hash to another digest. Any other
//! failure to open or read it (a directory that is not there, a permission, a read error) says
//! nothing about the bytes and is transient.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest as _, Sha256};

use super::Stop;

/// What the check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The first `length` bytes hash to the recorded digest.
    Verified,
    /// The agent proved the source broken; the word names how, for the warning.
    Broken(&'static str),
    /// The check could not be made: try again.
    Transient,
    /// A stop came before the check ended.
    Stopped,
}

/// Hashes the first `length` bytes of `path` on a blocking task, giving up at a stop.
pub(super) async fn verify(path: PathBuf, length: u64, sha256: [u8; 32], stop: &Stop) -> Verdict {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let task = tokio::task::spawn_blocking(move || check(&path, length, &sha256, &flag));
    tokio::select! {
        biased;
        () = stop.requested() => {
            // The hash stops at its next chunk.
            cancel.store(true, Ordering::Relaxed);
            Verdict::Stopped
        }
        verdict = task => verdict.unwrap_or(Verdict::Transient),
    }
}

fn check(path: &Path, length: u64, sha256: &[u8; 32], cancel: &AtomicBool) -> Verdict {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // A missing directory is not a missing file: the volume may be away.
            let directory_there = path
                .parent()
                .is_some_and(|parent| std::fs::metadata(parent).is_ok());
            return if directory_there {
                Verdict::Broken("source-missing")
            } else {
                Verdict::Transient
            };
        }
        Err(_) => return Verdict::Transient,
    };
    let mut hasher = Sha256::new();
    let mut left = length;
    let mut buffer = vec![0u8; 64 << 10];
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Verdict::Stopped;
        }
        let want = usize::try_from(left).map_or(buffer.len(), |l| l.min(buffer.len()));
        match file.read(&mut buffer[..want]) {
            Ok(0) => return Verdict::Broken("source-short"),
            Ok(read) => {
                hasher.update(&buffer[..read]);
                left -= read as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Verdict::Transient,
        }
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest == *sha256 {
        Verdict::Verified
    } else {
        Verdict::Broken("source-changed")
    }
}

/// The bytes of one part: `length` bytes of the source from `offset`, and no more however far
/// the file has grown. It opens the file on its first read (on the PUT's blocking thread, after
/// the connect), and any shortfall or error sets `unsound`: the loop then clears the entry's
/// verified flag, so the next attempt checks the source again.
pub(super) struct PartReader {
    path: PathBuf,
    offset: u64,
    left: u64,
    file: Option<File>,
    unsound: Arc<AtomicBool>,
}

impl PartReader {
    pub(super) fn new(path: PathBuf, offset: u64, length: u64, unsound: Arc<AtomicBool>) -> Self {
        Self {
            path,
            offset,
            left: length,
            file: None,
            unsound,
        }
    }

    fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        if self.file.is_none() {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(self.offset))?;
            self.file = Some(file);
        }
        let want = usize::try_from(self.left).map_or(buf.len(), |l| l.min(buf.len()));
        let read = self
            .file
            .as_mut()
            .expect("opened above")
            .read(&mut buf[..want])?;
        if read == 0 {
            // The file ended before the part did.
            return Ok(0);
        }
        self.left -= read as u64;
        Ok(read)
    }
}

impl Read for PartReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let before = self.left;
        let result = self.read_some(buf);
        let interrupted = result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted);
        if (result.is_err() && !interrupted) || (matches!(result, Ok(0)) && before > 0) {
            self.unsound.store(true, Ordering::Relaxed);
        }
        result
    }
}

#[cfg(test)]
mod tests;

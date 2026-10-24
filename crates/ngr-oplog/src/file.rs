//! A log file rotated by size (ADR-348 item 5).

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::private;

/// Where the file is and how far it grows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSettings {
    /// The file written to, for example `logs/agent.log`. Its directory is created private when
    /// missing.
    pub path: PathBuf,
    /// The size at which the file is rotated, in bytes. An entry is never split: the file is
    /// rotated before an entry that would take it past the limit, unless the file is empty.
    pub max_bytes: u64,
    /// How many rotated files are kept, `agent.1.log` the newest, at most
    /// [`MAX_FILES_LIMIT`](Self::MAX_FILES_LIMIT); the oldest beyond the count is deleted. With
    /// 0 the file is started again instead of rotated.
    pub max_files: u32,
}

impl FileSettings {
    /// The default size limit, 10 MiB.
    pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
    /// The default number of rotated files, 5: one process's log takes at most about 60 MiB.
    pub const DEFAULT_MAX_FILES: u32 = 5;
    /// The most rotated files kept, whatever `max_files` says: a rotation renames each of them
    /// while the log waits.
    pub const MAX_FILES_LIMIT: u32 = 100;

    /// `path` with the default limits.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_files: Self::DEFAULT_MAX_FILES,
        }
    }

    /// The path of rotated file `n` (1 is the newest): the number goes before the extension,
    /// `agent.log` giving `agent.1.log`, or is appended when there is none.
    pub fn rotated_path(&self, n: u32) -> PathBuf {
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        let name = match self.path.extension() {
            Some(extension) => format!("{stem}.{n}.{}", extension.to_string_lossy()),
            None => format!("{stem}.{n}"),
        };
        self.path.with_file_name(name)
    }
}

/// Why the file could not be opened. The caller falls back to another output (ADR-348 item 5
/// has it use stderr, with a warning).
#[derive(Debug)]
pub enum OpenError {
    /// The directory could not be created.
    Dir(io::Error),
    /// The file could not be opened, or not made private.
    File(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Dir(error) => write!(f, "the log directory could not be created: {error}"),
            OpenError::File(error) => {
                write!(f, "the log file could not be opened privately: {error}")
            }
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::Dir(error) | OpenError::File(error) => Some(error),
        }
    }
}

/// The open log file. [`write_entry`](Self::write_entry) writes one whole entry and rotates the
/// file first when the entry would take it past its limit.
#[derive(Debug)]
pub struct RotatingFile {
    settings: FileSettings,
    file: File,
    len: u64,
    /// The file was renamed to `rotated_path(1)` but no new file could be opened at the path,
    /// so `file` is that rotated file: the next rotation must neither move nor delete it.
    detached: bool,
    /// How many of the next reopens fail, so a test can make one fail.
    #[cfg(test)]
    fail_reopens: u32,
    /// A directory a test has the next rotation create at the path right after its rename, so
    /// the file cannot be moved back.
    #[cfg(test)]
    block_path_after_rename: bool,
}

impl RotatingFile {
    /// Opens the file for appending, creating it and its directory private and narrowing an
    /// existing file whose access is wider.
    pub fn open(settings: FileSettings) -> Result<Self, OpenError> {
        if let Some(dir) = settings
            .path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
        {
            private::create_dir(dir).map_err(OpenError::Dir)?;
        }
        let file = private::open_append(&settings.path).map_err(OpenError::File)?;
        let mut len = file.metadata().map_err(OpenError::File)?.len();
        // A run that ended in the middle of an entry (a crash, a power loss) left a line with
        // no end; it is ended, so the first new entry starts a line of its own.
        if len > 0 && private::byte_at(&file, len - 1).is_ok_and(|byte| byte != b'\n') {
            (&file).write_all(b"\n").map_err(OpenError::File)?;
            len += 1;
        }
        Ok(Self {
            settings,
            file,
            len,
            detached: false,
            #[cfg(test)]
            fail_reopens: 0,
            #[cfg(test)]
            block_path_after_rename: false,
        })
    }

    /// The settings the file was opened with.
    pub fn settings(&self) -> &FileSettings {
        &self.settings
    }

    /// Writes `entry`, which ends with its line break, in one write. When it would take a
    /// non-empty file past the limit, the file is rotated first; a rotation that fails does not
    /// keep the entry from being written.
    ///
    /// A write that fails part of the way is ended with a line break where possible, so the
    /// next entry still starts a line of its own.
    pub fn write_entry(&mut self, entry: &[u8]) -> io::Result<()> {
        if self.len > 0 && self.len.saturating_add(entry.len() as u64) > self.settings.max_bytes {
            self.rotate();
        }
        // The position is at the end already on Unix; on Windows it is moved there.
        let written = private::seek_end(&self.file).and_then(|()| self.file.write_all(entry));
        match written {
            Ok(()) => {
                self.len += entry.len() as u64;
                Ok(())
            }
            Err(error) => {
                // The length of what was written is unknown; counting the whole entry keeps
                // the rotation on the safe side.
                self.len += entry.len() as u64;
                if self.file.write_all(b"\n").is_ok() {
                    self.len += 1;
                }
                Err(error)
            }
        }
    }

    /// Moves each rotated file one number up, deleting the one beyond the count, and starts an
    /// empty file. Every step is tried even when an earlier one failed (a rotated file another
    /// process holds open, a file that cannot be deleted), and the current file is emptied in
    /// any case where the OS allows it, so the log keeps its size bound and keeps logging:
    /// - the current file cannot be renamed: it is truncated;
    /// - the new file cannot be opened: the renamed file is moved back and truncated, or, where
    ///   that fails too, entries go on to the renamed file, which no later rotation moves or
    ///   deletes; each rotation tries again to open a new file and otherwise truncates it.
    fn rotate(&mut self) {
        self.len = 0;
        if self.detached {
            match self.reopen() {
                Ok(file) => {
                    self.file = file;
                    self.detached = false;
                }
                Err(_) => {
                    let _ = self.empty();
                }
            }
            return;
        }
        let settings = &self.settings;
        let max_files = settings.max_files.min(FileSettings::MAX_FILES_LIMIT);
        if max_files == 0 && self.empty().is_ok() {
            return;
        }
        let renamed = if max_files > 0 {
            let _ = remove_if_present(&settings.rotated_path(max_files));
            for n in (1..max_files).rev() {
                let _ = rename_if_present(&settings.rotated_path(n), &settings.rotated_path(n + 1));
            }
            rename_if_present(&settings.path, &settings.rotated_path(1)).is_ok()
        } else {
            remove_if_present(&settings.path).is_ok()
        };
        #[cfg(test)]
        if renamed && std::mem::take(&mut self.block_path_after_rename) {
            std::fs::create_dir(&self.settings.path).unwrap();
        }
        if !renamed {
            // The file stays where it is; empty it instead. Where even that fails, it keeps
            // growing, by the limit each time it is reached.
            let _ = self.empty();
            return;
        }
        match self.reopen() {
            Ok(file) => self.file = file,
            Err(_) => {
                // `file` is now the rotated file. Put it back where the log is, emptied, so
                // the files stay as the next rotation expects them.
                let settings = &self.settings;
                if max_files > 0
                    && std::fs::rename(settings.rotated_path(1), &settings.path).is_ok()
                {
                    let _ = self.empty();
                } else {
                    self.detached = true;
                }
            }
        }
    }

    /// Empties the file entries go to, through the handle itself, so no path is opened again.
    fn empty(&self) -> io::Result<()> {
        self.file.set_len(0)?;
        private::seek_end(&self.file)
    }

    /// Opens a new file at the path.
    fn reopen(&mut self) -> io::Result<File> {
        #[cfg(test)]
        if self.fail_reopens > 0 {
            self.fail_reopens -= 1;
            return Err(io::Error::other("a reopen made to fail"));
        }
        private::open_append(&self.settings.path)
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(dir: &Path, max_bytes: u64, max_files: u32) -> FileSettings {
        FileSettings {
            path: dir.join("logs").join("agent.log"),
            max_bytes,
            max_files,
        }
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn rotated_files_are_numbered_before_the_extension() {
        let settings = FileSettings::new("logs/agent.log");
        assert_eq!(settings.rotated_path(1), Path::new("logs/agent.1.log"));
        let settings = FileSettings::new("logs/agent");
        assert_eq!(settings.rotated_path(3), Path::new("logs/agent.3"));
        assert_eq!(FileSettings::DEFAULT_MAX_BYTES, 10 * 1024 * 1024);
        assert_eq!(FileSettings::DEFAULT_MAX_FILES, 5);
    }

    /// The file is rotated before the entry that would take it past the limit, never in the
    /// middle of one; an entry larger than the limit still goes into an empty file whole.
    #[test]
    fn the_file_is_rotated_at_the_size_limit() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 10, 3);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"aaaa\n").unwrap();
        file.write_entry(b"bbbb\n").unwrap();
        assert_eq!(read(&settings.path), "aaaa\nbbbb\n");
        assert!(!settings.rotated_path(1).exists());
        file.write_entry(b"c\n").unwrap();
        assert_eq!(read(&settings.path), "c\n");
        assert_eq!(read(&settings.rotated_path(1)), "aaaa\nbbbb\n");
        file.write_entry(b"a long entry\n").unwrap();
        assert_eq!(read(&settings.path), "a long entry\n");
        assert_eq!(read(&settings.rotated_path(1)), "c\n");
        assert_eq!(read(&settings.rotated_path(2)), "aaaa\nbbbb\n");
    }

    /// Beyond the count the oldest rotated file is deleted, so at most `max_files` rotated
    /// files are left beside the current one.
    #[test]
    fn the_oldest_rotated_file_is_deleted_beyond_the_count() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 2);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        for entry in ["one\n", "two\n", "three\n", "four\n"] {
            file.write_entry(entry.as_bytes()).unwrap();
        }
        assert_eq!(read(&settings.path), "four\n");
        assert_eq!(read(&settings.rotated_path(1)), "three\n");
        assert_eq!(read(&settings.rotated_path(2)), "two\n");
        assert!(!settings.rotated_path(3).exists());
        let names = std::fs::read_dir(settings.path.parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(names, 3);
    }

    /// With no rotated files kept, the file starts over.
    #[test]
    fn with_no_rotated_files_the_file_starts_over() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 0);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"one\n").unwrap();
        file.write_entry(b"two\n").unwrap();
        assert_eq!(read(&settings.path), "two\n");
        assert!(!settings.rotated_path(1).exists());
    }

    /// A reopened file appends, and its existing length counts toward the limit.
    #[test]
    fn a_reopened_file_appends_and_counts_its_length() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 8, 1);
        RotatingFile::open(settings.clone())
            .unwrap()
            .write_entry(b"first\n")
            .unwrap();
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"second\n").unwrap();
        assert_eq!(read(&settings.path), "second\n");
        assert_eq!(read(&settings.rotated_path(1)), "first\n");
    }

    /// A rotated file that cannot be deleted or moved does not stop the log: the current file
    /// is still rotated and every entry written.
    #[test]
    fn a_stuck_rotated_file_does_not_stop_the_log() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 3);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        // The oldest and a middle rotated file are directories with something in them, which
        // neither a delete nor a rename onto them can replace.
        for n in [2, 3] {
            std::fs::create_dir(settings.rotated_path(n)).unwrap();
            std::fs::write(settings.rotated_path(n).join("x"), b"").unwrap();
        }
        for entry in ["one\n", "two\n", "three\n", "four\n"] {
            file.write_entry(entry.as_bytes()).unwrap();
        }
        assert_eq!(read(&settings.path), "four\n");
        assert_eq!(read(&settings.rotated_path(1)), "three\n");
        assert!(settings.rotated_path(2).is_dir() && settings.rotated_path(3).is_dir());
    }

    /// A new file that cannot be opened after the rename does not lose the log: the renamed
    /// file is moved back, and the next rotation works as usual.
    #[test]
    fn a_failed_reopen_moves_the_file_back() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 1);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"one\n").unwrap();
        file.fail_reopens = 1;
        file.write_entry(b"two\n").unwrap();
        assert_eq!(read(&settings.path), "two\n");
        assert!(!settings.rotated_path(1).exists());
        file.write_entry(b"three\n").unwrap();
        assert_eq!(read(&settings.path), "three\n");
        assert_eq!(read(&settings.rotated_path(1)), "two\n");
    }

    /// When the file cannot be moved back either, entries go on to the renamed file, which no
    /// later rotation deletes, and a new file is opened as soon as that works again.
    #[test]
    fn a_detached_file_is_never_deleted_and_is_left_once_a_reopen_works() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 1);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"one\n").unwrap();
        // The reopen fails, and something takes the path, so the file cannot be moved back.
        file.fail_reopens = 1;
        file.block_path_after_rename = true;
        file.write_entry(b"two\n").unwrap();
        assert!(file.detached);
        assert_eq!(read(&settings.rotated_path(1)), "one\ntwo\n");
        // Still unable to open a file at the path: the detached file is emptied, not deleted.
        file.fail_reopens = 1;
        file.write_entry(b"three\n").unwrap();
        assert_eq!(read(&settings.rotated_path(1)), "three\n");
        // The path is free again: the next rotation opens a new file there.
        std::fs::remove_dir(&settings.path).unwrap();
        file.write_entry(b"four\n").unwrap();
        assert!(!file.detached);
        assert_eq!(read(&settings.path), "four\n");
        assert_eq!(read(&settings.rotated_path(1)), "three\n");
    }

    /// A file that ends in the middle of a line, as a crash leaves it, is ended before the
    /// first new entry, so that entry starts a line of its own.
    #[test]
    fn a_torn_last_line_is_ended_on_open() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 1024, 1);
        std::fs::create_dir_all(settings.path.parent().unwrap()).unwrap();
        std::fs::write(&settings.path, b"{\"whole\":1}\n{\"torn").unwrap();
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"{\"next\":2}\n").unwrap();
        assert_eq!(
            read(&settings.path),
            "{\"whole\":1}\n{\"torn\n{\"next\":2}\n"
        );
        // A file that ends with its line break gets nothing added.
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"{\"last\":3}\n").unwrap();
        assert!(read(&settings.path).ends_with("{\"next\":2}\n{\"last\":3}\n"));
    }

    /// The number of rotated files is capped.
    #[test]
    fn the_number_of_rotated_files_is_capped() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, u32::MAX);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"one\n").unwrap();
        file.write_entry(b"two\n").unwrap();
        assert_eq!(read(&settings.rotated_path(1)), "one\n");
        const { assert!(FileSettings::MAX_FILES_LIMIT >= FileSettings::DEFAULT_MAX_FILES) };
    }

    /// A rotated file is private like the one it was.
    #[cfg(unix)]
    #[test]
    fn rotated_files_stay_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let settings = settings(temp.path(), 4, 1);
        let mut file = RotatingFile::open(settings.clone()).unwrap();
        file.write_entry(b"one\n").unwrap();
        file.write_entry(b"two\n").unwrap();
        for path in [settings.path.clone(), settings.rotated_path(1)] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        let dir = settings.path.parent().unwrap();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}

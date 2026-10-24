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
    /// How many rotated files are kept, `agent.1.log` the newest; the oldest beyond the count is
    /// deleted. With 0 the file is emptied instead of rotated.
    pub max_files: u32,
}

impl FileSettings {
    /// The default size limit, 10 MiB.
    pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
    /// The default number of rotated files, 5: one process's log takes at most about 60 MiB.
    pub const DEFAULT_MAX_FILES: u32 = 5;

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
        let len = file.metadata().map_err(OpenError::File)?.len();
        Ok(Self {
            settings,
            file,
            len,
        })
    }

    /// The settings the file was opened with.
    pub fn settings(&self) -> &FileSettings {
        &self.settings
    }

    /// Writes `entry`, which ends with its line break, in one write. When it would take a
    /// non-empty file past the limit, the file is rotated first.
    pub fn write_entry(&mut self, entry: &[u8]) -> io::Result<()> {
        if self.len > 0 && self.len.saturating_add(entry.len() as u64) > self.settings.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(entry)?;
        self.len += entry.len() as u64;
        Ok(())
    }

    /// Moves each rotated file one number up, deletes the one beyond the count, and starts an
    /// empty file. A failed rename or delete leaves the older files as they are; the current
    /// file is replaced in any case, so the log keeps its size bound.
    fn rotate(&mut self) -> io::Result<()> {
        let settings = &self.settings;
        if settings.max_files > 0 {
            let oldest = settings.rotated_path(settings.max_files);
            match std::fs::remove_file(&oldest) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            for n in (1..settings.max_files).rev() {
                rename_if_present(&settings.rotated_path(n), &settings.rotated_path(n + 1))?;
            }
            std::fs::rename(&settings.path, settings.rotated_path(1))?;
        } else {
            std::fs::remove_file(&settings.path)?;
        }
        self.file = private::open_append(&settings.path)?;
        self.len = 0;
        Ok(())
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

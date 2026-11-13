//! The file-system checks the server's own files share: the instruction key's file
//! (`instruction_key.rs`, ADR-462) and the object store's directory (`storage`).
//!
//! A directory is judged by path (it follows a symbolic link to a directory), the files inside it
//! on their own open handles. On Windows the directory's ACL, set when the service is installed,
//! is the protection and the code checks no ACL.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Why a directory failed [`check_directory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryFault {
    /// The metadata could not be read.
    Io(io::ErrorKind),
    NotADirectory,
    /// Owned by neither the server's account nor root (Unix).
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the Unix check reads the owner")
    )]
    NotOwned,
    /// Writable by group or others (Unix).
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the Unix check reads the mode bits")
    )]
    Writable,
}

/// The effective user ID of the process.
#[cfg(unix)]
pub(crate) fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Refuses a directory that group or others can write to, or that belongs to neither the
/// server's account nor root (Unix).
#[cfg(unix)]
pub(crate) fn check_directory(dir: &Path) -> Result<(), DirectoryFault> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::metadata(dir).map_err(|error| DirectoryFault::Io(error.kind()))?;
    check_directory_meta(meta.is_dir(), meta.uid(), meta.permissions().mode())
}

/// The decision of [`check_directory`] on what the metadata says, so that a test can judge a
/// owner it cannot create.
#[cfg(unix)]
pub(crate) fn check_directory_meta(
    is_dir: bool,
    uid: u32,
    mode: u32,
) -> Result<(), DirectoryFault> {
    if !is_dir {
        return Err(DirectoryFault::NotADirectory);
    }
    if uid != euid() && uid != 0 {
        return Err(DirectoryFault::NotOwned);
    }
    if mode & 0o022 != 0 {
        return Err(DirectoryFault::Writable);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn check_directory(dir: &Path) -> Result<(), DirectoryFault> {
    let meta = std::fs::metadata(dir).map_err(|error| DirectoryFault::Io(error.kind()))?;
    if !meta.is_dir() {
        return Err(DirectoryFault::NotADirectory);
    }
    Ok(())
}

/// Makes a directory's entries durable. Windows cannot sync a directory; a file's own sync covers
/// its entry there.
#[cfg(unix)]
pub(crate) fn sync_directory(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Makes `options` open a file without following a symbolic link and without blocking on a FIFO
/// (the same open as the agent's lock files, ADR-262 item 6). On Windows a reparse point is
/// opened as itself. The caller then checks the handle.
pub(crate) fn no_follow(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a reparse point itself, not its target.
        options.custom_flags(0x0020_0000);
    }
    options
}

/// Whether `error` is what [`no_follow`] gives for a path that is a symbolic link (Unix).
pub(crate) fn is_link_refusal(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// Whether an opened file is a regular file and, on Windows, not a reparse point.
pub(crate) fn is_plain_file(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        meta.is_file() && meta.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        meta.is_file()
    }
}

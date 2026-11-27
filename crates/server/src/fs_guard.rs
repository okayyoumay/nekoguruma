//! The file-system checks and writes the server's own files share: the instruction key's file
//! (`instruction_key.rs`, ADR-462), the object store's directory (`storage`) and the bundled
//! object server's credential files (`object_server/credentials.rs`, ADR-491).
//!
//! A directory is judged by path (it follows a symbolic link to a directory), the files inside it
//! on their own open handles. On Windows the directory's ACL, set when the service is installed,
//! is the protection and the code checks no ACL.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
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
#[expect(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent: `geteuid` reads the effective user id (ADR-502)"
)]
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

/// Whether an opened file is one the server may not trust: on Unix, owned by another account
/// or open to group or others; never on Windows, where the directory's ACL protects the files.
/// The same rule as the instruction key's file, applied to the object store's files.
pub(crate) fn is_foreign_file(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        is_foreign_file_meta(meta.uid(), meta.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// The decision of [`is_foreign_file`] on what the metadata says, so that a test can judge an
/// owner it cannot create.
#[cfg(unix)]
pub(crate) fn is_foreign_file_meta(uid: u32, mode: u32) -> bool {
    uid != euid() || mode & 0o077 != 0
}

/// Why [`open_private`] refused a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileFault {
    /// An operation failed; `op` names it ("open", "inspect").
    Io {
        op: &'static str,
        kind: io::ErrorKind,
    },
    /// Not a regular file: a directory, a FIFO or a symbolic link (a reparse point on Windows).
    NotRegular,
    /// Owned by another account (Unix).
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the Unix check reads the owner")
    )]
    NotOwned,
    /// Any group or other permission bit is set (Unix).
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only the Unix check reads the mode bits")
    )]
    TooOpen,
}

/// Opens a file the server keeps a secret in, for reading: `None` when it is absent. The open
/// does not follow a symbolic link and does not block on a FIFO ([`no_follow`]); the open handle
/// is then judged: a regular file, on Unix the server account's own and with no group or other
/// bit, on Windows not a reparse point. A link, dangling or not, is refused, never read as absent.
pub(crate) fn open_private(path: &Path) -> Result<Option<File>, FileFault> {
    let file = match no_follow(OpenOptions::new().read(true)).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if is_link_refusal(&error) => return Err(FileFault::NotRegular),
        Err(error) => {
            return Err(FileFault::Io {
                op: "open",
                kind: error.kind(),
            });
        }
    };
    let meta = file.metadata().map_err(|error| FileFault::Io {
        op: "inspect",
        kind: error.kind(),
    })?;
    check_private_meta(&meta)?;
    Ok(Some(file))
}

#[cfg(unix)]
fn check_private_meta(meta: &std::fs::Metadata) -> Result<(), FileFault> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !meta.is_file() {
        return Err(FileFault::NotRegular);
    }
    if meta.uid() != euid() {
        return Err(FileFault::NotOwned);
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(FileFault::TooOpen);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_meta(meta: &std::fs::Metadata) -> Result<(), FileFault> {
    if !is_plain_file(meta) {
        return Err(FileFault::NotRegular);
    }
    Ok(())
}

/// Why [`create_by_link`] failed.
#[derive(Debug)]
pub(crate) enum LinkFault {
    /// The operating system has no randomness for the temporary name; the source is the
    /// generator's error.
    Random(shared_crypto::RandomError),
    Io {
        op: &'static str,
        error: io::Error,
    },
}

/// Creates `path` (in `dir`) holding `bytes` if it is still absent, and says whether this call
/// did. The bytes go to a temporary file of a name no other attempt has (mode 0600 on Unix), are
/// synced, and are then hard-linked to the final name, which fails when the name exists: so the
/// file appears complete or not at all, and of any number of creators one wins (`Ok(true)`) and
/// the others get `Ok(false)` and read what the winner wrote. The directory is synced after a
/// link. A crash between the link and the removal of the temporary file leaves that file behind;
/// it is never read.
pub(crate) fn create_by_link(
    dir: &Path,
    path: &Path,
    tmp_prefix: &str,
    bytes: &[u8],
) -> Result<bool, LinkFault> {
    let suffix: String = shared_crypto::random_32()
        .map_err(LinkFault::Random)?
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    let tmp = dir.join(format!("{tmp_prefix}{suffix}.tmp"));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(LinkFault::Io { op: "write", error });
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            sync_directory(dir).map_err(|error| LinkFault::Io {
                op: "sync the directory of",
                error,
            })?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(LinkFault::Io { op: "link", error }),
    }
}

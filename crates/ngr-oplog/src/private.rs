//! Files and directories readable and writable only by the process's account and administrators
//! (ADR-348 item 5): mode 0700 and 0600 on Unix (root needs no grant), and on Windows an access
//! list, protected from inheritance, that allows only the object's owner, SYSTEM and the
//! Administrators group. Ownership and the access list are checked and set through the handle
//! to the object opened, never by its path.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Creates `dir` and its missing parents, each private. An existing directory is left as it is:
/// it may be one the installation prepared, or one the caller shares (a configured path).
pub(crate) fn create_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(windows)]
    {
        // Each directory this creates is made private; the ones that existed are not touched.
        let mut missing = Vec::new();
        let mut at = Some(dir);
        while let Some(path) = at.filter(|path| !path.as_os_str().is_empty() && !path.exists()) {
            missing.push(path);
            at = path.parent();
        }
        for path in missing.into_iter().rev() {
            match std::fs::create_dir(path) {
                Ok(()) => {
                    // A directory that could not be made private is not left behind, or the
                    // next start would take it as one prepared for the log.
                    if let Err(error) = windows::restrict_dir(path) {
                        let _ = std::fs::remove_dir(path);
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Opens `path` for writing at its end, creating it private, and narrows an existing file whose access is
/// wider. A path that is not a regular file is refused: a symbolic link or reparse point is not
/// followed (the open itself refuses it, so one put in place after the check is refused too), and
/// a FIFO does not block the open.
pub(crate) fn open_append(path: &Path) -> io::Result<File> {
    let not_a_file = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the log path is not a regular file",
        )
    };
    if std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file()) {
        return Err(not_a_file());
    }
    let mut options = OpenOptions::new();
    // On Unix the file is opened to append. On Windows a handle opened to append may not
    // truncate, which a rotation needs when the file cannot be renamed, so it is opened to write
    // and its one writer moves to the end before each write ([`seek_end`]).
    options.create(true);
    #[cfg(unix)]
    options.append(true);
    #[cfg(windows)]
    options.write(true);
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
        // Writing, and reading and changing the file's own security, so its owner is checked and
        // its access list set through this handle rather than by its path, which another
        // account could have changed by then.
        options
            .access_mode(windows::FILE_ACCESS)
            .custom_flags(windows::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(not_a_file());
    }
    restrict_file(&file)?;
    seek_end(&file)?;
    Ok(file)
}

/// Moves `file`'s position to its end, where the next write goes: a no-op on Unix, where the file
/// is opened to append.
pub(crate) fn seek_end(file: &File) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::io::Seek;
        (&*file).seek(io::SeekFrom::End(0))?;
    }
    #[cfg(unix)]
    let _ = file;
    Ok(())
}

/// The error for an existing file that another account owns: narrowing its mode or its access
/// list would still leave that account access, so the file is refused and the caller falls back.
fn foreign_owner() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the log file belongs to another account",
    )
}

#[cfg(unix)]
fn restrict_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = file.metadata()?;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    // Root owns what an administrator prepared; any other owner keeps access to the file.
    if meta.uid() != me && meta.uid() != 0 {
        return Err(foreign_owner());
    }
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_file(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    if !windows::owned_by_us_or_administrators(handle)? {
        return Err(foreign_owner());
    }
    windows::restrict(handle, false)
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_FILE_OBJECT,
        SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
        IsWellKnownSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Full access for the owner (`OW`), SYSTEM (`SY`) and the Administrators group (`BA`),
    /// nothing inherited (`P`); a directory's entries are inherited by what is created in it
    /// (`OICI`).
    pub(super) const FILE_SDDL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)";
    pub(super) const DIR_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    /// Opens the reparse point itself rather than what it points to.
    pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    /// Opens a directory.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    /// A log file's handle: writing, its attributes (for its metadata), and reading and
    /// changing its security.
    pub(super) const FILE_ACCESS: u32 =
        GENERIC_WRITE | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC;

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        text.encode_wide().chain(Some(0)).collect()
    }

    /// Whether the owner of the object `handle` is open on is the process's account, SYSTEM or
    /// the Administrators group (an elevated process's files are owned by the group), the
    /// accounts the access list grants.
    pub(super) fn owned_by_us_or_administrators(handle: RawHandle) -> io::Result<bool> {
        let mut owner = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `handle` is open with READ_CONTROL; `owner` points into `descriptor`, an
        // allocation the call makes and which is freed below.
        let status = unsafe {
            GetSecurityInfo(
                handle as HANDLE,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let result = (|| {
            // SAFETY: `owner` is a valid SID inside `descriptor`.
            if unsafe {
                IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
                    || IsWellKnownSid(owner, WinLocalSystemSid) != 0
            } {
                return Ok(true);
            }
            let mut token: HANDLE = std::ptr::null_mut();
            // SAFETY: the current process's pseudo handle needs no closing; `token` receives a
            // handle closed below.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // A buffer of `u64`s keeps the `TOKEN_USER` it receives aligned.
            let mut buffer = vec![0u64; 16];
            let mut len = 0u32;
            // SAFETY: `token` is open with TOKEN_QUERY, and `buffer` holds `len` bytes.
            let ok = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                    &mut len,
                )
            };
            let error = io::Error::last_os_error();
            // SAFETY: `token` was opened above and is closed once.
            unsafe { CloseHandle(token) };
            if ok == 0 {
                return Err(error);
            }
            // SAFETY: the call filled `buffer` with a `TOKEN_USER` whose SID lies inside it;
            // `owner` is valid as above.
            let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
            Ok(unsafe { EqualSid(owner, user.User.Sid) } != 0)
        })();
        // SAFETY: `descriptor` was allocated by the call above and is freed once.
        unsafe { LocalFree(descriptor as _) };
        result
    }

    /// Gives the directory at `path`, which this process has just created, the private access
    /// list, through a handle to that directory itself (a reparse point put there instead is
    /// refused).
    pub(super) fn restrict_dir(path: &Path) -> io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let meta = dir.metadata()?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the log directory is not a directory",
            ));
        }
        restrict(dir.as_raw_handle(), true)
    }

    /// Replaces the access list of the object `handle` is open on (with WRITE_DAC) with the
    /// private one.
    pub(super) fn restrict(handle: RawHandle, dir: bool) -> io::Result<()> {
        let sddl = wide(if dir { DIR_SDDL } else { FILE_SDDL }.as_ref());
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 string, and `descriptor` receives a
        // descriptor the call allocates, freed below with `LocalFree`.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1, // SDDL_REVISION_1
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut present = 0;
            let mut defaulted = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            // SAFETY: `descriptor` is the valid descriptor converted above; `dacl` points into
            // it and stays valid until it is freed.
            if unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `handle` is open with WRITE_DAC and `dacl` is a valid access list; no
            // owner, group or system list is changed.
            let status = unsafe {
                SetSecurityInfo(
                    handle as HANDLE,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            Ok(())
        })();
        // SAFETY: `descriptor` was allocated by the conversion above and is freed once.
        unsafe { LocalFree(descriptor as _) };
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_created_directory_and_file_are_private() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("a/logs");
        create_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&temp.path().join("a")), 0o700);
        let path = dir.join("agent.log");
        drop(open_append(&path).unwrap());
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_wider_existing_file_is_narrowed_and_an_existing_directory_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        create_dir(temp.path()).unwrap();
        assert_eq!(mode(temp.path()), 0o755);
        let path = temp.path().join("agent.log");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_append(&path).unwrap());
        assert_eq!(mode(&path), 0o600);
    }

    /// An existing file another account owns is refused: narrowing its mode would still leave
    /// that account access. Only root can give a file away, so the test runs only as root.
    #[test]
    fn a_file_another_account_owns_is_refused() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agent.log");
        std::fs::write(&path, b"").unwrap();
        std::os::unix::fs::chown(&path, Some(65534), None).unwrap();
        assert_eq!(
            open_append(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        std::os::unix::fs::chown(&path, Some(0), None).unwrap();
        drop(open_append(&path).unwrap());
    }

    #[test]
    fn a_path_that_is_not_a_regular_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, b"").unwrap();
        let link = temp.path().join("agent.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            open_append(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            open_append(temp.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    use super::*;

    /// `path`'s access list in SDDL.
    fn dacl(path: &Path) -> String {
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `name` is a NUL-terminated path; `descriptor` receives an allocation freed
        // below.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        let mut text = std::ptr::null_mut();
        let mut len = 0;
        // SAFETY: `descriptor` is valid; `text` receives an allocation freed below.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut len,
            )
        };
        assert_ne!(converted, 0);
        // SAFETY: `text` holds `len` UTF-16 units, its NUL included.
        let units = unsafe { std::slice::from_raw_parts(text, len as usize) };
        let sddl = String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_owned();
        // SAFETY: both were allocated by the calls above and are freed once.
        unsafe {
            LocalFree(text as _);
            LocalFree(descriptor as _);
        }
        sddl
    }

    /// Whether `sddl`, as read back, is the private list `expected`: protected (`P`) with the
    /// same entries. Windows adds its own control flags on the way back, such as `AI` (the list
    /// was set through the inheritance-aware API), which say nothing about who has access.
    fn is_private(sddl: &str, expected: &str) -> bool {
        let split = |text: &str| -> (String, String) {
            let text = text.strip_prefix("D:").unwrap_or(text);
            let at = text.find('(').unwrap_or(text.len());
            (text[..at].to_owned(), text[at..].to_owned())
        };
        let (flags, entries) = split(sddl);
        let (_, expected_entries) = split(expected);
        flags.contains('P') && entries == expected_entries
    }

    #[test]
    fn a_created_directory_and_file_get_the_private_access_list() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("a").join("logs");
        create_dir(&dir).unwrap();
        assert!(is_private(&dacl(&dir), windows::DIR_SDDL), "{}", dacl(&dir));
        let parent = dacl(&temp.path().join("a"));
        assert!(is_private(&parent, windows::DIR_SDDL), "{parent}");
        let path = dir.join("agent.log");
        drop(open_append(&path).unwrap());
        assert!(
            is_private(&dacl(&path), windows::FILE_SDDL),
            "{}",
            dacl(&path)
        );
    }

    #[test]
    fn an_existing_file_is_narrowed_and_an_existing_directory_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let before = dacl(temp.path());
        create_dir(temp.path()).unwrap();
        assert_eq!(dacl(temp.path()), before);
        let path = temp.path().join("agent.log");
        std::fs::write(&path, b"").unwrap();
        assert!(
            !is_private(&dacl(&path), windows::FILE_SDDL),
            "{}",
            dacl(&path)
        );
        drop(open_append(&path).unwrap());
        assert!(
            is_private(&dacl(&path), windows::FILE_SDDL),
            "{}",
            dacl(&path)
        );
    }
}

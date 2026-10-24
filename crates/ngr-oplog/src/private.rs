//! Files and directories readable and writable only by the process's account and administrators
//! (ADR-348 item 5): mode 0700 and 0600 on Unix (root needs no grant), and on Windows an access
//! list, protected from inheritance, that allows only the object's owner, SYSTEM and the
//! Administrators group.

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
                    if let Err(error) = windows::restrict(path, true) {
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

/// Opens `path` for appending, creating it private, and narrows an existing file whose access is
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
    options.create(true).append(true);
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
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(not_a_file());
    }
    restrict_file(path, &file)?;
    Ok(file)
}

#[cfg(unix)]
fn restrict_file(_path: &Path, file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode();
    if mode & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_file(path: &Path, _file: &File) -> io::Result<()> {
    windows::restrict(path, false)
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    /// Full access for the owner (`OW`), SYSTEM (`SY`) and the Administrators group (`BA`),
    /// nothing inherited (`P`); a directory's entries are inherited by what is created in it
    /// (`OICI`).
    pub(super) const FILE_SDDL: &str = "D:P(A;;FA;;;OW)(A;;FA;;;SY)(A;;FA;;;BA)";
    pub(super) const DIR_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    /// Replaces `path`'s access list with the private one. The file was opened without following
    /// a reparse point and checked to be a regular file, in a directory the log owns, so the path
    /// names the object opened.
    pub(super) fn restrict(path: &Path, dir: bool) -> io::Result<()> {
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
            let name = wide(path.as_os_str());
            // SAFETY: `name` is a NUL-terminated UTF-16 path and `dacl` a valid access list;
            // no owner, group or system list is changed.
            let status = unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
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

    #[test]
    fn a_created_directory_and_file_get_the_private_access_list() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("a").join("logs");
        create_dir(&dir).unwrap();
        assert_eq!(dacl(&dir), windows::DIR_SDDL);
        assert_eq!(dacl(&temp.path().join("a")), windows::DIR_SDDL);
        let path = dir.join("agent.log");
        drop(open_append(&path).unwrap());
        assert_eq!(dacl(&path), windows::FILE_SDDL);
    }

    #[test]
    fn an_existing_file_is_narrowed_and_an_existing_directory_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let before = dacl(temp.path());
        create_dir(temp.path()).unwrap();
        assert_eq!(dacl(temp.path()), before);
        let path = temp.path().join("agent.log");
        std::fs::write(&path, b"").unwrap();
        assert_ne!(dacl(&path), windows::FILE_SDDL);
        drop(open_append(&path).unwrap());
        assert_eq!(dacl(&path), windows::FILE_SDDL);
    }
}

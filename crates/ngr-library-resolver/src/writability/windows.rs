//! The Windows rule (7.2, ADR-270) and the probe that feeds it from the file's security
//! descriptor.
//!
//! The rule works on plain data (owner SID text and a list of ACEs) so it is compiled and tested
//! on every platform; only the probe, which calls the operating system, is Windows-only.
#![cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only the Windows probe uses the Windows rule")
)]

use super::{Reason, Role};

/// SYSTEM, BUILTIN\Administrators and TrustedInstaller.
pub(super) const SYSTEM_SIDS: [&str; 3] = [
    "S-1-5-18",
    "S-1-5-32-544",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
];

/// OWNER RIGHTS: an ACE for it only limits what the owner gets, and the owner is checked itself.
const OWNER_RIGHTS_SID: &str = "S-1-3-4";

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 9;
const ACCESS_DENIED_CALLBACK_ACE_TYPE: u8 = 10;
/// ACE flag: the ACE only passes to children and does not apply to this object.
const INHERIT_ONLY_ACE: u8 = 0x08;

// Access mask bits (winnt.h names; the file and directory meanings share bit values).
/// File: write data. Directory: add a file.
const FILE_WRITE_DATA: u32 = 0x2;
/// File: append data. Directory: add a subdirectory.
const FILE_APPEND_DATA: u32 = 0x4;
const FILE_DELETE_CHILD: u32 = 0x40;
/// Write attributes; on a link entry, counted because the link's target is part of its data.
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
const WRITE_DAC: u32 = 0x4_0000;
const WRITE_OWNER: u32 = 0x8_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

/// Rights that let the holder change or replace an object, whatever it is.
const COMMON_WRITE: u32 =
    DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED;

/// One access-control entry of a DACL, reduced to what the rule needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ace {
    pub(super) ace_type: u8,
    pub(super) flags: u8,
    pub(super) mask: u32,
    /// Text form of the trustee SID; empty for ACE types the rule does not decode.
    pub(super) sid: String,
}

/// The access-mask bits that count as write access for a role.
fn write_mask(role: Role) -> u32 {
    match role {
        // Adding files or subdirectories to a container is accepted; removing from it is not.
        Role::Directory => COMMON_WRITE | FILE_DELETE_CHILD,
        Role::LibraryDirectory => {
            COMMON_WRITE | FILE_DELETE_CHILD | FILE_WRITE_DATA | FILE_APPEND_DATA
        }
        Role::Library | Role::NamingFile => COMMON_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA,
        Role::Link => COMMON_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_ATTRIBUTES,
    }
}

/// The reasons an entry fails, from its owner SID, DACL (`None` for a null DACL) and role.
pub(super) fn reasons(
    owner: &str,
    dacl: Option<&[Ace]>,
    role: Role,
    trusted: &[String],
) -> Vec<Reason> {
    let is_trusted = |sid: &str| trusted.iter().any(|t| t == sid);
    let mut out = Vec::new();
    if !is_trusted(owner) {
        out.push(Reason::UnprivilegedOwner(owner.to_owned()));
    }
    let Some(dacl) = dacl else {
        out.push(Reason::NullDacl);
        return out;
    };
    let mask = write_mask(role);
    for ace in dacl {
        if ace.flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match ace.ace_type {
            ACCESS_DENIED_ACE_TYPE | ACCESS_DENIED_CALLBACK_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {
                if is_trusted(&ace.sid) || ace.sid == OWNER_RIGHTS_SID {
                    continue;
                }
                if ace.mask & mask != 0 {
                    out.push(Reason::WritableByRegularUsers(format!(
                        "{} mask {:#x}",
                        ace.sid, ace.mask
                    )));
                }
            }
            other => out.push(Reason::WritableByRegularUsers(format!(
                "unknown ACE type {other}"
            ))),
        }
    }
    out
}

#[cfg(windows)]
pub(super) use probe::WindowsProbe;
#[cfg(all(windows, any(test, debug_assertions)))]
pub(super) use probe::current_user_sid;

#[cfg(windows)]
mod probe {
    use std::{
        io,
        os::windows::ffi::OsStrExt,
        path::Path,
        ptr::{addr_of, null, null_mut},
        slice,
    };

    use windows_sys::{
        Win32::{
            Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree},
            Security::{
                ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
                Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT},
                DACL_SECURITY_INFORMATION, GetAce, OWNER_SECURITY_INFORMATION,
                PSECURITY_DESCRIPTOR, PSID,
            },
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
            },
        },
        core::PWSTR,
    };

    use super::{Ace, reasons};
    use crate::writability::{Inspected, Kind, Probe, Role};

    pub(in crate::writability) struct WindowsProbe {
        pub(in crate::writability) sids: Vec<String>,
    }

    impl Probe for WindowsProbe {
        fn inspect(&self, path: &Path, role: Role) -> io::Result<Inspected> {
            let meta = std::fs::symlink_metadata(path)?;
            // Symbolic links and junctions (name-surrogate reparse points). Other reparse points,
            // such as deduplicated or cloud placeholder files, are the files they stand for.
            let kind = if meta.file_type().is_symlink() {
                Kind::Link
            } else if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            let role = if kind == Kind::Link { Role::Link } else { role };
            let (owner, dacl) = read_security(path)?;
            Ok(Inspected {
                kind,
                reasons: reasons(&owner, dacl.as_deref(), role, &self.sids),
            })
        }
    }

    /// Closes a handle on drop.
    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by a successful call and is closed only here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Frees a security descriptor allocated by `GetSecurityInfo` on drop.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the descriptor was allocated by GetSecurityInfo, which documents LocalFree
            // as the way to release it; it is freed only here.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    /// Text form of a SID.
    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut text: PWSTR = null_mut();
        // SAFETY: `sid` points to a valid SID that outlives the call, and `text` is a valid
        // out pointer.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success `text` points to a NUL-terminated UTF-16 string, which stays valid
        // until the LocalFree below; the contents are copied before it.
        let result = unsafe {
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(slice::from_raw_parts(text, len))
        };
        // SAFETY: ConvertSidToStringSidW documents LocalFree for the string it allocated.
        unsafe {
            LocalFree(text.cast());
        }
        Ok(result)
    }

    /// The owner SID text and the DACL (`None` if null) of `path`, without following a
    /// reparse point.
    fn read_security(path: &Path) -> io::Result<(String, Option<Vec<Ace>>)> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the security attributes and
        // template handle are null, as allowed for an existing file.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Handle(raw);

        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle is open with READ_CONTROL; the group and SACL out pointers are
        // null because those parts are not requested; the others are valid out pointers.
        let status = unsafe {
            GetSecurityInfo(
                handle.0,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // `owner` and `dacl` point into the descriptor; everything is copied out before it is
        // freed when this guard drops.
        let _descriptor = Descriptor(descriptor);
        if owner.is_null() {
            return Err(io::Error::other("the file has no owner SID"));
        }
        let owner = sid_string(owner)?;
        if dacl.is_null() {
            return Ok((owner, None));
        }
        // SAFETY: `dacl` is non-null and points to a valid ACL inside the live descriptor.
        let count = unsafe { (*dacl).AceCount };
        let mut aces = Vec::with_capacity(usize::from(count));
        for index in 0..u32::from(count) {
            let mut raw_ace: *mut core::ffi::c_void = null_mut();
            // SAFETY: `dacl` is a valid ACL, `index` is below its ACE count, and `raw_ace` is a
            // valid out pointer.
            if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: GetAce returned a pointer to an ACE inside the descriptor; every ACE
            // starts with an ACE_HEADER.
            let header = unsafe { (raw_ace as *const ACE_HEADER).read_unaligned() };
            let decoded = match header.AceType {
                // The allowed and allowed-callback ACEs share the leading layout: header, mask,
                // then the trustee SID. Other types are not decoded.
                0 | 9 => {
                    let ace = raw_ace as *const ACCESS_ALLOWED_ACE;
                    // SAFETY: both ACE types are at least as large as ACCESS_ALLOWED_ACE
                    // (header, mask, SID start), and the SID begins at `SidStart`.
                    let (mask, sid) = unsafe {
                        (
                            addr_of!((*ace).Mask).read_unaligned(),
                            sid_string(addr_of!((*ace).SidStart) as PSID)?,
                        )
                    };
                    Ace {
                        ace_type: header.AceType,
                        flags: header.AceFlags,
                        mask,
                        sid,
                    }
                }
                other => Ace {
                    ace_type: other,
                    flags: header.AceFlags,
                    mask: 0,
                    sid: String::new(),
                },
            };
            aces.push(decoded);
        }
        Ok((owner, Some(aces)))
    }

    /// The user SID text of the current process token.
    #[cfg(any(test, debug_assertions))]
    pub(in crate::writability) fn current_user_sid() -> io::Result<String> {
        use windows_sys::Win32::{
            Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };

        let mut raw: HANDLE = null_mut();
        // SAFETY: the pseudo handle of the current process is valid, and `raw` is a valid out
        // pointer.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(raw);
        let mut needed = 0u32;
        // SAFETY: a zero-length query only reports the required size in `needed`; it is expected
        // to fail with an insufficient-buffer error, which is ignored.
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64 elements keep the buffer aligned for TOKEN_USER.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: the buffer is writable and at least `needed` bytes long.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points into the same
        // buffer, which is alive here.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        sid_string(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted() -> Vec<String> {
        SYSTEM_SIDS.iter().map(|s| (*s).to_owned()).collect()
    }

    const USERS: &str = "S-1-5-32-545";
    const SYSTEM: &str = "S-1-5-18";
    /// Write data on a file, or add-file on a directory.
    const WD: u32 = 0x2;
    /// Append data on a file, or add-subdirectory on a directory.
    const AD: u32 = 0x4;

    fn allow(sid: &str, mask: u32) -> Ace {
        Ace {
            ace_type: 0,
            flags: 0,
            mask,
            sid: sid.to_owned(),
        }
    }

    fn check(owner: &str, dacl: Option<&[Ace]>, role: Role) -> Vec<Reason> {
        reasons(owner, dacl, role, &trusted())
    }

    #[test]
    fn well_known_sids_are_well_formed() {
        for sid in SYSTEM_SIDS {
            assert!(sid.starts_with("S-1-5-"));
        }
    }

    #[test]
    fn clean_descriptor_passes() {
        let dacl = [allow(SYSTEM, 0x1f_01ff), allow(USERS, 0x1200a9)];
        for role in [
            Role::Library,
            Role::Directory,
            Role::LibraryDirectory,
            Role::NamingFile,
        ] {
            assert!(check(SYSTEM, Some(&dacl), role).is_empty(), "{role}");
        }
    }

    #[test]
    fn foreign_owner_is_found() {
        let r = check("S-1-5-21-1-2-3-1001", Some(&[]), Role::Library);
        assert_eq!(
            r,
            [Reason::UnprivilegedOwner("S-1-5-21-1-2-3-1001".to_owned())]
        );
    }

    #[test]
    fn users_write_on_a_file_is_found() {
        let dacl = [allow(USERS, WD)];
        let r = check(SYSTEM, Some(&dacl), Role::Library);
        assert_eq!(
            r,
            [Reason::WritableByRegularUsers(format!("{USERS} mask 0x2"))]
        );
        assert_eq!(check(SYSTEM, Some(&dacl), Role::NamingFile).len(), 1);
        assert_eq!(check(SYSTEM, Some(&dacl), Role::Link).len(), 1);
    }

    #[test]
    fn modify_and_ownership_rights_are_found() {
        for mask in [
            0x1301bf,
            0x1_0000,
            0x4_0000,
            0x8_0000,
            0x4000_0000,
            0x1000_0000,
            0x0200_0000,
        ] {
            let dacl = [allow(USERS, mask)];
            for role in [Role::Library, Role::Directory, Role::LibraryDirectory] {
                assert_eq!(
                    check(SYSTEM, Some(&dacl), role).len(),
                    1,
                    "{mask:#x} {role}"
                );
            }
        }
    }

    #[test]
    fn add_only_passes_on_a_container_but_not_on_the_library_directory() {
        let dacl = [allow(USERS, WD | AD)];
        assert!(check(SYSTEM, Some(&dacl), Role::Directory).is_empty());
        assert_eq!(check(SYSTEM, Some(&dacl), Role::LibraryDirectory).len(), 1);
    }

    #[test]
    fn delete_child_is_found_on_a_container() {
        let dacl = [allow(USERS, 0x40)];
        assert_eq!(check(SYSTEM, Some(&dacl), Role::Directory).len(), 1);
    }

    #[test]
    fn deny_ace_is_ignored() {
        let deny = Ace {
            ace_type: 1,
            flags: 0,
            mask: 0x1f_01ff,
            sid: USERS.to_owned(),
        };
        let deny_callback = Ace {
            ace_type: 10,
            ..deny.clone()
        };
        assert!(check(SYSTEM, Some(&[deny, deny_callback]), Role::Library).is_empty());
    }

    #[test]
    fn inherit_only_ace_is_skipped() {
        let ace = Ace {
            flags: 0x08 | 0x03,
            ..allow(USERS, 0x1f_01ff)
        };
        assert!(check(SYSTEM, Some(&[ace]), Role::Directory).is_empty());
    }

    #[test]
    fn callback_allow_ace_counts() {
        let ace = Ace {
            ace_type: 9,
            ..allow(USERS, WD)
        };
        assert_eq!(check(SYSTEM, Some(&[ace]), Role::Library).len(), 1);
    }

    #[test]
    fn owner_rights_ace_is_skipped() {
        assert!(check(SYSTEM, Some(&[allow("S-1-3-4", 0x1f_01ff)]), Role::Library).is_empty());
    }

    #[test]
    fn null_dacl_is_found() {
        assert_eq!(check(SYSTEM, None, Role::Library), [Reason::NullDacl]);
    }

    #[test]
    fn unknown_ace_type_is_found() {
        let ace = Ace {
            ace_type: 5,
            flags: 0,
            mask: 0,
            sid: String::new(),
        };
        assert_eq!(
            check(SYSTEM, Some(&[ace]), Role::Library),
            [Reason::WritableByRegularUsers(
                "unknown ACE type 5".to_owned()
            )]
        );
    }

    #[test]
    fn extra_trusted_sid_is_honoured() {
        let mut policy = trusted();
        policy.push(USERS.to_owned());
        let dacl = [allow(USERS, WD)];
        assert!(reasons(USERS, Some(&dacl), Role::Library, &policy).is_empty());
    }
}

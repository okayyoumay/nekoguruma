//! The Unix rule (7.2, ADR-270) and the probe that feeds it from `lstat`.
#![cfg_attr(
    all(not(unix), not(test)),
    expect(dead_code, reason = "only the Unix probe uses the Unix rule")
)]

use super::{Reason, Role};

/// Group-write and other-write permission bits.
const WRITE_BITS: u32 = 0o022;
/// The sticky bit: in a directory, entries can be removed or renamed only by their owner.
const STICKY: u32 = 0o1000;

/// The reasons an entry fails, from its owner, mode and role.
///
/// A link is checked for its owner only: its own mode is meaningless and the target is walked
/// separately. A container directory (`Role::Directory`) may be sticky, which keeps others from
/// replacing what is in it; the library's own directory may not, since an add-only directory is
/// not enough there.
pub(super) fn reasons(owner: u32, mode: u32, role: Role, trusted: &[u32]) -> Vec<Reason> {
    let mut out = Vec::new();
    if !trusted.contains(&owner) {
        out.push(Reason::UnprivilegedOwner(format!("uid {owner}")));
    }
    if role == Role::Link {
        return out;
    }
    let sticky_ok = role == Role::Directory && mode & STICKY != 0;
    if mode & WRITE_BITS != 0 && !sticky_ok {
        out.push(Reason::WritableByRegularUsers(format!(
            "mode {:04o}",
            mode & 0o7777
        )));
    }
    out
}

#[cfg(unix)]
pub(super) struct UnixProbe {
    pub(super) uids: Vec<u32>,
}

#[cfg(unix)]
impl super::Probe for UnixProbe {
    fn inspect(&self, path: &std::path::Path, role: Role) -> std::io::Result<super::Inspected> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(path)?;
        let kind = if meta.file_type().is_symlink() {
            super::Kind::Link
        } else if meta.is_dir() {
            super::Kind::Dir
        } else if meta.is_file() {
            super::Kind::File
        } else {
            super::Kind::Other
        };
        let role = if kind == super::Kind::Link {
            Role::Link
        } else {
            role
        };
        Ok(super::Inspected {
            kind,
            reasons: reasons(meta.uid(), meta.mode(), role, &self.uids),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &[u32] = &[0];

    fn writable(r: &[Reason]) -> bool {
        r.iter()
            .any(|x| matches!(x, Reason::WritableByRegularUsers(_)))
    }

    #[test]
    fn clean_entries_pass() {
        assert!(reasons(0, 0o755, Role::Directory, ROOT).is_empty());
        assert!(reasons(0, 0o644, Role::Library, ROOT).is_empty());
        assert!(reasons(0, 0o755, Role::LibraryDirectory, ROOT).is_empty());
    }

    #[test]
    fn foreign_owner_is_found() {
        let r = reasons(1000, 0o755, Role::Directory, ROOT);
        assert_eq!(r, [Reason::UnprivilegedOwner("uid 1000".to_owned())]);
    }

    #[test]
    fn group_or_other_write_is_found() {
        assert!(writable(&reasons(0, 0o666, Role::Library, ROOT)));
        assert!(writable(&reasons(0, 0o775, Role::Directory, ROOT)));
        assert!(writable(&reasons(0, 0o777, Role::Directory, ROOT)));
        assert!(writable(&reasons(0, 0o664, Role::NamingFile, ROOT)));
        assert!(writable(&reasons(0, 0o757, Role::LibraryDirectory, ROOT)));
    }

    #[test]
    fn sticky_container_passes_but_library_directory_does_not() {
        assert!(reasons(0, 0o1777, Role::Directory, ROOT).is_empty());
        assert!(writable(&reasons(0, 0o1777, Role::LibraryDirectory, ROOT)));
        assert!(writable(&reasons(0, 0o1777, Role::Library, ROOT)));
    }

    #[test]
    fn link_is_checked_for_its_owner_only() {
        assert!(reasons(0, 0o777, Role::Link, ROOT).is_empty());
        assert_eq!(reasons(5, 0o777, Role::Link, ROOT).len(), 1);
    }

    #[test]
    fn extra_trusted_uid_is_honoured() {
        assert!(reasons(1000, 0o755, Role::Library, &[0, 1000]).is_empty());
    }
}

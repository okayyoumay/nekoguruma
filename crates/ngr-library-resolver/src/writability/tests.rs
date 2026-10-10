//! Walker tests on real files. The per-entry rules are tested next to the rules themselves.

use std::{fs, path::Path};

use super::*;

/// The roles and reasons of the findings, for compact assertions.
fn found(r: &Result<(), WritabilityError>) -> Vec<(Role, &Reason)> {
    match r {
        Ok(()) => Vec::new(),
        Err(e) => e.findings.iter().map(|f| (f.role, &f.reason)).collect(),
    }
}

fn strictly_find(e: &WritabilityError, path: &Path, role: Role) -> bool {
    e.findings.iter().any(|f| f.path == path && f.role == role)
}

#[test]
fn relative_paths_are_not_absolute() {
    let policy = Policy::system().trusting_current_user();
    let r = check_writability_with(Path::new("lib.so"), &[PathBuf::from("x.toml")], &policy);
    let e = r.unwrap_err();
    assert_eq!(e.findings.len(), 2);
    assert!(e.findings.iter().all(|f| f.reason == Reason::NotAbsolute));
    assert_eq!(e.findings[0].role, Role::Library);
    assert_eq!(e.findings[1].role, Role::NamingFile);
}

#[test]
fn error_display_lists_one_finding_per_line() {
    let e = WritabilityError {
        findings: vec![
            Finding {
                path: PathBuf::from("a"),
                role: Role::Library,
                reason: Reason::NotAbsolute,
            },
            Finding {
                path: PathBuf::from("b"),
                role: Role::Directory,
                reason: Reason::NullDacl,
            },
        ],
    };
    assert_eq!(e.to_string().lines().count(), 2);
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    /// A tree `base/dir/lib.so` and `base/naming/x.toml` with clean modes.
    struct Tree {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        dir: PathBuf,
        lib: PathBuf,
        naming: PathBuf,
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let dir = base.join("dir");
        let naming_dir = base.join("naming");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&naming_dir).unwrap();
        let lib = dir.join("lib.so");
        let naming = naming_dir.join("x.toml");
        fs::write(&lib, b"x").unwrap();
        fs::write(&naming, b"x").unwrap();
        for d in [&base, &dir, &naming_dir] {
            chmod(d, 0o755);
        }
        for f in [&lib, &naming] {
            chmod(f, 0o644);
        }
        Tree {
            _tmp: tmp,
            base,
            dir,
            lib,
            naming,
        }
    }

    fn policy() -> Policy {
        Policy::system().trusting_current_user()
    }

    fn check(t: &Tree) -> Result<(), WritabilityError> {
        check_writability_with(&t.lib, std::slice::from_ref(&t.naming), &policy())
    }

    #[test]
    fn clean_tree_passes() {
        let t = tree();
        let r = check(&t);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn world_writable_library_is_found() {
        let t = tree();
        chmod(&t.lib, 0o666);
        let e = check(&t).unwrap_err();
        assert_eq!(e.findings.len(), 1);
        assert_eq!(e.findings[0].path, t.lib);
        assert_eq!(e.findings[0].role, Role::Library);
        assert!(matches!(
            e.findings[0].reason,
            Reason::WritableByRegularUsers(_)
        ));
    }

    #[test]
    fn group_writable_naming_file_is_found() {
        let t = tree();
        chmod(&t.naming, 0o664);
        let e = check(&t).unwrap_err();
        assert_eq!(e.findings.len(), 1);
        assert!(strictly_find(&e, &t.naming, Role::NamingFile));
    }

    #[test]
    fn writable_ancestor_is_found_as_a_container_and_once() {
        let t = tree();
        chmod(&t.base, 0o777);
        let e = check(&t).unwrap_err();
        // Reached by the library walk and by the naming walk, reported once.
        assert_eq!(e.findings.len(), 1, "{e}");
        assert!(strictly_find(&e, &t.base, Role::Directory));
    }

    #[test]
    fn sticky_ancestor_passes() {
        let t = tree();
        chmod(&t.base, 0o1777);
        let r = check(&t);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn sticky_library_directory_is_found() {
        let t = tree();
        chmod(&t.dir, 0o1777);
        let e = check(&t).unwrap_err();
        assert_eq!(e.findings.len(), 1);
        assert!(strictly_find(&e, &t.dir, Role::LibraryDirectory));
    }

    #[test]
    fn directory_shared_by_both_walks_keeps_the_stricter_role() {
        // The naming file sits in the library's directory.
        let t = tree();
        let naming = t.dir.join("x.toml");
        fs::write(&naming, b"x").unwrap();
        chmod(&naming, 0o644);
        chmod(&t.dir, 0o777);
        let e = check_writability_with(&t.lib, &[naming], &policy()).unwrap_err();
        assert_eq!(e.findings.len(), 1, "{e}");
        assert!(strictly_find(&e, &t.dir, Role::LibraryDirectory));
    }

    #[test]
    fn untrusted_owner_is_found() {
        // The same tree under the system policy alone: the fixtures belong to the test user,
        // which is not root unless the test runs as root.
        let t = tree();
        // SAFETY: geteuid has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let e = check_writability_with(&t.lib, &[], &Policy::system()).unwrap_err();
        assert!(
            e.findings
                .iter()
                .any(|f| matches!(f.reason, Reason::UnprivilegedOwner(_)))
        );
    }

    #[test]
    fn symlinked_directory_is_followed() {
        let t = tree();
        let link = t.base.join("link");
        symlink(&t.dir, &link).unwrap();
        let via = link.join("lib.so");
        assert!(check_writability_with(&via, &[], &policy()).is_ok());
        // The directory the link points to is still checked, as the library directory.
        chmod(&t.dir, 0o777);
        let e = check_writability_with(&via, &[], &policy()).unwrap_err();
        assert!(strictly_find(&e, &t.dir, Role::LibraryDirectory));
    }

    #[test]
    fn symlinked_library_is_followed_and_the_link_is_not_judged_by_its_mode() {
        let t = tree();
        let link = t.base.join("lib-link.so");
        symlink(&t.lib, &link).unwrap();
        assert!(check_writability_with(&link, &[], &policy()).is_ok());
        chmod(&t.lib, 0o666);
        let e = check_writability_with(&link, &[], &policy()).unwrap_err();
        assert_eq!(e.findings.len(), 1);
        assert!(strictly_find(&e, &t.lib, Role::Library));
    }

    #[test]
    fn relative_link_and_dotdot_resolve() {
        let t = tree();
        let link = t.dir.join("rel.so");
        symlink("../dir/lib.so", &link).unwrap();
        assert!(check_writability_with(&link, &[], &policy()).is_ok());
        chmod(&t.lib, 0o666);
        let e = check_writability_with(&link, &[], &policy()).unwrap_err();
        assert!(strictly_find(&e, &t.lib, Role::Library));
    }

    #[test]
    fn link_loop_is_too_many_links() {
        let t = tree();
        let link = t.dir.join("loop.so");
        symlink(&link, &link).unwrap();
        let e = check_writability_with(&link, &[], &policy()).unwrap_err();
        assert!(e.findings.iter().any(|f| f.reason == Reason::TooManyLinks));
    }

    #[test]
    fn directory_as_library_is_not_a_regular_file() {
        let t = tree();
        let r = check_writability_with(&t.dir, &[], &policy());
        assert!(found(&r).contains(&(Role::Library, &Reason::NotARegularFile)));
    }

    #[test]
    fn missing_library_is_an_io_finding() {
        let t = tree();
        let r = check_writability_with(&t.dir.join("none.so"), &[], &policy());
        assert!(matches!(found(&r)[..], [(Role::Library, Reason::Io(_))]));
    }

    #[test]
    fn system_shell_passes_under_the_system_policy() {
        let sh = Path::new("/bin/sh");
        if !sh.exists() {
            return;
        }
        let r = check_writability(sh, &[]);
        assert!(r.is_ok(), "{r:?}");
    }
}

#[cfg(windows)]
mod windows {
    use std::process::Command;

    use super::*;

    const USERS: &str = "*S-1-5-32-545";

    struct Tree {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        dir: PathBuf,
        lib: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let dir = base.join("dir");
        fs::create_dir(&dir).unwrap();
        let lib = dir.join("lib.dll");
        fs::write(&lib, b"x").unwrap();
        Tree {
            _tmp: tmp,
            base,
            dir,
            lib,
        }
    }

    fn grant(path: &Path, rights: &str) {
        let status = Command::new("icacls")
            .arg(path)
            .arg("/grant")
            .arg(format!("{USERS}:{rights}"))
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn check(t: &Tree) -> Result<(), WritabilityError> {
        check_writability_with(&t.lib, &[], &Policy::system().trusting_current_user())
    }

    #[test]
    fn clean_tree_passes() {
        let t = tree();
        let r = check(&t);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn users_write_on_the_library_is_found() {
        let t = tree();
        grant(&t.lib, "(W)");
        let e = check(&t).unwrap_err();
        assert!(strictly_find(&e, &t.lib, Role::Library));
    }

    #[test]
    fn users_modify_on_an_ancestor_is_found() {
        let t = tree();
        grant(&t.base, "(M)");
        let e = check(&t).unwrap_err();
        assert!(strictly_find(&e, &t.base, Role::Directory));
    }

    #[test]
    fn add_only_passes_on_an_ancestor() {
        let t = tree();
        grant(&t.base, "(WD,AD)");
        let r = check(&t);
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn add_only_is_found_on_the_library_directory() {
        let t = tree();
        grant(&t.dir, "(WD,AD)");
        let e = check(&t).unwrap_err();
        assert!(strictly_find(&e, &t.dir, Role::LibraryDirectory));
    }

    #[test]
    fn system_library_passes_under_the_system_policy() {
        let dll = Path::new(r"C:\Windows\System32\kernel32.dll");
        let r = check_writability(dll, &[]);
        assert!(r.is_ok(), "{r:?}");
    }
}

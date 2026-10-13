//! The design 7.2 pre-load checks on the resolved library, and what the operating mode makes of
//! a failed one (ADR-281 Decision item 2).

use std::fs::File;

use ngr_library_resolver::{Policy, Signer, j2534_0404::Resolved};
use tonic::Status;
use tracing::{error, info, warn};
use vci_service_launcher::mode::OperatingMode;

/// What the checks keep open until the library is mapped, so the file they checked is the file
/// that is loaded (ADR-278).
#[derive(Debug)]
pub struct CheckedLibrary {
    _signer: Option<Signer>,
    _held: Option<File>,
}

/// Runs the writability and signer checks on `resolved`'s library and the file that named it (a
/// registration definition; a registry hit has none). The launcher has already checked the
/// worker's configuration file, before reading any setting from it (ADR-285).
///
/// A library that passes is reported with its signer. One that fails is refused in device mode,
/// with a client message that names no path (ADR-089) and the findings in the log; in user mode
/// the findings are logged as a warning and the library is loaded.
pub fn check_library(resolved: &Resolved, mode: OperatingMode) -> Result<CheckedLibrary, Status> {
    check_library_with(resolved, mode, &Policy::system())
}

/// [`check_library`] with the owners `policy` trusts instead of the system owners, so tests can
/// trust the user they run as.
fn check_library_with(
    resolved: &Resolved,
    mode: OperatingMode,
    policy: &Policy,
) -> Result<CheckedLibrary, Status> {
    let library = &resolved.definition.library;
    match ngr_library_resolver::check_library(library, &resolved.naming_files(), policy) {
        Ok(signer) => {
            match &signer {
                Signer::Trusted {
                    subject,
                    thumbprint_sha256,
                    ..
                } => info!(
                    library = %library.display(),
                    signer = %subject,
                    thumbprint_sha256 = %thumbprint_sha256,
                    "library passed the pre-load checks"
                ),
                Signer::Unsigned { .. } | Signer::NotApplicable => info!(
                    library = %library.display(),
                    "library passed the pre-load checks"
                ),
            }
            Ok(CheckedLibrary {
                _signer: Some(signer),
                _held: None,
            })
        }
        Err(failed) => match mode {
            OperatingMode::Device => {
                error!(
                    library = %library.display(),
                    findings = %failed,
                    "refusing the library in device mode: it failed the pre-load checks"
                );
                Err(Status::failed_precondition(
                    "the J2534 library failed the pre-load checks and is not loaded in device mode",
                ))
            }
            OperatingMode::User => {
                warn!(
                    library = %library.display(),
                    findings = %failed,
                    "loading the library in user mode although it failed the pre-load checks"
                );
                Ok(CheckedLibrary {
                    _signer: failed.signer,
                    _held: failed.held,
                })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use ngr_library_resolver::j2534_0404::{Definition, Source};
    use tonic::Code;

    use super::*;

    /// A library file and a registration definition in a fresh directory, both owned by the user
    /// the test runs as and writable only by that user.
    fn fixture() -> (tempfile::TempDir, Resolved) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let dir = std::fs::canonicalize(tmp.path()).expect("canonicalize temp dir");
        // An unsigned executable image, so the Windows signer check has a real PE file to read.
        let library = dir.join("lib.bin");
        std::fs::copy(std::env::current_exe().expect("test executable"), &library)
            .expect("copy library");
        let definition =
            j2534_0404_mock::write_definition(&dir, "Lib", &library).expect("write definition");
        let resolved = Resolved {
            definition: Definition {
                name: "Lib".into(),
                vendor: None,
                library,
                config_application: None,
                protocols: Vec::new(),
                long_size: None,
                search_paths: Vec::new(),
            },
            source: Source::Definition(definition),
        };
        (tmp, resolved)
    }

    #[cfg(unix)]
    fn make_writable_by_regular_users(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
            .expect("make the file world-writable");
    }

    #[cfg(windows)]
    fn make_writable_by_regular_users(path: &Path) {
        // BUILTIN\Users gets write access to the file.
        let status = std::process::Command::new("icacls")
            .arg(path)
            .arg("/grant")
            .arg("*S-1-5-32-545:(W)")
            .status()
            .expect("run icacls");
        assert!(status.success(), "icacls failed: {status}");
    }

    fn policy() -> Policy {
        Policy::system().trusting_current_user()
    }

    fn library(resolved: &Resolved) -> PathBuf {
        resolved.definition.library.clone()
    }

    fn definition_file(resolved: &Resolved) -> PathBuf {
        match &resolved.source {
            Source::Definition(path) => path.clone(),
            Source::Registry => unreachable!("the fixture resolves through a definition"),
        }
    }

    #[test]
    fn a_clean_library_is_accepted_in_both_modes() {
        let (_tmp, resolved) = fixture();
        for mode in [OperatingMode::Device, OperatingMode::User] {
            let checked = check_library_with(&resolved, mode, &policy());
            assert!(checked.is_ok(), "{mode}: {checked:?}");
        }
    }

    #[test]
    fn device_mode_refuses_a_writable_library_without_naming_it() {
        let (_tmp, resolved) = fixture();
        make_writable_by_regular_users(&library(&resolved));
        let status = check_library_with(&resolved, OperatingMode::Device, &policy())
            .expect_err("device mode should refuse the library");
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert!(
            !status
                .message()
                .contains(&*library(&resolved).to_string_lossy()),
            "{}",
            status.message()
        );
    }

    #[test]
    fn user_mode_accepts_a_writable_library() {
        let (_tmp, resolved) = fixture();
        make_writable_by_regular_users(&library(&resolved));
        let checked = check_library_with(&resolved, OperatingMode::User, &policy());
        assert!(checked.is_ok(), "{checked:?}");
    }

    #[test]
    fn device_mode_refuses_a_writable_definition() {
        let (_tmp, resolved) = fixture();
        make_writable_by_regular_users(&definition_file(&resolved));
        let status = check_library_with(&resolved, OperatingMode::Device, &policy())
            .expect_err("device mode should refuse a library named by a writable definition");
        assert_eq!(status.code(), Code::FailedPrecondition);
    }
}

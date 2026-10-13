//! The design 7.2 writability check on the worker's configuration file, run before any setting
//! is read from it (ADR-270 Decision item 5, ADR-285).
//!
//! The file holds the logging settings, which name a file the worker opens, and the per-library
//! settings the service acts on. The launcher checks it once, before logging starts, and hands
//! the verdict to the service, which takes its settings from the file only when the verdict
//! allows it.

use std::path::{Path, PathBuf};

use ngr_library_resolver::{Policy, WritabilityError};
use tracing::{error, warn};

use crate::{BoxError, mode::OperatingMode};

/// What the service may take from its configuration file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFile {
    /// There was no file when the launcher checked. A file created later has passed no check, so
    /// nothing is configured for the worker's lifetime.
    Absent,
    /// The file passed the writability check.
    Checked,
    /// The file failed the writability check and user mode accepted it; the findings were logged
    /// as a warning.
    AcceptedWithFindings,
}

impl ConfigFile {
    /// Whether the service takes its settings from the file.
    pub fn settings_apply(self) -> bool {
        !matches!(self, ConfigFile::Absent)
    }
}

/// What checking the configuration file found, before the operating mode is applied.
#[derive(Debug)]
pub(crate) enum Outcome {
    Absent,
    Passed,
    Failed(WritabilityError),
}

impl Outcome {
    /// Whether the launcher may read the logging settings from the file in `mode`: device mode
    /// does not read a file that failed the check.
    pub(crate) fn settings_readable(&self, mode: OperatingMode) -> bool {
        match self {
            Outcome::Absent => false,
            Outcome::Passed => true,
            Outcome::Failed(_) => mode == OperatingMode::User,
        }
    }

    /// Applies `mode` to the outcome, logging any findings; logging must be set up by now. Device
    /// mode refuses a file that failed the check, with an error that names no path (ADR-089).
    pub(crate) fn verdict(self, mode: OperatingMode) -> Result<ConfigFile, BoxError> {
        match self {
            Outcome::Absent => Ok(ConfigFile::Absent),
            Outcome::Passed => Ok(ConfigFile::Checked),
            Outcome::Failed(findings) => match mode {
                OperatingMode::Device => {
                    error!(
                        findings = %findings,
                        "refusing to start in device mode: the configuration file failed the pre-load checks"
                    );
                    Err("the configuration file failed the pre-load checks; \
                         the service does not start in device mode"
                        .into())
                }
                OperatingMode::User => {
                    warn!(
                        findings = %findings,
                        "using the configuration file in user mode although it failed the pre-load checks"
                    );
                    Ok(ConfigFile::AcceptedWithFindings)
                }
            },
        }
    }
}

/// Checks the configuration file at its fixed location against the system owners.
pub(crate) fn check() -> Outcome {
    check_with(&crate::config::config_file(), &Policy::system())
}

/// Checks `config` against the owners `policy` trusts. Only a file that is not there counts as
/// absent: any other failure to probe it (a parent directory the worker cannot search, say)
/// goes to the check, which reports it, instead of the worker running on defaults that select a
/// device the configuration did not name.
fn check_with(config: &Path, policy: &Policy) -> Outcome {
    match std::fs::symlink_metadata(config) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Absent,
        _ => match ngr_library_resolver::check_files_with(&[PathBuf::from(config)], policy) {
            Ok(()) => Outcome::Passed,
            Err(findings) => Outcome::Failed(findings),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration file in a fresh directory, owned by the user the test runs as and
    /// writable only by that user.
    fn config_file() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let config = std::fs::canonicalize(tmp.path())
            .expect("canonicalize temp dir")
            .join("config.toml");
        std::fs::write(&config, "").expect("write config file");
        (tmp, config)
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

    #[cfg(debug_assertions)]
    fn policy() -> Policy {
        Policy::system().trusting_current_user()
    }

    #[test]
    fn a_missing_file_configures_nothing_in_both_modes() {
        let (tmp, _config) = config_file();
        let missing = tmp.path().join("missing.toml");
        for mode in [OperatingMode::Device, OperatingMode::User] {
            let outcome = check_with(&missing, &Policy::system());
            assert!(matches!(outcome, Outcome::Absent), "{outcome:?}");
            assert!(!outcome.settings_readable(mode));
            let verdict = outcome.verdict(mode).expect("no file is not a refusal");
            assert_eq!(verdict, ConfigFile::Absent);
            assert!(!verdict.settings_apply());
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_clean_file_is_read_in_both_modes() {
        let (_tmp, config) = config_file();
        for mode in [OperatingMode::Device, OperatingMode::User] {
            let outcome = check_with(&config, &policy());
            assert!(matches!(outcome, Outcome::Passed), "{outcome:?}");
            assert!(outcome.settings_readable(mode));
            assert_eq!(outcome.verdict(mode).expect("clean"), ConfigFile::Checked);
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn device_mode_reads_nothing_from_a_writable_file_and_refuses_to_start() {
        let (_tmp, config) = config_file();
        make_writable_by_regular_users(&config);
        let outcome = check_with(&config, &policy());
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        // Logging starts with its defaults instead of the file's settings.
        assert!(!outcome.settings_readable(OperatingMode::Device));
        let error = outcome
            .verdict(OperatingMode::Device)
            .expect_err("device mode should refuse to start");
        let message = error.to_string();
        assert!(message.contains("pre-load checks"), "{message}");
        // The error names no local path (ADR-089).
        assert!(!message.contains(&*config.to_string_lossy()), "{message}");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn user_mode_reads_a_writable_file_and_starts() {
        let (_tmp, config) = config_file();
        make_writable_by_regular_users(&config);
        let outcome = check_with(&config, &policy());
        assert!(outcome.settings_readable(OperatingMode::User));
        let verdict = outcome
            .verdict(OperatingMode::User)
            .expect("user mode starts");
        assert_eq!(verdict, ConfigFile::AcceptedWithFindings);
        assert!(verdict.settings_apply());
    }
}

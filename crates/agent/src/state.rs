//! Where the agent keeps its state on the device (design 6.2, ADR-460).
//!
//! The state belongs to the device's operating mode and has one root per mode:
//!
//! | Mode | OS | Root | Created by |
//! |---|---|---|---|
//! | user | Unix | `$XDG_STATE_HOME/nekoguruma` (absolute), else `~/.local/state/nekoguruma` | the agent |
//! | user | Windows | `%LOCALAPPDATA%\nekoguruma` | the agent |
//! | device | Linux and other Unix | `/var/lib/nekoguruma` | the installation |
//! | device | Windows | the service account's own `%LOCALAPPDATA%\nekoguruma` | the agent |
//!
//! The installation creates the Unix device root (systemd's `StateDirectory=`), so the agent
//! creates only the sub-directories under it and a missing root is an error, not something to
//! make. Each kind of state has a sub-directory of its own ([`private_subdir`]): `identity`
//! (the agent key's lock and, in a debug build, its test store), `registration` (the registration
//! record and its lock, ADR-470) and `journal` (the write-job journal, which a later change wires
//! in).
//!
//! A debug build reads `NGR_AGENT_STATE_DIR`, an absolute directory that replaces the root for
//! both modes, so tests keep their state in a temporary directory. A release build never reads
//! it (the pattern of `vci-service-config`'s configuration directory, ADR-228).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use worker_host::service::OperatingMode;

/// The debug-only variable that replaces the state root.
pub const STATE_DIR_VAR: &str = "NGR_AGENT_STATE_DIR";

/// The directory name under the platform's state location.
const ROOT_NAME: &str = "nekoguruma";

/// The Unix device root, which the installation creates.
#[cfg(unix)]
const DEVICE_ROOT: &str = "/var/lib/nekoguruma";

/// Why a state directory cannot be had. The text holds no path that names a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDirError(String);

impl StateDirError {
    fn new(why: impl Into<String>) -> Self {
        Self(why.into())
    }
}

impl fmt::Display for StateDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateDirError {}

/// The user's state directory from the environment `var` reads: an absolute `XDG_STATE_HOME`,
/// else `~/.local/state` (Unix), or `LOCALAPPDATA` (Windows). The agent's log directory is built
/// on it as well (design 12.2).
pub(crate) fn user_base_dir(
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, StateDirError> {
    let absolute = |name: &str| {
        var(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if cfg!(windows) {
        absolute("LOCALAPPDATA").ok_or_else(|| StateDirError::new("LOCALAPPDATA is not set"))
    } else {
        absolute("XDG_STATE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local").join("state")))
            .ok_or_else(|| StateDirError::new("neither XDG_STATE_HOME nor HOME is set"))
    }
}

/// The state root of `mode`, without touching the file system.
pub fn state_root(mode: OperatingMode) -> Result<PathBuf, StateDirError> {
    root_from(mode, &|name| std::env::var_os(name))
}

fn root_from(
    mode: OperatingMode,
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, StateDirError> {
    #[cfg(debug_assertions)]
    if let Some(dir) = var(STATE_DIR_VAR) {
        let dir = PathBuf::from(dir);
        return if dir.is_absolute() {
            Ok(dir)
        } else {
            Err(StateDirError::new(format!(
                "{STATE_DIR_VAR} must be an absolute directory"
            )))
        };
    }
    #[cfg(unix)]
    if mode == OperatingMode::Device {
        return Ok(PathBuf::from(DEVICE_ROOT));
    }
    // The Windows service account's profile is its own (user area), and a user's is theirs.
    let _ = mode;
    user_base_dir(var).map(|base| base.join(ROOT_NAME))
}

/// Whether the agent makes the root of `mode` itself: not where the installation does.
fn agent_creates_root(mode: OperatingMode) -> bool {
    !(cfg!(unix) && mode == OperatingMode::Device) || cfg!(debug_assertions) && override_set()
}

/// Whether the debug override names the root. A release build has no override.
fn override_set() -> bool {
    cfg!(debug_assertions) && std::env::var_os(STATE_DIR_VAR).is_some()
}

/// The sub-directory `name` of `mode`'s state root, created when missing with permissions only
/// the agent's account has (0700 on Unix), and refused on Unix when it is writable by its group
/// or others. The root is created too, except where the installation makes it (Unix device
/// mode), where a missing root is an error.
pub fn private_subdir(mode: OperatingMode, name: &str) -> Result<PathBuf, StateDirError> {
    let root = state_root(mode)?;
    prepare_subdir(&root, agent_creates_root(mode), name)
}

/// [`private_subdir`] for a given root.
pub(crate) fn prepare_subdir(
    root: &Path,
    create_root: bool,
    name: &str,
) -> Result<PathBuf, StateDirError> {
    if create_root {
        let existed = root.exists();
        make_dir_all(root).map_err(|error| io_error("create the state directory", &error))?;
        if !existed {
            sync_parent(root)?;
        }
    } else if !root.is_dir() {
        return Err(StateDirError::new(
            "the state directory is missing (the installation creates it)",
        ));
    }
    let dir = root.join(name);
    match make_dir(&dir) {
        Ok(()) => sync_parent(&dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error("create a state sub-directory", &error)),
    }
    check_private(&dir)?;
    Ok(dir)
}

/// Makes the entry of a directory just created durable, by syncing the directory it is in.
fn sync_parent(dir: &Path) -> Result<(), StateDirError> {
    match dir.parent() {
        Some(parent) => crate::journal::sync_dir(parent)
            .map_err(|error| io_error("sync the state directory", &error)),
        None => Ok(()),
    }
}

fn io_error(what: &str, error: &std::io::Error) -> StateDirError {
    StateDirError::new(format!("cannot {what}: {:?}", error.kind()))
}

#[cfg(unix)]
fn make_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn make_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn make_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn make_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// The directory must be a directory, and on Unix not writable by its group or others (the rule
/// of `guards::prepare_dir`), since another account could plant files in it.
fn check_private(dir: &Path) -> Result<(), StateDirError> {
    let meta =
        std::fs::metadata(dir).map_err(|error| io_error("read a state directory", &error))?;
    if !meta.is_dir() {
        return Err(StateDirError::new(
            "a state sub-directory is not a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o022 != 0 {
            return Err(StateDirError::new(
                "a state sub-directory is writable by its group or others",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    /// The user's state directory comes from an absolute `XDG_STATE_HOME`, else `HOME` (Unix), or
    /// `LOCALAPPDATA` (Windows).
    #[test]
    fn the_state_directory_comes_from_the_environment() {
        if cfg!(windows) {
            assert_eq!(
                user_base_dir(env(&[("LOCALAPPDATA", r"C:\Users\a\AppData\Local")])).unwrap(),
                PathBuf::from(r"C:\Users\a\AppData\Local")
            );
            assert!(user_base_dir(env(&[])).is_err());
        } else {
            assert_eq!(
                user_base_dir(env(&[("XDG_STATE_HOME", "/x/state"), ("HOME", "/home/a")])).unwrap(),
                PathBuf::from("/x/state")
            );
            assert_eq!(
                user_base_dir(env(&[("XDG_STATE_HOME", "relative"), ("HOME", "/home/a")])).unwrap(),
                PathBuf::from("/home/a/.local/state")
            );
            assert!(user_base_dir(env(&[])).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn each_mode_has_its_root() {
        let var = env(&[("XDG_STATE_HOME", "/x/state")]);
        assert_eq!(
            root_from(OperatingMode::User, &var).unwrap(),
            PathBuf::from("/x/state/nekoguruma")
        );
        assert_eq!(
            root_from(OperatingMode::Device, &var).unwrap(),
            PathBuf::from("/var/lib/nekoguruma")
        );
    }

    #[cfg(windows)]
    #[test]
    fn both_modes_use_the_accounts_own_profile_on_windows() {
        let var = env(&[("LOCALAPPDATA", r"C:\Users\a\AppData\Local")]);
        for mode in [OperatingMode::User, OperatingMode::Device] {
            assert_eq!(
                root_from(mode, &var).unwrap(),
                PathBuf::from(r"C:\Users\a\AppData\Local\nekoguruma")
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_debug_build_takes_an_absolute_override_for_both_modes() {
        let abs = if cfg!(windows) {
            r"C:\t\state"
        } else {
            "/t/state"
        };
        let var = move |name: &str| (name == STATE_DIR_VAR).then(|| OsString::from(abs));
        for mode in [OperatingMode::User, OperatingMode::Device] {
            assert_eq!(root_from(mode, &var).unwrap(), PathBuf::from(abs));
        }
        let relative = |name: &str| (name == STATE_DIR_VAR).then(|| OsString::from("rel"));
        assert!(root_from(OperatingMode::User, &relative).is_err());
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn a_release_build_ignores_the_override() {
        let var = env(&[
            (STATE_DIR_VAR, "/t/state"),
            ("XDG_STATE_HOME", "/x/state"),
            ("LOCALAPPDATA", r"C:\a"),
        ]);
        let root = root_from(OperatingMode::User, &var).unwrap();
        assert!(!root.starts_with("/t/state"), "{root:?}");
    }

    #[test]
    fn a_sub_directory_is_created_private_and_found_again() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let dir = prepare_subdir(&root, true, "identity").unwrap();
        assert_eq!(dir, root.join("identity"));
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(prepare_subdir(&root, true, "identity").unwrap(), dir);
    }

    /// An installation-made root is never created by the agent.
    #[test]
    fn a_missing_installation_root_is_an_error_and_is_not_created() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("absent");
        assert!(prepare_subdir(&root, false, "identity").is_err());
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        assert!(prepare_subdir(&root, false, "identity").is_ok());
    }

    #[test]
    fn a_sub_directory_that_is_a_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("identity"), b"x").unwrap();
        assert!(prepare_subdir(temp.path(), true, "identity").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_group_or_other_writable_sub_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("identity");
        std::fs::create_dir(&dir).unwrap();
        for mode in [0o770, 0o707] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                prepare_subdir(temp.path(), true, "identity").is_err(),
                "{mode:o}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(prepare_subdir(temp.path(), true, "identity").is_ok());
    }
}

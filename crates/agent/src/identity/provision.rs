//! `ngr-agent provision-key`: the administrator creates the Linux device-mode agent key (design
//! 6.4, ADR-498).
//!
//! The device key store ([`super::systemd_credential`]) only reads, so this command makes the
//! key once, outside the agent's account. The steps, in order, each stopping the command before
//! the next when it fails: the caller is root (a release build); the target directory is
//! private and root's; nothing is at the final name; `systemd-creds` is on `PATH`; a P-256 key
//! is generated, as `load_or_create` does, and lives only in memory; `systemd-creds encrypt`
//! seals it (the plaintext goes down a pipe to its standard input, never into an argument, the
//! environment or a file); `systemd-creds decrypt` opens the ciphertext and the result must be
//! the generated key; the ciphertext is linked into place with `journal::create_linked`, which
//! never replaces. Nothing is written until the key has been verified.
//!
//! Exit statuses ([`ProvisionError::exit_code`]): 1 it cannot be done as asked, 3 an I/O or tool
//! failure with nothing in place, 5 the credential already exists.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shared_crypto::{PublicKey, Signer, SigningKey, Zeroizing};

use super::{DEVICE_CREDENTIAL_NAME, DEVICE_CREDSTORE_DIR, WithKey};
use crate::journal::{CreateLinkedError, create_linked};

/// The tool that seals and opens the credential; systemd 250 or later has the options used here.
pub const TOOL: &str = "systemd-creds";

/// How long one run of the tool may take before it is killed.
pub const TOOL_BOUND: Duration = Duration::from_secs(60);

/// The longest answer read from the tool: a sealed key is well under 1 KiB.
pub const MAX_ANSWER: usize = 64 * 1024;

/// The debug-only variable that moves the target directory and lifts the root checks, so a test
/// provisions into a temporary directory as an ordinary user.
#[cfg(debug_assertions)]
pub const CREDSTORE_DIR_VAR: &str = "NGR_AGENT_CREDSTORE_DIR";

/// Why no key was provisioned. No message holds key material.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// It cannot be done as asked (not root, a refused directory, no tool, the tool refused).
    #[error("{0}")]
    Cannot(String),
    /// An I/O or tool failure; nothing is in place.
    #[error("{0}")]
    Io(String),
    /// The credential is already there; it is never replaced.
    #[error(
        "the agent key credential already exists and is never replaced; to reset the key, remove it as the administrator and provision again"
    )]
    Exists,
}

impl ProvisionError {
    /// The process exit status the command ends with.
    pub fn exit_code(&self) -> u8 {
        match self {
            ProvisionError::Cannot(_) => 1,
            ProvisionError::Io(_) => 3,
            ProvisionError::Exists => 5,
        }
    }
}

fn cannot(text: impl Into<String>) -> ProvisionError {
    ProvisionError::Cannot(text.into())
}

fn io_error(what: &str, error: &io::Error) -> ProvisionError {
    ProvisionError::Io(format!("{what}: {:?}", error.kind()))
}

/// Where and how to provision; [`provision`] fills it in from the environment.
pub struct Plan<'a> {
    /// The target directory; its last component is created when missing.
    pub dir: &'a Path,
    /// The `PATH` value the tool is looked up in (absolute entries only).
    pub search_path: &'a OsStr,
    pub with_key: WithKey,
    /// How long one run of the tool may take.
    pub bound: Duration,
    /// Whether the target directory must be owned by root.
    pub root_owned: bool,
}

/// Provisions the device key into `/etc/credstore.encrypted` as root (a release build). A debug
/// build with `NGR_AGENT_CREDSTORE_DIR` set (an absolute directory) uses that directory and does
/// not require root.
pub fn provision(with_key: WithKey) -> Result<PublicKey, ProvisionError> {
    let moved = moved_dir()?;
    check_root(moved.is_some())?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    provision_with(&Plan {
        dir: moved.as_deref().unwrap_or(Path::new(DEVICE_CREDSTORE_DIR)),
        search_path: &path,
        with_key,
        bound: TOOL_BOUND,
        root_owned: moved.is_none(),
    })
}

/// The directory a debug build's `NGR_AGENT_CREDSTORE_DIR` names; always `None` in a release
/// build, which reads no variable.
fn moved_dir() -> Result<Option<PathBuf>, ProvisionError> {
    #[cfg(debug_assertions)]
    match std::env::var_os(CREDSTORE_DIR_VAR) {
        None => Ok(None),
        Some(dir) if Path::new(&dir).is_absolute() => Ok(Some(PathBuf::from(dir))),
        Some(_) => Err(cannot(
            "NGR_AGENT_CREDSTORE_DIR must be an absolute directory",
        )),
    }
    #[cfg(not(debug_assertions))]
    Ok(None)
}

fn check_root(moved: bool) -> Result<(), ProvisionError> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !moved && unsafe { libc::geteuid() } != 0 {
        return Err(cannot("provision-key must run as root"));
    }
    Ok(())
}

/// The first check of the command: the caller is root, unless a debug build moved the target
/// directory. [`provision`] repeats it.
pub fn require_root() -> Result<(), ProvisionError> {
    check_root(moved_dir()?.is_some())
}

/// The steps of the module's documentation, for `plan`.
pub fn provision_with(plan: &Plan<'_>) -> Result<PublicKey, ProvisionError> {
    prepare_dir(plan.dir, plan.root_owned)?;
    let final_path = plan.dir.join(DEVICE_CREDENTIAL_NAME);
    if taken(&final_path)? {
        return Err(ProvisionError::Exists);
    }
    let tool = find_tool(plan.search_path).ok_or_else(|| {
        cannot("systemd-creds was not found on PATH; provisioning needs systemd 250 or later")
    })?;

    let generated = SigningKey::generate().map_err(|_| cannot("no random bytes for the key"))?;
    let public = generated.public_key();
    let plaintext = generated.to_pkcs8_der();
    drop(generated);

    let name_arg = format!("--name={DEVICE_CREDENTIAL_NAME}");
    let sealed = run_tool(
        &tool,
        &[
            "encrypt".to_owned(),
            name_arg.clone(),
            format!("--with-key={}", plan.with_key.as_str()),
            "--tpm2-pcrs=".to_owned(),
            "-".to_owned(),
            "-".to_owned(),
        ],
        &plaintext,
        plan.bound,
    )?;

    // The ciphertext must open to the key just generated before anything is written.
    let opened = run_tool(
        &tool,
        &[
            "decrypt".to_owned(),
            name_arg,
            "-".to_owned(),
            "-".to_owned(),
        ],
        &sealed,
        plan.bound,
    )
    .map_err(|error| ProvisionError::Io(format!("verification failed: {error}")))?;
    let matches = SigningKey::from_pkcs8_der(&opened).is_ok_and(|key| key.public_key() == public);
    if !matches {
        return Err(ProvisionError::Io(
            "verification failed: the sealed credential does not open to the generated key"
                .to_owned(),
        ));
    }

    // Two provisioners can reach this point at once; the directory lock makes the second see
    // the first's file instead of racing over the temporary name.
    let _lock =
        lock_dir(plan.dir).map_err(|error| io_error("cannot lock the directory", &error))?;
    if taken(&final_path)? {
        return Err(ProvisionError::Exists);
    }
    let tmp = plan.dir.join(format!("{DEVICE_CREDENTIAL_NAME}.tmp"));
    let mut options = OpenOptions::new();
    options.mode(0o600);
    match create_linked(plan.dir, &final_path, &tmp, &sealed, options) {
        Ok(_) => Ok(public),
        Err(CreateLinkedError::AlreadyExists) => Err(ProvisionError::Exists),
        Err(CreateLinkedError::Io(error)) => Err(io_error("cannot write the credential", &error)),
    }
}

/// Whether anything, a dangling symlink included, is at `path`.
fn taken(path: &Path) -> Result<bool, ProvisionError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("cannot look at the credential's name", &error)),
    }
}

/// Makes the directory when it is missing (0700, its last component only) and refuses one that
/// is a symlink or not a directory, that its group or others may write, or, when `root_owned`,
/// that another account owns.
fn prepare_dir(dir: &Path, root_owned: bool) -> Result<(), ProvisionError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(io_error("cannot create the credential directory", &error));
                }
            }
            fs::symlink_metadata(dir)
                .map_err(|error| io_error("cannot look at the credential directory", &error))?
        }
        Err(error) => return Err(io_error("cannot look at the credential directory", &error)),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(cannot(
            "the credential directory is a symlink or not a directory",
        ));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(cannot(
            "the credential directory is writable by its group or others",
        ));
    }
    if root_owned && meta.uid() != 0 {
        return Err(cannot("the credential directory is not owned by root"));
    }
    Ok(())
}

/// The first executable regular file named [`TOOL`] in the absolute entries of `search_path`.
fn find_tool(search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .filter(|entry| entry.is_absolute())
        .map(|entry| entry.join(TOOL))
        .find(|candidate| {
            fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// An exclusive advisory lock on the directory, held until the returned handle drops.
fn lock_dir(dir: &Path) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let handle = File::open(dir)?;
    loop {
        // SAFETY: the descriptor is open for the life of `handle`.
        if unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(handle);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Runs `tool` with `args`, feeds it `input` on standard input and returns its standard output.
/// Its standard error is the caller's. The tool is killed (with its process group) when it runs
/// past `bound`. A non-zero status is [`ProvisionError::Cannot`]; a timeout, an I/O failure and an
/// empty or oversize answer are [`ProvisionError::Io`].
fn run_tool(
    tool: &Path,
    args: &[String],
    input: &[u8],
    bound: Duration,
) -> Result<Zeroizing<Vec<u8>>, ProvisionError> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .map_err(|error| io_error("cannot start systemd-creds", &error))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(ProvisionError::Io("systemd-creds has no pipes".to_owned()));
    };
    let deadline = Instant::now() + bound;
    let group = child.id() as libc::pid_t;

    let (status, answer) = std::thread::scope(|scope| {
        // A tool that exits before it has read everything makes the write fail: its status
        // says why, so the write's own failure is not reported.
        let writer = scope.spawn(move || {
            let mut stdin = stdin;
            let _ = stdin.write_all(input);
        });
        let reader = scope.spawn(move || {
            let mut buffer = Zeroizing::new(Vec::with_capacity(MAX_ANSWER + 1));
            stdout
                .take(MAX_ANSWER as u64 + 1)
                .read_to_end(&mut buffer)
                .map(|_| buffer)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() >= deadline => {
                    // SAFETY: a negative pid names the child's process group, made by
                    // `process_group(0)` above.
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(ProvisionError::Io(format!(
                        "systemd-creds did not finish within {} s and was killed",
                        bound.as_secs()
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => break Err(io_error("cannot wait for systemd-creds", &error)),
            }
        };
        let _ = writer.join();
        let answer = reader.join();
        (status, answer)
    });

    let status = status?;
    if !status.success() {
        return Err(cannot(match status.code() {
            Some(code) => format!("systemd-creds failed with exit status {code}"),
            None => "systemd-creds was ended by a signal".to_owned(),
        }));
    }
    let answer = answer
        .map_err(|_| ProvisionError::Io("reading systemd-creds panicked".to_owned()))?
        .map_err(|error| io_error("cannot read the answer of systemd-creds", &error))?;
    if answer.len() > MAX_ANSWER {
        return Err(ProvisionError::Io(
            "the answer of systemd-creds is too long".to_owned(),
        ));
    }
    if answer.is_empty() {
        return Err(ProvisionError::Io(
            "systemd-creds answered with nothing".to_owned(),
        ));
    }
    Ok(answer)
}

#[cfg(test)]
fn search(dir: &Path) -> Option<PathBuf> {
    find_tool(dir.as_os_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_key_is_a_closed_list() {
        for (text, value) in [
            ("auto", WithKey::Auto),
            ("host", WithKey::Host),
            ("tpm2", WithKey::Tpm2),
            ("host+tpm2", WithKey::HostTpm2),
        ] {
            assert_eq!(text.parse::<WithKey>(), Ok(value));
            assert_eq!(value.as_str(), text);
        }
        for bad in ["", "Auto", "tpm2-absent", "host+", "null", "auto "] {
            assert!(bad.parse::<WithKey>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_tool_is_an_executable_file_in_an_absolute_entry() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(search(temp.path()), None);
        let tool = temp.path().join(TOOL);
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(search(temp.path()), None);
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(search(temp.path()), Some(tool));
        assert_eq!(find_tool(OsStr::new("relative")), None);
    }

    #[test]
    fn exit_codes_follow_the_documented_classes() {
        assert_eq!(cannot("x").exit_code(), 1);
        assert_eq!(ProvisionError::Io("x".into()).exit_code(), 3);
        assert_eq!(ProvisionError::Exists.exit_code(), 5);
    }
}

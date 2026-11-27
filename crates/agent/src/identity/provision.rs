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

use std::ffi::{OsStr, OsString};
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
#[derive(Debug)]
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

#[expect(
    unsafe_code,
    reason = "libc::geteuid reads the effective user id; std has no equivalent (ADR-502)"
)]
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
    let tmp = plan.dir.join(format!("{DEVICE_CREDENTIAL_NAME}.tmp"));
    if taken(&final_path)? {
        remove_stale_temporary(plan.dir, &tmp);
        return Err(ProvisionError::Exists);
    }
    let tool = find_tool(plan.search_path, plan.root_owned).ok_or_else(|| {
        cannot(if plan.root_owned {
            "no trusted systemd-creds was found on PATH (a root-owned file in root-owned directories \
             that others cannot write); provisioning needs systemd 250 or later"
        } else {
            "systemd-creds was not found on PATH; provisioning needs systemd 250 or later"
        })
    })?;

    let generated = SigningKey::generate()
        .map_err(|error| cannot(format!("no random bytes for the key: {error}")))?;
    let public = generated.public_key();
    let plaintext = generated.to_pkcs8_der();
    drop(generated);

    let name_arg = format!("--name={DEVICE_CREDENTIAL_NAME}");
    let env = tool_environment(plan.search_path);
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
        &env,
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
        &env,
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
        let _ = fs::remove_file(&tmp);
        return Err(ProvisionError::Exists);
    }
    let mut options = OpenOptions::new();
    options.mode(0o600);
    match create_linked(plan.dir, &final_path, &tmp, &sealed, options) {
        Ok(_) => Ok(public),
        Err(CreateLinkedError::AlreadyExists) => Err(ProvisionError::Exists),
        Err(CreateLinkedError::Io(error)) => {
            // The link may have been made before the failure (a sync, a reopen): say which.
            match taken(&final_path) {
                Ok(true) => Err(ProvisionError::Io(format!(
                    "the credential is in place but was not synced ({:?}); check it and restart the unit",
                    error.kind()
                ))),
                Ok(false) => Err(io_error(
                    "cannot write the credential; nothing is in place",
                    &error,
                )),
                Err(_) => Err(io_error(
                    "cannot write the credential, and whether it is in place is unknown; check it",
                    &error,
                )),
            }
        }
    }
}

/// Removes a temporary file an earlier attempt left, under the directory lock, so that it is not
/// taken from a run that is writing it. Best effort: the answer is "exists" either way.
fn remove_stale_temporary(dir: &Path, tmp: &Path) {
    if let Ok(_lock) = lock_dir(dir) {
        let _ = fs::remove_file(tmp);
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
/// When `trusted_only`, the file must pass [`trusted_path`] and the path returned is the
/// resolved one that was checked: a link in a directory another account can write is never
/// executed, since it could be pointed elsewhere after the check.
fn find_tool(search_path: &OsStr, trusted_only: bool) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .filter(|entry| entry.is_absolute())
        .map(|entry| entry.join(TOOL))
        .find_map(|candidate| {
            let path = if trusted_only {
                trusted_path(&candidate)?
            } else {
                candidate
            };
            fs::metadata(&path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .then_some(path)
        })
}

/// Whether the file at `path` and every directory above it (after resolving links) are owned by
/// root and not writable by their group or others: a program run as root must not be one that
/// another account can replace.
pub fn trusted(path: &Path) -> bool {
    trusted_path(path).is_some()
}

/// The resolved path of `path` when [`trusted`] holds for it. The caller executes this path, not
/// the one it passed.
fn trusted_path(path: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(path).ok()?;
    real.ancestors()
        .all(|part| {
            fs::symlink_metadata(part).is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o022 == 0)
        })
        .then_some(real)
}

/// An exclusive advisory lock on the directory, held until the returned handle drops.
#[expect(
    unsafe_code,
    reason = "libc FFI with no safe std equivalent: `flock` takes an advisory file lock (ADR-502)"
)]
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

/// The environment the tool gets: `path` as `PATH`, and the caller's `LANG` and `LC_*`; nothing
/// else (ADR-498 item 3).
fn tool_environment(path: &OsStr) -> Vec<(OsString, OsString)> {
    let mut vars = vec![(OsString::from("PATH"), path.to_owned())];
    vars.extend(std::env::vars_os().filter(|(name, _)| {
        name.to_str()
            .is_some_and(|n| n == "LANG" || n.starts_with("LC_"))
    }));
    vars
}

/// Runs `tool` with `args` and `env` only, feeds it `input` on standard input and returns its
/// standard output. Its standard error is the caller's. The whole run, the read of standard output
/// included, is bounded by `bound`: past it the tool's process group is killed and the run is
/// [`ProvisionError::Io`]. The group is killed again once the tool has exited, so a descendant
/// that kept standard output open does not hold the command; one that left the group (`setsid`)
/// is caught by the bound. The size cap is checked before the status: a tool cut off at the cap
/// dies of a broken pipe, which is still "too long". A non-zero status is
/// [`ProvisionError::Cannot`]; a timeout, an I/O failure and an empty or oversize answer are
/// [`ProvisionError::Io`].
#[expect(
    unsafe_code,
    reason = "child setup between fork and exec and polling use libc calls (_exit, getpid, getppid, prctl, poll) that std does not offer (ADR-502)"
)]
fn run_tool(
    tool: &Path,
    args: &[String],
    env: &[(OsString, OsString)],
    input: &[u8],
    bound: Duration,
) -> Result<Zeroizing<Vec<u8>>, ProvisionError> {
    use std::os::fd::AsRawFd;
    let mut command = Command::new(tool);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Its own group, so the kill reaches what it started.
        .process_group(0);
    // SAFETY: getpid cannot fail.
    let parent = unsafe { libc::getpid() };
    // SAFETY: the closure calls only prctl, getppid and _exit, which are async-signal-safe. The
    // tool dies with the provisioner (an interrupt, a termination), which it would otherwise
    // outlive; the parent is checked after the request, since one that died before it is never
    // signalled.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(127);
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| io_error("cannot start systemd-creds", &error))?;
    let (Some(stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ProvisionError::Io("systemd-creds has no pipes".to_owned()));
    };
    let deadline = Instant::now() + bound;
    let pid = child.id() as libc::pid_t;
    // Whether the leader has been reaped. Its pid, and so its group, is reserved only until then:
    // the group is signalled before the reap and never after it.
    let mut reaped = false;
    #[expect(
        unsafe_code,
        reason = "libc FFI with no safe std equivalent: `kill` sends a signal (ADR-502)"
    )]
    fn kill_group(pid: libc::pid_t, reaped: bool) {
        if !reaped {
            // SAFETY: a negative pid names the child's process group, made by `process_group(0)`,
            // whose leader has not been reaped, so the number is still ours.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
    // Ends the tool and reaps it.
    fn terminate(child: &mut std::process::Child, pid: libc::pid_t, reaped: &mut bool) {
        kill_group(pid, *reaped);
        if !*reaped {
            let _ = child.kill();
            let _ = child.wait();
            *reaped = true;
        }
    }
    // Whether the leader has exited and is still waiting to be reaped (`WNOWAIT` leaves it a
    // zombie, so its pid stays reserved while the group is signalled).
    #[expect(
        unsafe_code,
        reason = "libc FFI with no safe std equivalent: `waitid` waits for a child without reaping it (ADR-502)"
    )]
    fn exited(pid: libc::pid_t) -> io::Result<bool> {
        // SAFETY: an all-zero siginfo is a valid value for the call to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid for writes; the id is a child of this process.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so the union holds the child fields.
        Ok(unsafe { info.si_pid() } != 0)
    }

    let outcome = std::thread::scope(|scope| {
        // The input is the key (about 140 bytes) or the sealed credential (at most the 64 KiB
        // answer cap), so the write rarely fills the pipe; a thread keeps a tool that does not
        // read it from stalling the loop below. It ends when the tool exits, the write then
        // failing, whose cause the status gives. Only a descendant that holds the read end after
        // the tool is gone could keep it, and the sizes above fit the pipe.
        scope.spawn(move || {
            let mut stdin = stdin;
            let _ = stdin.write_all(input);
        });
        let mut buffer = Zeroizing::new(vec![0u8; MAX_ANSWER + 1]);
        let mut filled = 0;
        let (mut eof, mut oversize) = (false, false);
        let mut status = None;
        loop {
            if !reaped {
                match exited(pid) {
                    Ok(true) => {
                        // The group goes while the leader is a zombie, then the reap.
                        kill_group(pid, reaped);
                        status = child.wait().ok();
                        reaped = true;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        terminate(&mut child, pid, &mut reaped);
                        return Err(io_error("cannot wait for systemd-creds", &error));
                    }
                }
            }
            if (eof || oversize) && reaped {
                break;
            }
            if oversize {
                terminate(&mut child, pid, &mut reaped);
                continue;
            }
            if Instant::now() >= deadline {
                // A descendant that left the group is not reached: closing the pipe (the
                // return) and the exit status say what happened.
                terminate(&mut child, pid, &mut reaped);
                return Err(ProvisionError::Io(format!(
                    "systemd-creds did not finish within {} s and was killed (timeout)",
                    bound.as_secs()
                )));
            }
            if eof {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut poll = libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; the bounded timeout keeps the loop checking the clock.
            let ready = unsafe { libc::poll(&mut poll, 1, 10) };
            if ready > 0 {
                match stdout.read(&mut buffer[filled..]) {
                    Ok(0) => eof = true,
                    Ok(n) => {
                        filled += n;
                        oversize = filled > MAX_ANSWER;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        terminate(&mut child, pid, &mut reaped);
                        return Err(io_error("cannot read the answer of systemd-creds", &error));
                    }
                }
            }
        }
        Ok((status, oversize, filled, buffer))
    });

    let (status, oversize, filled, mut answer) = outcome?;
    if oversize {
        return Err(ProvisionError::Io(
            "the answer of systemd-creds is too long".to_owned(),
        ));
    }
    if status.is_some_and(|status| !status.success()) {
        return Err(cannot(match status.and_then(|status| status.code()) {
            Some(code) => format!("systemd-creds failed with exit status {code}"),
            None => "systemd-creds was ended by a signal".to_owned(),
        }));
    }
    if filled == 0 {
        return Err(ProvisionError::Io(
            "systemd-creds answered with nothing".to_owned(),
        ));
    }
    answer.truncate(filled);
    Ok(answer)
}

#[cfg(test)]
fn search(dir: &Path) -> Option<PathBuf> {
    find_tool(dir.as_os_str(), false)
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
        assert_eq!(find_tool(OsStr::new("relative"), false), None);
    }

    /// The trust rule of a release build: a file or a directory that others may write, or that a
    /// non-root account owns, is not trusted (the temporary directory is under world-writable
    /// `/tmp`, whoever the tests run as); the system shell is.
    #[test]
    fn a_tool_is_trusted_only_under_root_owned_unwritable_directories() {
        let temp = tempfile::tempdir().unwrap();
        let tool = temp.path().join(TOOL);
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!trusted(&tool));
        assert!(!trusted(&temp.path().join("missing")));
        assert!(trusted(Path::new("/bin/sh")));
    }

    /// A link in a directory the caller owns is resolved before it is trusted, and the resolved
    /// path is the one returned (so the one executed); a link to an untrusted file is refused.
    #[test]
    fn a_trusted_tool_is_found_by_its_resolved_path() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join(TOOL);
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        assert_eq!(
            find_tool(temp.path().as_os_str(), true),
            Some(fs::canonicalize("/bin/sh").unwrap())
        );
        // The plain lookup keeps the path it was given.
        assert_eq!(
            find_tool(temp.path().as_os_str(), false),
            Some(link.clone())
        );

        let other = tempfile::tempdir().unwrap();
        let target = other.path().join("real");
        fs::write(&target, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(find_tool(temp.path().as_os_str(), true), None);
    }

    #[test]
    fn exit_codes_follow_the_documented_classes() {
        assert_eq!(cannot("x").exit_code(), 1);
        assert_eq!(ProvisionError::Io("x".into()).exit_code(), 3);
        assert_eq!(ProvisionError::Exists.exit_code(), 5);
    }
}

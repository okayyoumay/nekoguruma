//! The Windows device-mode key as a service account (design 6.4, ADR-459 Decision item 4,
//! ADR-507): `ngr-agent identity --mode device` run by the service control manager as the
//! virtual account `NT SERVICE\<service name>`, which the device-mode service runs as.
//!
//! The test runs only when `NGR_TEST_WINDOWS_SERVICE_ACCOUNT=1`, and then a failure to create the
//! service or to start it fails the test rather than skipping it. It needs an elevated account
//! (it creates and deletes a service and grants the service account a directory), which the
//! `core-windows` job's step for it has. It
//!
//! 1. creates a demand-start service under `NT SERVICE\<name>` whose command is `cmd /c` running
//!    a batch file that records `set`, `whoami /user` and the debug `ngr-agent identity --mode
//!    device` (with `NGR_AGENT_KEY_STORE=dpapi` and no state override, so the key lands in the
//!    service account's own profile) into a directory the account may write;
//! 2. starts the service twice and checks the account from the recorded environment
//!    (`USERDOMAIN` is `NT SERVICE`, `USERNAME` the service name, `USERPROFILE` its service
//!    profile); the key is created by the first run (exit 0, a key file with the `NGRDPAPI`
//!    magic) and found by the second (exit 0, the file byte for byte and in modification time
//!    unchanged), and the identity directory holds nothing but the key file and its lock;
//! 3. as the account running the test, copies the service's key file and reads it through the
//!    store, which must refuse it (another account's blob);
//! 4. prints the facts it recorded, always, and deletes the service and the directory.
//!
//! A service has no console, and on the CI image the output of every external program it runs
//! (`whoami`, the agent) is lost: the redirected files stay empty although the programs exit 0.
//! Only the `cmd` builtins (`set`, `echo`) write. The assertions therefore rest on the
//! environment, exit codes and the files the agent leaves, never on a program's output; the
//! output is still printed in case a machine does deliver it.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use agent::identity::dpapi;
use agent::identity::store::{SecretName, SecretStore, StoreErrorKind};
use worker_host::service::OperatingMode;

#[path = "support/installation_file.rs"]
mod installation_file;

const GATE: &str = "NGR_TEST_WINDOWS_SERVICE_ACCOUNT";

/// How long one run of the service may take to leave its marker file.
const RUN_WAIT: Duration = Duration::from_secs(90);

/// The files a run writes, which the batch file creates in the working directory.
const OUTPUTS: [&str; 8] = [
    "env.txt",
    "version.txt",
    "whoami.txt",
    "whoami_exit.txt",
    "stdout.txt",
    "stderr.txt",
    "exit.txt",
    "done.txt",
];

fn enabled() -> bool {
    std::env::var_os(GATE).is_some_and(|value| value == "1")
}

/// Runs `program` and returns its exit code and output text, with a spawn failure as a panic.
fn run(program: &str, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{program} cannot run: {error}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), text)
}

/// Deletes the service and the directory when dropped, also after a failed assertion.
struct Cleanup {
    service: String,
    dir: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("sc.exe")
            .args(["stop", &self.service])
            .output();
        let _ = Command::new("sc.exe")
            .args(["delete", &self.service])
            .output();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What one run of the service left, as text.
struct RunFacts {
    env: String,
    version: String,
    whoami: String,
    whoami_exit: String,
    stdout: String,
    stderr: String,
    exit: String,
}

impl RunFacts {
    fn print(&self, label: &str) {
        println!(
            "--- {label}: agent without arguments\n{}",
            self.version.trim()
        );
        println!(
            "--- {label}: whoami /user (exit {})\n{}",
            self.whoami_exit.trim(),
            self.whoami.trim()
        );
        println!(
            "--- {label}: exit {}\n--- stdout\n{}\n--- stderr\n{}",
            self.exit.trim(),
            self.stdout.trim(),
            self.stderr.trim()
        );
    }

    fn env_var(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        self.env
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .map(|value| value.trim().to_owned())
    }
}

/// Reads one output file as lossy text (the console tools write in the OEM code page), printing
/// its byte length, or the error and what the directory says about the file when it cannot be read.
fn read_lossy(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    match fs::read(&path) {
        Ok(bytes) => {
            println!("--- {name}: {} bytes", bytes.len());
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Err(error) => {
            println!(
                "--- {name}: cannot be read: {error}; metadata: {:?}",
                fs::metadata(&path).map(|meta| (meta.len(), meta.is_file()))
            );
            String::new()
        }
    }
}

/// Prints the names and sizes under `dir`, recursively, and the tail of each file under a `logs`
/// directory; a directory that cannot be read prints the error.
fn print_tree(dir: &Path, in_logs: bool) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            println!("--- {}: cannot be listed: {error}", dir.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => {
                println!("    {} (dir)", path.display());
                let logs = in_logs || entry.file_name() == "logs";
                print_tree(&path, logs);
            }
            Ok(meta) => {
                println!("    {} ({} bytes)", path.display(), meta.len());
                if in_logs {
                    match fs::read(&path) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes);
                            let lines: Vec<&str> = text.lines().collect();
                            let tail = &lines[lines.len().saturating_sub(60)..];
                            println!("--- tail of {}\n{}", path.display(), tail.join("\n"));
                        }
                        Err(error) => println!("--- {}: cannot be read: {error}", path.display()),
                    }
                }
            }
            Err(error) => println!("    {}: no metadata: {error}", path.display()),
        }
    }
}

/// Prints the agent's directory under the service account's profile, from its `LOCALAPPDATA`.
fn print_service_state(facts: &RunFacts, label: &str) {
    match facts.env_var("LOCALAPPDATA") {
        Some(local) => {
            let root = Path::new(&local).join("nekoguruma");
            println!("--- {label}: {}", root.display());
            print_tree(&root, false);
        }
        None => println!("--- {label}: the service has no LOCALAPPDATA"),
    }
}

/// Starts the service and collects what its command wrote, removing the files for the next run.
fn start_and_collect(service: &str, dir: &Path) -> RunFacts {
    for name in OUTPUTS {
        let _ = fs::remove_file(dir.join(name));
    }
    let (_, started) = run("sc.exe", &["start", service]);
    println!("sc start: {}", started.trim());
    let deadline = Instant::now() + RUN_WAIT;
    while !dir.join("done.txt").exists() {
        assert!(
            Instant::now() < deadline,
            "the service's command did not finish within {RUN_WAIT:?}: {started}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let read = |name: &str| read_lossy(dir, name);
    let facts = RunFacts {
        env: read("env.txt"),
        version: read("version.txt"),
        whoami: read("whoami.txt"),
        whoami_exit: read("whoami_exit.txt"),
        stdout: read("stdout.txt"),
        stderr: read("stderr.txt"),
        exit: read("exit.txt"),
    };
    // Wait for the service to be stopped before the next start.
    for _ in 0..50 {
        let (_, state) = run("sc.exe", &["query", service]);
        if state.contains("STOPPED") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    facts
}

/// The key file and its directory as a run left them.
struct BlobSnapshot {
    bytes: Result<Vec<u8>, String>,
    modified: Result<std::time::SystemTime, String>,
    /// The names of all entries in the identity directory.
    entries: Vec<String>,
}

fn snapshot(identity: &Path, file: &str) -> BlobSnapshot {
    let path = identity.join(file);
    let mut entries: Vec<String> = fs::read_dir(identity)
        .map(|dir| {
            dir.flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    BlobSnapshot {
        bytes: fs::read(&path).map_err(|error| error.to_string()),
        modified: fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .map_err(|error| error.to_string()),
        entries,
    }
}

#[test]
fn the_device_key_is_created_once_and_found_again_as_a_service_account() {
    if !enabled() {
        return;
    }
    let id = std::process::id();
    let service = format!("ngr-keystore-probe-{id}");
    let account = format!("NT SERVICE\\{service}");
    // A directory with no space in its path, so that the service command line needs no quoting.
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
    let dir = PathBuf::from(format!("{drive}\\ngr-keystore-probe-{id}"));
    fs::create_dir_all(&dir).unwrap();
    let _cleanup = Cleanup {
        service: service.clone(),
        dir: dir.clone(),
    };
    fs::copy(env!("CARGO_BIN_EXE_ngr-agent"), dir.join("ngr-agent.exe")).unwrap();
    fs::write(
        dir.join("agent.toml"),
        installation_file::text("device", "http://127.0.0.1:1"),
    )
    .unwrap();
    fs::write(
        dir.join("run.cmd"),
        "@echo off\r\n\
         set NGR_AGENT_INSTALLATION_FILE=%~dp0agent.toml\r\n\
         set NGR_AGENT_KEY_STORE=dpapi\r\n\
         set NGR_AGENT_STATE_DIR=\r\n\
         set > \"%~dp0env.txt\"\r\n\
         \"%~dp0ngr-agent.exe\" > \"%~dp0version.txt\" 2>&1\r\n\
         whoami /user > \"%~dp0whoami.txt\" 2>&1\r\n\
         echo %errorlevel% > \"%~dp0whoami_exit.txt\"\r\n\
         \"%~dp0ngr-agent.exe\" identity --mode device > \"%~dp0stdout.txt\" 2> \"%~dp0stderr.txt\"\r\n\
         echo %errorlevel% > \"%~dp0exit.txt\"\r\n\
         echo done > \"%~dp0done.txt\"\r\n",
    )
    .unwrap();

    let command = format!("cmd.exe /c {}\\run.cmd", dir.display());
    let (code, text) = run(
        "sc.exe",
        &[
            "create", &service, "binPath=", &command, "start=", "demand", "obj=", &account,
        ],
    );
    assert_eq!(code, Some(0), "sc create: {text}");
    // The debug agent checks its installation file (design 7.2): only SYSTEM, Administrators,
    // TrustedInstaller and the service account itself may own or write it. The directory inherits
    // the drive root's ACL, which gives other accounts write access, so replace it instead of
    // adding to it. The test runs elevated, so it keeps full control as an administrator.
    let dir_text = dir.to_string_lossy().into_owned();
    let service_grant = format!("{account}:(OI)(CI)F");
    let (code, text) = run(
        "icacls.exe",
        &[
            &dir_text,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            &service_grant,
        ],
    );
    assert_eq!(code, Some(0), "icacls grant: {text}");
    let (code, text) = run(
        "icacls.exe",
        &[&dir_text, "/setowner", "*S-1-5-32-544", "/T"],
    );
    assert_eq!(code, Some(0), "icacls setowner: {text}");
    for target in [
        dir_text.clone(),
        dir.join("agent.toml").to_string_lossy().into_owned(),
    ] {
        let (_, acl) = run("icacls.exe", &[&target]);
        println!("--- icacls {target}\n{}", acl.trim());
    }

    let name = SecretName::agent_key(OperatingMode::Device);
    let file = format!("{name}.dpapi");
    let first = start_and_collect(&service, &dir);
    first.print("first run");
    print_service_state(&first, "after the first run");
    println!(
        "--- the service's environment\n{}",
        first.env.trim().replace("\r\n", "\n")
    );
    let local = first
        .env_var("LOCALAPPDATA")
        .expect("the service has no LOCALAPPDATA: see the environment above");
    let identity_dir = Path::new(&local).join("nekoguruma").join("identity");
    let after_first = snapshot(&identity_dir, &file);
    let second = start_and_collect(&service, &dir);
    second.print("second run");
    print_service_state(&second, "after the second run");
    let after_second = snapshot(&identity_dir, &file);
    println!(
        "--- identity directory: after the first run {:?}, after the second {:?}",
        after_first.entries, after_second.entries
    );

    // The service account's blob, read by the account running the test.
    let theirs = identity_dir.join(&file);
    let mine = tempfile::tempdir().unwrap();
    let identity = mine.path().join("state").join("identity");
    fs::create_dir_all(&identity).unwrap();
    let copied = fs::copy(&theirs, identity.join(&file));
    println!("--- copy of {}: {copied:?}", theirs.display());
    let read = copied.map(|_| dpapi::open(mine.path().join("state")).get(&name));

    // The account, from the environment the builtin `set` recorded (see the module docs).
    assert_eq!(
        first.env_var("USERDOMAIN").as_deref(),
        Some("NT SERVICE"),
        "the command did not run as {account}: see the environment above"
    );
    let username = first.env_var("USERNAME").unwrap_or_default();
    assert!(
        username.eq_ignore_ascii_case(&service),
        "USERNAME is {username:?}, expected {service:?}"
    );
    let profile = first.env_var("USERPROFILE").unwrap_or_default();
    let expected_profile = format!("\\ServiceProfiles\\{service}").to_lowercase();
    assert!(
        profile.to_lowercase().ends_with(&expected_profile),
        "USERPROFILE {profile:?} is not the profile of {service}"
    );
    assert!(
        local.to_lowercase().starts_with(&profile.to_lowercase()),
        "LOCALAPPDATA {local:?} is not under USERPROFILE {profile:?}"
    );

    // Created once: the first run leaves the key file.
    assert_eq!(first.exit.trim(), "0", "the first run failed");
    let created = after_first
        .bytes
        .as_ref()
        .unwrap_or_else(|error| panic!("the first run left no key file: {error}"));
    assert!(
        created.starts_with(b"NGRDPAPI"),
        "the key file does not start with the store's magic: {} bytes",
        created.len()
    );
    // Found again: the second run leaves the file as it was, so it did not write a new one.
    assert_eq!(second.exit.trim(), "0", "the second run failed");
    assert_eq!(
        after_second.bytes.as_ref().ok(),
        Some(created),
        "the second run changed or lost the key file"
    );
    assert_eq!(
        after_second.modified, after_first.modified,
        "the second run rewrote the key file"
    );
    // Nothing else is left in the directory: no other key and no stray temporary file.
    for entries in [&after_first.entries, &after_second.entries] {
        let others: Vec<&String> = entries
            .iter()
            .filter(|entry| **entry != file && !entry.ends_with(".lock"))
            .collect();
        assert!(others.is_empty(), "unexpected files in {entries:?}");
        assert!(
            entries.contains(&file),
            "the key file is missing: {entries:?}"
        );
    }
    match read.expect("the service account's key file cannot be copied") {
        Err(error) => assert_eq!(error.kind(), StoreErrorKind::Undecryptable, "{error}"),
        Ok(_) => panic!("another account read the service account's key"),
    }
}

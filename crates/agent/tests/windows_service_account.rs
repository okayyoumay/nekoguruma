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
//! 2. starts the service twice: the key is created by the first run and found by the second;
//! 3. as the account running the test, copies the service's key file and reads it through the
//!    store, which must refuse it (another account's blob);
//! 4. prints the facts it recorded, always, and deletes the service and the directory.

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
const OUTPUTS: [&str; 6] = [
    "env.txt",
    "whoami.txt",
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
    whoami: String,
    stdout: String,
    stderr: String,
    exit: String,
}

impl RunFacts {
    fn print(&self, label: &str) {
        println!("--- {label}: whoami /user\n{}", self.whoami.trim());
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
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let facts = RunFacts {
        env: read("env.txt"),
        whoami: read("whoami.txt"),
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

fn json(facts: &RunFacts) -> Option<serde_json::Value> {
    serde_json::from_str(facts.stdout.trim()).ok()
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
         whoami /user > \"%~dp0whoami.txt\" 2>&1\r\n\
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

    let first = start_and_collect(&service, &dir);
    first.print("first run");
    let second = start_and_collect(&service, &dir);
    second.print("second run");
    println!(
        "--- the service's environment\n{}",
        first.env.trim().replace("\r\n", "\n")
    );

    // The service account's blob, read by the account running the test.
    let local = first
        .env_var("LOCALAPPDATA")
        .expect("the service has no LOCALAPPDATA: see the environment above");
    let name = SecretName::agent_key(OperatingMode::Device);
    let file = format!("{name}.dpapi");
    let theirs = Path::new(&local)
        .join("nekoguruma")
        .join("identity")
        .join(&file);
    let mine = tempfile::tempdir().unwrap();
    let identity = mine.path().join("state").join("identity");
    fs::create_dir_all(&identity).unwrap();
    let copied = fs::copy(&theirs, identity.join(&file));
    println!("--- copy of {}: {copied:?}", theirs.display());
    let read = copied.map(|_| dpapi::open(mine.path().join("state")).get(&name));

    assert!(
        first
            .whoami
            .to_lowercase()
            .contains(&account.to_lowercase()),
        "the command did not run as {account}"
    );
    assert_eq!(first.exit.trim(), "0", "the first run failed");
    assert_eq!(second.exit.trim(), "0", "the second run failed");
    let (first_json, second_json) = (json(&first).unwrap(), json(&second).unwrap());
    assert_eq!(first_json["created"], true, "{first_json}");
    assert_eq!(second_json["created"], false, "{second_json}");
    assert_eq!(first_json["fingerprint"], second_json["fingerprint"]);
    match read.expect("the service account's key file cannot be copied") {
        Err(error) => assert_eq!(error.kind(), StoreErrorKind::Undecryptable, "{error}"),
        Ok(_) => panic!("another account read the service account's key"),
    }
}

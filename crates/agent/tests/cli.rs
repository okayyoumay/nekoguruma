//! `ngr-agent` failures that end the run before a worker starts. A run against a worker is
//! tested end to end in `crates/j2534-0404-service/tests/ngr_agent_end_to_end.rs`, which also
//! relies on this file: cargo builds the `ngr-agent` binary for a package's integration tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program};

fn ngr_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ngr-agent"))
}

/// A fresh temporary directory, removed however the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ngr-agent-cli-{name}-{nanos}"));
        std::fs::create_dir_all(&path).expect("temporary directory should be writable");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_program(dir: &Path, code: Vec<Op>) -> PathBuf {
    let program = Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    };
    let path = dir.join("program.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&program).expect("program should serialize"),
    )
    .expect("program file should be writable");
    path
}

// Only the debug-only tests read the VIN through a worker.
#[cfg(debug_assertions)]
fn read_vin(dir: &Path) -> PathBuf {
    write_program(
        dir,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
    )
}

/// Asserts a failure with exit status 1, nothing on stdout and `message` on stderr.
fn assert_fails_with(output: Output, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(message), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn usage_error_exits_with_2() {
    let output = ngr_agent().output().expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: ngr-agent run"));
    assert!(output.stdout.is_empty());
}

#[test]
fn a_29_bit_can_id_is_a_usage_error() {
    let output = ngr_agent()
        .args([
            "run",
            "--vci",
            "x",
            "--mode",
            "user",
            "--program",
            "p.json",
            "--tx-id",
            "18DA10F1",
        ])
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("11-bit"));
}

#[test]
fn unreadable_program_exits_with_1() {
    let temp = TempDir::new("missing");
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--program"])
        .arg(temp.0.join("missing.json"))
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "cannot read");
}

#[test]
fn refused_program_exits_with_1_before_resolving_the_vci() {
    let temp = TempDir::new("refused");
    // SecurityAccess is refused in every build: a standalone agent has no key source (ADR-259).
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::SecurityAccess { level: 1 }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--program"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

/// A release build refuses every write before a worker starts (ADR-235 item 8, ADR-247); a
/// debug build allows them on the simulator, so it cannot refuse them up front.
#[cfg(not(debug_assertions))]
#[test]
fn a_write_is_refused_before_resolving_the_vci_in_a_release_build() {
    let temp = TempDir::new("refused-write");
    // WriteDataByIdentifier is outside the read-only policy.
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x2E }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--mode", "user", "--program"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

// The tests below point the agent at a test registration-definition directory through the
// debug-only NGR_J2534_DEFINITION_DIR override (ADR-282), which replaces the Linux definition
// directory and the Windows registry alike.

/// A definition directory that names `library` as the VCI `test-vci`, or names nothing.
#[cfg(debug_assertions)]
fn write_definitions(dir: &Path, library: Option<&Path>) -> PathBuf {
    let definitions = dir.join("definitions");
    std::fs::create_dir_all(&definitions).expect("definition directory should be writable");
    if let Some(library) = library {
        let library = std::path::absolute(library).expect("library path should be absolute");
        let text = format!(
            "Name = \"test-vci\"\nFunctionLibrary = {:?}\n",
            library.display().to_string()
        );
        std::fs::write(definitions.join("test-vci.toml"), text)
            .expect("definition file should be writable");
    }
    definitions
}

#[cfg(debug_assertions)]
#[test]
fn unknown_vci_exits_with_1() {
    let temp = TempDir::new("unknown-vci");
    let definitions = write_definitions(&temp.0, None);
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

#[cfg(debug_assertions)]
#[test]
fn missing_worker_build_exits_with_1() {
    let temp = TempDir::new("no-worker");
    // Any library header does: the agent's own executable stands in for the VCI library.
    let definitions = write_definitions(&temp.0, Some(Path::new(env!("CARGO_BIN_EXE_ngr-agent"))));
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(temp.0.join("empty"))
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "no worker bundled for ABI");
}

/// Installs, for the agent's own ABI, a stand-in worker that records the `NGR_J2534_LONG_SIZE`
/// it was started with in `<workers>/long_size` and exits, and runs the agent against a VCI whose
/// definition carries `extra` (the agent's executable stands in for the library). Returns what
/// the stand-in recorded.
#[cfg(all(debug_assertions, unix))]
fn long_size_passed_to_the_worker(name: &str, extra: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new(name);
    let agent = Path::new(env!("CARGO_BIN_EXE_ngr-agent"));
    let definitions = write_definitions(&temp.0, Some(agent));
    let file = definitions.join("test-vci.toml");
    let text = std::fs::read_to_string(&file).expect("definition file should be readable");
    std::fs::write(&file, format!("{text}{extra}")).expect("definition file should be writable");
    let abi = worker_host::abi::detect_file(agent).expect("the agent's header should be readable");
    let workers = temp.0.join("workers");
    let dir = workers.join(abi.name());
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let record = workers.join("long_size");
    let worker = dir.join("j2534-0404-service");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\nprintf '%s' \"${{NGR_J2534_LONG_SIZE-unset}}\" > '{}'\nexit 1\n",
            record.display()
        ),
    )
    .expect("stand-in worker should be writable");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
        .expect("stand-in worker should be executable");
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&workers)
        .arg("--locks")
        .arg(temp.0.join("locks"))
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .output()
        .expect("ngr-agent should run");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    std::fs::read_to_string(&record)
        .unwrap_or_else(|error| panic!("the stand-in worker should have run ({error}): {output:?}"))
}

/// A definition's `LongSize` reaches the worker even where it differs from the ABI's default
/// (7.1.1, 7.1.2). The host's default is 8, so 4 is the non-default width.
#[cfg(all(debug_assertions, unix, target_pointer_width = "64"))]
#[test]
fn the_definition_long_size_reaches_the_worker() {
    assert_eq!(
        long_size_passed_to_the_worker("long-size-4", "LongSize = 4\n"),
        "4"
    );
}

/// Without a `LongSize`, the worker gets the ABI's default.
#[cfg(all(debug_assertions, unix, target_pointer_width = "64"))]
#[test]
fn without_long_size_the_worker_gets_the_abi_default() {
    assert_eq!(long_size_passed_to_the_worker("long-size-default", ""), "8");
}

/// `ngr-agent run` takes the per-VCI lock before it resolves the VCI, and waits while another
/// job holds it (ADR-257).
#[cfg(debug_assertions)]
#[test]
fn a_run_waits_for_the_vci_lock() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use agent::guards::{GuardSetup, JobGuards};

    let temp = TempDir::new("vci-lock");
    let definitions = write_definitions(&temp.0, None);
    let locks = temp.0.join("locks");
    let held = JobGuards::take_vci_only(
        &GuardSetup {
            dir: locks.clone(),
            vci: "test-vci".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .expect("the test takes the VCI lock");
    let mut child = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(&locks)
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ngr-agent should start");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("the run's status").is_none(),
        "the run waits for the VCI lock"
    );
    drop(held);
    // With the lock free, the run goes on and fails at the VCI, which no definition names.
    let output = child.wait_with_output().expect("ngr-agent should end");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

/// A writing `ngr-agent run` also takes the device's reprogramming slot before it resolves the
/// VCI, so it waits while a job on another VCI holds the slot (ADR-257).
#[cfg(debug_assertions)]
#[test]
fn a_writing_run_waits_for_the_reprogramming_slot() {
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use agent::guards::{GuardSetup, JobGuards};

    let temp = TempDir::new("slot");
    let definitions = write_definitions(&temp.0, None);
    let locks = temp.0.join("locks");
    let held = JobGuards::take(
        &GuardSetup {
            dir: locks.clone(),
            vci: "other-vci".to_owned(),
        },
        Duration::from_millis(1),
        &AtomicBool::new(false),
    )
    .expect("the test takes the slot on another VCI");
    // WriteDataByIdentifier is outside the read-only policy.
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x2E }],
    );
    let mut child = ngr_agent()
        .args(["run", "--vci", "test-vci", "--mode", "user", "--program"])
        .arg(program)
        .arg("--workers")
        .arg(&temp.0)
        .arg("--locks")
        .arg(&locks)
        .env("NGR_J2534_DEFINITION_DIR", &definitions)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ngr-agent should start");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("the run's status").is_none(),
        "the run waits for the reprogramming slot"
    );
    drop(held);
    // With the slot free, the run goes on and fails at the VCI, which no definition names.
    let output = child.wait_with_output().expect("ngr-agent should end");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

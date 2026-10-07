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
    };
    let path = dir.join("program.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&program).expect("program should serialize"),
    )
    .expect("program file should be writable");
    path
}

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
        .args(["run", "--vci", "none", "--program"])
        .arg(temp.0.join("missing.json"))
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "cannot read");
}

#[test]
fn refused_program_exits_with_1_before_resolving_the_vci() {
    let temp = TempDir::new("refused");
    // WriteDataByIdentifier is outside the read-only policy.
    let program = write_program(
        &temp.0,
        vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x2E }],
    );
    let output = ngr_agent()
        .args(["run", "--vci", "none", "--program"])
        .arg(program)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "refused");
}

// The tests below point the agent at a test config through the debug-only VCI_CONFIG_PATH
// override (ADR-073).

#[cfg(debug_assertions)]
fn write_config(dir: &Path, library_path: Option<&Path>) -> PathBuf {
    let path = dir.join("config.toml");
    let text = match library_path {
        Some(library) => format!(
            "[config.apis.j2534-0404.libs.\"test-vci\"]\nlibrary_path = {:?}\n",
            library.display().to_string()
        ),
        None => String::new(),
    };
    std::fs::write(&path, text).expect("test config file should be writable");
    path
}

#[cfg(debug_assertions)]
#[test]
fn unknown_vci_exits_with_1() {
    let temp = TempDir::new("unknown-vci");
    let config = write_config(&temp.0, None);
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(&temp.0)
        .env("VCI_CONFIG_PATH", config)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "cannot resolve the library of VCI \"test-vci\"");
}

#[cfg(debug_assertions)]
#[test]
fn missing_worker_build_exits_with_1() {
    let temp = TempDir::new("no-worker");
    // Any library header does: the agent's own executable stands in for the VCI library.
    let config = write_config(&temp.0, Some(Path::new(env!("CARGO_BIN_EXE_ngr-agent"))));
    let output = ngr_agent()
        .args(["run", "--vci", "test-vci", "--program"])
        .arg(read_vin(&temp.0))
        .arg("--workers")
        .arg(temp.0.join("empty"))
        .env("VCI_CONFIG_PATH", config)
        .output()
        .expect("ngr-agent should run");
    assert_fails_with(output, "no worker bundled for ABI");
}

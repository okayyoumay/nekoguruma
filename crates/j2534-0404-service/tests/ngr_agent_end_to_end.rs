// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! `ngr-agent run` end to end without hardware: the agent binary resolves `sim-vci`, picks the
//! `j2534-0404-service` build for its ABI from a workers directory, launches it and runs an IR
//! program file on the simulated ECU behind it.
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the `agent` package is
//! tested in the same run (as `cargo test --workspace` and CI do). It is looked up next to the
//! test's `deps` directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";

fn deps_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    exe.parent()
        .expect("test executable has a directory")
        .to_path_buf()
}

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same `deps` directory
/// as this test executable.
fn sim_vci_path() -> PathBuf {
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = deps_dir().join(&name);
    assert!(path.is_file(), "{} should be built", path.display());
    path
}

fn ngr_agent_path() -> PathBuf {
    let dir = deps_dir();
    let path = dir
        .parent()
        .expect("deps directory has a parent")
        .join(format!("ngr-agent{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "{} should be built: test the agent package in the same run \
         (cargo test --workspace, or -p agent -p j2534-0404-service)",
        path.display()
    );
    path
}

/// Removes the temporary directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads the VIN, then a DID the simulated ECU does not know.
fn program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x22 },
        ],
        constants: vec![vec![0xF1, 0x90], vec![0x12, 0x34]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// Installs the service binary as `<workers>/<ABI name>/j2534-0404-service`, the layout
/// `ngr-agent` selects from.
fn install_worker(workers: &Path, abi_name: &str) {
    let service = Path::new(env!("CARGO_BIN_EXE_j2534-0404-service"));
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}

#[test]
fn ngr_agent_runs_a_program_file_on_sim_vci() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ngr-agent-e2e-{nanos}"));
    std::fs::create_dir_all(&root).expect("temporary directory should be writable");
    let temp = TempDir(root.clone());

    let sim_vci = sim_vci_path();
    let abi = worker_host::abi::detect_file(&sim_vci).expect("sim-vci should have a known ABI");
    let workers = root.join("workers");
    install_worker(&workers, abi.name());

    let config_path = root.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            sim_vci.display().to_string()
        ),
    )
    .expect("test config file should be writable");
    let program_path = root.join("program.json");
    std::fs::write(
        &program_path,
        serde_json::to_vec(&program()).expect("program should serialize"),
    )
    .expect("program file should be writable");

    // The agent and the worker it launches both read the library path from this config.
    let mut child = Command::new(ngr_agent_path())
        .args(["run", "--vci", LIBRARY_NAME, "--program"])
        .arg(&program_path)
        .arg("--workers")
        .arg(&workers)
        // Its own lock directory, not the one next to the shared target directory's binary.
        .arg("--locks")
        .arg(root.join("locks"))
        .env("VCI_CONFIG_PATH", &config_path)
        .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
        // The simulated ECU uses its built-in configuration.
        .env_remove("NGR_SIM_ECU_CONFIG")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("ngr-agent should start");

    // A wedged agent or worker must fail the test, not hang the CI job. The output is one
    // short line, so it cannot fill the pipe while the agent runs.
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().expect("ngr-agent should be waitable") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("ngr-agent did not finish in time");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().expect("ngr-agent output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        status.success(),
        "ngr-agent failed: {status}, stdout {stdout}"
    );

    let state: VmState = serde_json::from_str(stdout.trim()).expect("stdout should be a VM state");
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    // A negative response is a result for the procedure to inspect, not a job failure.
    assert_eq!(
        state.stack,
        [Value::Bytes(vin), Value::Bytes(vec![0x7F, 0x22, 0x31])],
        "{state:?}"
    );
    drop(temp);
}

// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! One ECU's CSV + Starlark definition end to end through the D-PDU worker (M2 exit criterion 1),
//! as `crates/j2534-0404-service/tests/definition_end_to_end.rs` runs it through the j2534-0404
//! worker: `ngr-agent run --api dpdu --document` runs each document on the simulated ECU
//! `iso22900-mock` answers from (ADR-299), configured as the J2534 test configures `sim-vci`'s.
//! The variant is identified from the same answers, the read procedure checks the same decoded
//! values, and the routine procedure runs with the same result: a debug-build agent treats
//! `iso22900-mock` as a simulated VCI and may change the session there (ADR-331).
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the run builds the
//! `agent` package's binaries (as `cargo test --workspace` and CI do). It is looked up next to
//! the test's `deps` directory and checked against its sources
//! (`crates/agent/tests/support/ngr_agent_binary.rs`): build it first with
//! `cargo build -p agent --bin ngr-agent` to run this package's tests alone.

#[path = "../../diag-frontend/tests/support/definition_end_to_end.rs"]
mod definition;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diag_ir::VmState;

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

#[path = "../../agent/tests/support/ngr_agent_binary.rs"]
mod ngr_agent_binary;

fn ngr_agent_path() -> PathBuf {
    ngr_agent_binary::ngr_agent_path("iso22900-service")
}

/// Installs the service binary as `<workers>/<ABI name>/iso22900-service`, the layout
/// `ngr-agent` selects from.
fn install_worker(workers: &Path, abi_name: &str) {
    let service = Path::new(env!("CARGO_BIN_EXE_iso22900-service"));
    let dir = workers.join(abi_name);
    std::fs::create_dir_all(&dir).expect("workers directory should be writable");
    let target = dir.join(service.file_name().expect("service binary has a file name"));
    if std::fs::hard_link(service, &target).is_err() {
        std::fs::copy(service, &target).expect("service binary should be copyable");
    }
}

/// What every run needs: the workers, the configuration and the mock's root description file.
struct Setup {
    root: PathBuf,
    workers: PathBuf,
    config: PathBuf,
    root_file: PathBuf,
}

impl Setup {
    fn new(root: &Path) -> Self {
        let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
        let abi =
            worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
        let workers = root.join("workers");
        install_worker(&workers, abi.name());
        let config = root.join("config.toml");
        std::fs::write(&config, "").expect("test config file should be writable");
        let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
            .expect("root description file should be writable");
        Self {
            root: root.to_owned(),
            workers,
            config,
            root_file,
        }
    }

    /// Runs `document` with `ngr-agent` on the ECU [`definition::ecu_config`] configures, and
    /// returns the final state it prints.
    fn run(&self, name: &str, document: &[u8]) -> VmState {
        self.run_on(&definition::ecu_config(), name, document)
    }

    /// [`Setup::run`] on the ECU `ecu_config` configures. Every run starts a new mock, which
    /// reads it again.
    fn run_on(&self, ecu_config: &Path, name: &str, document: &[u8]) -> VmState {
        let path = self.root.join(format!("{name}.dir"));
        std::fs::write(&path, document).expect("document file should be writable");
        let mut child = Command::new(ngr_agent_path())
            .args([
                "run",
                "--api",
                "dpdu",
                "--vci",
                LIBRARY_NAME,
                "--mode",
                "user",
                "--document",
            ])
            .arg(&path)
            .arg("--workers")
            .arg(&self.workers)
            .arg("--locks")
            .arg(self.root.join("locks"))
            // The procedures log with `diag.log`, whose messages go to a communication log.
            .arg("--comm-log")
            .arg(self.root.join("commlog"))
            .env("VCI_CONFIG_PATH", &self.config)
            .env("NGR_PDU_API_ROOT_FILE", &self.root_file)
            .env("NGR_SIM_ECU_CONFIG", ecu_config)
            .env_remove("NGR_SIM_ECU_STATE")
            .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("ngr-agent should start");
        // A wedged agent or worker must fail the test, not hang the CI job.
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait().expect("ngr-agent should be waitable") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("ngr-agent did not finish {name} in time");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let output = child.wait_with_output().expect("ngr-agent output");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(status.success(), "{name}: ngr-agent failed: {status}");
        serde_json::from_str(stdout.trim()).expect("stdout should be a VM state")
    }
}

#[test]
fn a_definition_runs_end_to_end_through_the_d_pdu_worker() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let setup = Setup::new(temp.path());

    // Identify the variant: each variant's identification services, sent to the ECU.
    let reads = definition::documents("read.star");
    let identify = |ecu_config: &Path| {
        let runs: Vec<_> = reads
            .iter()
            .map(|(name, document)| {
                let (identification, services) = definition::identification(document);
                let state = setup.run_on(
                    ecu_config,
                    &format!("{name}-identification"),
                    &identification,
                );
                (services, state)
            })
            .collect();
        definition::identified(&reads, &runs).expect("a variant is identified")
    };
    let variant = identify(&definition::ecu_config());
    // The simulated ECU reports software version 1.0.0, the first generation's.
    assert_eq!(reads[variant].0, "exampleEcuV1");
    // An ECU of the second generation is told apart, although its variant sorts after the first.
    let v2 = definition::ecu_config_with_version(&temp.path().join("ecu-v2.json"), "2.0.0");
    assert_eq!(reads[identify(&v2)].0, "exampleEcuV2");

    // The read procedure checks every value it decodes; a value that differs fails the run.
    let state = setup.run(&reads[variant].0, &reads[variant].1);
    assert!(state.call_stack.is_empty(), "{state:?}");

    // The routine procedure checks the programming session's P2* and the ECU's refusal of the
    // routine, as through the j2534-0404 worker.
    let routines = definition::documents("routine.star");
    assert_eq!(routines[variant].0, reads[variant].0);
    let state = setup.run(
        &format!("{}-routine", routines[variant].0),
        &routines[variant].1,
    );
    assert!(state.call_stack.is_empty(), "{state:?}");

    // Each run's communication log names the D-PDU API and the module's version data as the
    // mock reports it (design 8.11 "Header").
    let logs: Vec<_> = std::fs::read_dir(temp.path().join("commlog"))
        .expect("the communication logs")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.to_string_lossy().ends_with(".commlog.jsonl"))
        .collect();
    assert!(!logs.is_empty());
    for path in logs {
        let text = std::fs::read_to_string(&path).expect("a log");
        let connected: serde_json::Value = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
            .find(|line| line["event"] == "vci_connected")
            .unwrap_or_else(|| panic!("vci_connected in {text}"));
        assert_eq!(connected["api"], "dpdu", "{connected}");
        assert!(
            connected.get("unsigned_long_bytes").is_none(),
            "{connected}"
        );
        let version = &connected["version"];
        assert_eq!(version["vendor"], "Mock Vendor", "{connected}");
        assert_eq!(version["hardware_name"], "Mock Hardware", "{connected}");
        assert_eq!(version["hardware_serial_number"], 123456, "{connected}");
        assert_eq!(version["api_library_name"], "iso22900-mock", "{connected}");
    }
}

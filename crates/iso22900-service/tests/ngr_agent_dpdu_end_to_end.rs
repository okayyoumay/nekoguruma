// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! `ngr-agent run --api dpdu` end to end without hardware: the agent binary resolves the
//! `iso22900-mock` library through a root description file, picks the `iso22900-service` build
//! for its ABI from a workers directory, launches it and runs the procedure of a variant's document on the simulated
//! ECU the mock answers from (ADR-299).
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the `agent` package is
//! tested in the same run (as `cargo test --workspace` and CI do). It is looked up next to the
//! test's `deps` directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";

fn ngr_agent_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test executable is in a deps directory")
        .join(format!("ngr-agent{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "{} should be built: test the agent package in the same run \
         (cargo test --workspace, or -p agent -p iso22900-service)",
        path.display()
    );
    path
}

/// A variant's document whose procedure is `procedure`: its protocol is UDS over ISO 15765 at
/// 500 kbit/s on 7E0 / 7E8, the link the job opens (ADR-324).
fn document(procedure: &Program) -> Vec<u8> {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("sim".into());
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = Some(postcard::to_allocvec(procedure).expect("program should serialize"));
    encode_ecu_document(&document)
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

/// The mock's simulated ECU, its library and workers, and the configuration every run uses.
struct Setup {
    root: PathBuf,
    library: PathBuf,
    workers: PathBuf,
    config: PathBuf,
    ecu_config: PathBuf,
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
        // The mock attaches a simulated ECU with this configuration at PDUConstruct (ADR-299):
        // `sim-vci`'s default one.
        let ecu_config = root.join("ecu.json");
        std::fs::write(
            &ecu_config,
            r#"{"vin":"NGRSIMECU00000001","part_number":"NGR-SIM-ECU","sw_version":"1.0.0",
                "response_delay_ms":0,"drop_at_block":null,"require_security_access":false,
                "require_gateway_auth":false,"fail_checksum":false}"#,
        )
        .expect("ECU configuration should be writable");
        Self {
            root: root.to_owned(),
            library,
            workers,
            config,
            ecu_config,
        }
    }

    /// Runs `ngr-agent` on `procedure`'s document with the root file `root_file`, keeping a
    /// communication log in `comm_log` when given; returns its exit status and stdout.
    fn run(
        &self,
        root_file: &Path,
        procedure: &Program,
        comm_log: Option<&Path>,
    ) -> (std::process::ExitStatus, String) {
        let document_path = self.root.join("variant.dir");
        std::fs::write(&document_path, document(procedure)).expect("document should be writable");
        let mut command = Command::new(ngr_agent_path());
        command
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
            .arg(&document_path)
            .arg("--workers")
            .arg(&self.workers)
            .arg("--locks")
            .arg(self.root.join("locks"));
        if let Some(dir) = comm_log {
            command.arg("--comm-log").arg(dir);
        }
        let mut child = command
            .env("VCI_CONFIG_PATH", &self.config)
            .env("NGR_PDU_API_ROOT_FILE", root_file)
            .env("NGR_SIM_ECU_CONFIG", &self.ecu_config)
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
                panic!("ngr-agent did not finish in time");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let output = child.wait_with_output().expect("ngr-agent output");
        (status, String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[test]
fn ngr_agent_runs_a_document_on_a_d_pdu_api_vci() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let setup = Setup::new(temp.path());
    let root_file = iso22900_mock::write_root_file(temp.path(), LIBRARY_NAME, &setup.library)
        .expect("root description file should be writable");
    let (status, stdout) = setup.run(&root_file, &program(), None);
    assert!(
        status.success(),
        "ngr-agent failed: {status}, stdout {stdout}"
    );

    let state: VmState = serde_json::from_str(stdout.trim()).expect("stdout should be a VM state");
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    // A negative response is a result for the procedure, as on the J2534 worker.
    assert_eq!(
        state.stack,
        [Value::Bytes(vin), Value::Bytes(vec![0x7F, 0x22, 0x31])],
        "{state:?}"
    );
}

/// TesterPresent with its suppress bit set, which the simulated ECU leaves unanswered, so the
/// mock ends the primitive with its receive timeout and its own extra error code.
fn unanswered_program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x3E }],
        constants: vec![vec![0x80]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// The `detail` of the one exchange entry in the communication log under `dir`.
fn exchange_detail(dir: &Path) -> serde_json::Value {
    let mut entries: Vec<serde_json::Value> = std::fs::read_dir(dir)
        .expect("the communication log directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.to_string_lossy().ends_with(".commlog.jsonl"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .expect("a log")
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
                .collect::<Vec<_>>()
        })
        .filter(|line| line["event"] == "exchange")
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.remove(0)["detail"].clone()
}

#[test]
fn the_communication_log_names_the_mock_s_extra_error_code_by_its_mdf() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let setup = Setup::new(temp.path());
    let code = iso22900_mock::MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP;
    let name = iso22900_mock::MOCK_XTRA_ERR_RX_TIMEOUT_NO_RSP_NAME;
    let hex = format!("{code:#x}");
    // An entry whose MDF names `codes`.
    let with_mdf = |codes: &[(u32, &str)]| {
        let mdf = iso22900_mock::write_module_description_file(temp.path(), codes)
            .expect("the MDF should be writable");
        iso22900_mock::write_root_file_with_mdf(temp.path(), LIBRARY_NAME, &setup.library, &mdf)
            .expect("root description file should be writable")
    };
    // The MDF's name for the code; an MDF that names only another code, and an entry with no
    // MDF, leave the code's number. Each case writes its root file over the last one's.
    for (case, expected) in [("named", name), ("unnamed", &hex), ("no-mdf", &hex)] {
        let root_file = match case {
            "named" => with_mdf(&[(code, name)]),
            "unnamed" => with_mdf(&[(code + 1, "ANOTHER_CODE")]),
            _ => iso22900_mock::write_root_file(temp.path(), LIBRARY_NAME, &setup.library)
                .expect("root description file should be writable"),
        };
        let comm_log = temp.path().join(format!("commlog-{case}"));
        let (status, stdout) = setup.run(&root_file, &unanswered_program(), Some(&comm_log));
        // The unanswered request ends the job; its log still holds the exchange.
        assert!(!status.success(), "{case}: {stdout}");
        let detail = exchange_detail(&comm_log);
        assert_eq!(
            detail["error_event"], "PDU_ERR_EVT_RX_TIMEOUT",
            "{case}: {detail}"
        );
        assert_eq!(detail["extra_error_info"], expected, "{case}: {detail}");
    }
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! `ngr-agent run` end to end without hardware: the agent binary resolves `sim-vci`, picks the
//! `j2534-0404-service` build for its ABI from a workers directory, launches it and runs the
//! procedure of a variant's document on the simulated ECU behind it.
//!
//! `ngr-agent` belongs to another package, so cargo builds it only when the `agent` package is
//! tested in the same run (as `cargo test --workspace` and CI do). It is looked up next to the
//! test's `deps` directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diag_ir::plan::{
    Candidate, Entry, IdentificationPlan, PLAN_SCHEMA_VERSION, PlanDefinition, Selection, Sha256,
};
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

/// A temporary directory with the agent's surroundings: a workers directory holding the
/// service for `sim-vci`'s ABI, an empty configuration file and the registration definition
/// that names the library (through the debug-only `NGR_J2534_DEFINITION_DIR` override, ADR-282).
struct Surroundings {
    _temp: TempDir,
    root: PathBuf,
    workers: PathBuf,
    config_path: PathBuf,
    definition_dir: PathBuf,
}

impl Surroundings {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("ngr-agent-e2e-{pid}-{nanos}"));
        std::fs::create_dir_all(&root).expect("temporary directory should be writable");
        let temp = TempDir(root.clone());

        let sim_vci = sim_vci_path();
        let abi = worker_host::abi::detect_file(&sim_vci).expect("sim-vci should have a known ABI");
        let workers = root.join("workers");
        install_worker(&workers, abi.name());

        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "").expect("test config file should be writable");
        // The agent and the worker it launches both resolve the library through the
        // definition.
        let definition_dir = root.join("definitions");
        std::fs::create_dir_all(&definition_dir).expect("definition directory");
        j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci)
            .expect("definition file should be writable");
        Self {
            _temp: temp,
            root,
            workers,
            config_path,
            definition_dir,
        }
    }

    /// `ngr-agent` with the options every command takes, and the environment it needs.
    fn agent(&self, command: &str) -> Command {
        let mut agent = Command::new(ngr_agent_path());
        agent
            .args([command, "--vci", LIBRARY_NAME, "--mode", "user"])
            .arg("--workers")
            .arg(&self.workers)
            // Its own lock directory, not the one next to the shared target directory's binary.
            .arg("--locks")
            .arg(self.root.join("locks"))
            .env("VCI_CONFIG_PATH", &self.config_path)
            .env("NGR_J2534_DEFINITION_DIR", &self.definition_dir)
            .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
            // The simulated ECU uses its built-in configuration.
            .env_remove("NGR_SIM_ECU_CONFIG");
        agent
    }
}

/// Runs `agent` to its end, failing the test if it does not finish in time. Gives its output and
/// what it wrote on stderr, which goes to a file so that it cannot fill a pipe.
fn finish(mut agent: Command, stderr_file: &Path) -> (std::process::Output, String) {
    let stderr = std::fs::File::create(stderr_file).expect("stderr file");
    let mut child = agent
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("ngr-agent should start");
    // A wedged agent or worker must fail the test, not hang the CI job. The output is one
    // short line, so it cannot fill the pipe while the agent runs.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if child
            .try_wait()
            .expect("ngr-agent should be waitable")
            .is_some()
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("ngr-agent did not finish in time");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().expect("ngr-agent output");
    let stderr = std::fs::read_to_string(stderr_file).unwrap_or_default();
    // Shown when the test fails.
    eprintln!("{stderr}");
    (output, stderr)
}

#[test]
fn ngr_agent_runs_a_document_on_sim_vci() {
    let surroundings = Surroundings::new();
    let document_path = surroundings.root.join("variant.dir");
    std::fs::write(&document_path, document(&program())).expect("document should be writable");

    let mut agent = surroundings.agent("run");
    agent.arg("--document").arg(&document_path);
    let (output, _) = finish(agent, &surroundings.root.join("stderr.txt"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ngr-agent failed: {}, stdout {stdout}",
        output.status
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
    drop(surroundings);
}

/// A variant's document that declares only its link: UDS over ISO 15765 at 500 kbit/s on
/// `tx_id` / `rx_id`, with the functional identifier 0x7DF. `name` keeps two documents apart.
fn link_document(name: &str, tx_id: u32, rx_id: u32) -> Vec<u8> {
    use diag_ir::declaration::{EcuDocumentT, EcuVariantT, ProtocolT, encode_ecu_document};
    let mut protocol = ProtocolT::default();
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = tx_id;
    protocol.rx_id = rx_id;
    protocol.functional_id = 0x7DF;
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some(name.into());
    variant.protocols = Some(vec![protocol]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    encode_ecu_document(&document)
}

/// `ngr-agent identify` end to end (row E4 of ADR-398): a Candidates plan whose one candidate
/// is declared at 0x7E1 / 0x7E9, where nothing answers, beside a definition at the simulated
/// ECU's own pair, 0x7E0 / 0x7E8, which answers although no candidate explains it. The report
/// lists the plan's candidates unchanged (presence drops none, ADR-327 item 2) and the
/// presence section holds the one answer.
#[test]
fn ngr_agent_identifies_presence_on_sim_vci() {
    let surroundings = Surroundings::new();
    let silent = link_document("silent", 0x7E1, 0x7E9);
    let answering = link_document("answering", 0x7E0, 0x7E8);
    let hash = |bytes: &[u8]| -> Sha256 {
        agent::JobDocument::new(bytes.to_vec())
            .expect("the document loads")
            .sha256()
    };
    let plan = IdentificationPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        selection: Selection::Candidates,
        candidates: vec![Candidate {
            id: "candidate-a".to_owned(),
            values: Vec::new(),
            vin_rules: Vec::new(),
            field_rules: Vec::new(),
            entries: vec![Entry {
                role: "engine".to_owned(),
                optional: false,
                gateway: None,
                alternatives: vec!["silent-ecu".to_owned()],
            }],
        }],
        definitions: vec![
            PlanDefinition {
                id: "silent-ecu".to_owned(),
                documents: vec![hash(&silent)],
            },
            PlanDefinition {
                id: "answering-ecu".to_owned(),
                documents: vec![hash(&answering)],
            },
        ],
    };
    let plan_path = surroundings.root.join("plan.bin");
    std::fs::write(&plan_path, plan.encode().expect("the plan encodes")).expect("plan file");
    let silent_path = surroundings.root.join("silent.dir");
    let answering_path = surroundings.root.join("answering.dir");
    std::fs::write(&silent_path, &silent).expect("document file");
    std::fs::write(&answering_path, &answering).expect("document file");
    // A document the plan does not name is ignored with a warning (row D4).
    let unnamed_path = surroundings.root.join("unnamed.dir");
    std::fs::write(&unnamed_path, link_document("unnamed", 0x7E2, 0x7EA)).expect("document file");

    let mut agent = surroundings.agent("identify");
    agent
        .arg("--plan")
        .arg(&plan_path)
        .arg("--document")
        .arg(&silent_path)
        .arg("--document")
        .arg(&answering_path)
        // The same document twice is one entry (row D3).
        .arg("--document")
        .arg(&answering_path)
        .arg("--document")
        .arg(&unnamed_path);
    let (output, stderr) = finish(agent, &surroundings.root.join("stderr.txt"));
    assert!(
        stderr.contains("unnamed.dir is not named by the plan"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ngr-agent failed: {}, stdout {stdout}",
        output.status
    );
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout should be the report as JSON");
    assert_eq!(report["candidates"], serde_json::json!(["candidate-a"]));
    // One answer, from the response identifier of the answering ECU, with the positive
    // response to the presence request.
    let presence = report["presence"].as_array().expect("a presence list");
    assert_eq!(presence.len(), 1, "{report}");
    assert_eq!(presence[0]["rx_id"], 0x7E8, "{report}");
    assert_eq!(presence[0]["bytes"], "7e00", "{report}");
    assert_eq!(report["presence_links_opened"], 1, "{report}");
    assert_eq!(report["presence_interrupted"], false, "{report}");
    assert_eq!(report["endpoints"].as_array().map(Vec::len), Some(2));
    assert!(output.stdout.ends_with(b"\n") && !stdout.trim().contains('\n'));
    drop(surroundings);
}

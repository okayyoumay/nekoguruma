// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! The agent's unbound indication path (design 8.11 `sent_evidence`, ADR-427) end to end on a
//! D-PDU API: `ngr-agent run --api dpdu` runs a document's procedure through `iso22900-service` on
//! the simulated ECU `iso22900-mock` answers from (ADR-299), with the mock's response items
//! carrying no times (`SimEcuCommand::ResponseTimestamps`) and its transmit and start-of-message
//! indication items unbound (`SimEcuCommand::IndicationBinding`): `hCop` is undefined and the CAN
//! identifier is in the extra info header, as the J2534 worker queues them. The only source of an
//! exchange's transmit done and start is then the unbound indications. The mock's control reaches
//! the worker through `NGR_SIM_ECU_COMMANDS` on the spawned agent, so no test sets a variable of
//! this process.
//!
//! `ngr-agent` belongs to another package: build it first with
//! `cargo build -p agent --bin ngr-agent` to run this package's tests alone (see
//! `ngr_agent_dpdu_end_to_end.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diag_ir::{IR_SCHEMA_VERSION, Op, Program};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

#[path = "../../agent/tests/support/ngr_agent_binary.rs"]
mod ngr_agent_binary;

fn ngr_agent_path() -> PathBuf {
    ngr_agent_binary::ngr_agent_path("iso22900-service")
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

/// The mock's control: the default ECU, response items without times, indication items unbound
/// with the document's identifiers (7E0 = 2016, 7E8 = 2024).
const COMMANDS: &str = concat!(
    r#"[{"attach":{}},{"response_timestamps":{"enabled":false}},"#,
    r#"{"indication_binding":{"bound":false,"request_id":2016,"response_id":2024}}]"#,
);

/// The VIN read twice: no session control, so the agent starts no tester present.
fn reads() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
        ],
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// The mock's control with its default binding: indication items bound to their ComPrimitive.
const BOUND_COMMANDS: &str = r#"[{"attach":{}},{"response_timestamps":{"enabled":false}}]"#;

/// The session control, which starts the agent's tester present (the mock's own, ADR-423), then
/// the VIN read.
fn session_then_read() -> Program {
    Program {
        constants: vec![vec![0x03], vec![0xF1, 0x90]],
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x22 },
        ],
        ..reads()
    }
}

/// Runs `ngr-agent` on `procedure` with the mock's `COMMANDS`, and returns the exchange entries
/// of its communication log.
fn run(procedure: &Program) -> Vec<serde_json::Value> {
    run_with(procedure, COMMANDS)
}

/// Like [`run`], with the mock's control `commands`.
fn run_with(procedure: &Program, commands: &str) -> Vec<serde_json::Value> {
    run_log(procedure, commands)
        .0
        .into_iter()
        .filter(|line| line["event"] == "exchange")
        .collect()
}

/// Like [`run_with`], but returns every line of the communication log, in file order, and the
/// agent's stderr, which the worker it spawns shares (its log lines).
fn run_log(procedure: &Program, commands: &str) -> (Vec<serde_json::Value>, String) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let abi = worker_host::abi::detect_file(&library).expect("the mock should have a known ABI");
    let workers = root.join("workers");
    install_worker(&workers, abi.name());
    let config = root.join("config.toml");
    std::fs::write(&config, "").expect("test config file should be writable");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    let document_path = root.join("variant.dir");
    std::fs::write(&document_path, document(procedure)).expect("document should be writable");
    let comm_log = root.join("commlog");
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
        .arg(&document_path)
        .arg("--workers")
        .arg(&workers)
        .arg("--locks")
        .arg(root.join("locks"))
        .arg("--comm-log")
        .arg(&comm_log)
        .env("VCI_CONFIG_PATH", &config)
        .env("NGR_PDU_API_ROOT_FILE", &root_file)
        .env(iso22900_mock::ECU_COMMANDS_ENV, commands)
        .env_remove(iso22900_mock::ECU_CONFIG_ENV)
        .env_remove("NGR_SIM_ECU_STATE")
        .env_remove("VCI_SERVICE_INSECURE_NO_AUTH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ngr-agent should start");
    // Drained while the agent runs, so a full pipe cannot stall it; echoed for the test output.
    let stderr_reader = {
        use std::io::Read;
        let mut stderr = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            text.push_str(&String::from_utf8_lossy(&bytes));
            eprint!("{text}");
            text
        })
    };
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
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        status.success(),
        "ngr-agent failed: {status}, stdout {stdout}"
    );
    let stderr = stderr_reader.join().expect("stderr reader");
    let lines = std::fs::read_dir(&comm_log)
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
        .collect::<Vec<_>>();
    (lines, stderr)
}

/// The VCI time `field` of an exchange's `times.vci`: its microseconds on the device clock.
fn vci_us(entry: &serde_json::Value, field: &str) -> u64 {
    let time = &entry["times"]["vci"][field];
    assert_eq!(time["clock"], "device", "{field}: {entry}");
    time["us"]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} has a time: {entry}"))
}

/// With the mock's indication items unbound and its response items carrying no times, each
/// exchange takes its transmit done and its response's start from the unbound indications whose
/// CAN identifier is the link's (design 8.11 `sent_evidence`, "Link settings for the log"; the
/// agent's link starts no tester present without a session control). The VIN read's 20-byte
/// answer spans frames, so it has a start-of-message indication.
#[test]
fn the_agent_takes_an_exchange_s_times_from_unbound_indications() {
    let exchanges = run(&reads());
    assert_eq!(exchanges.len(), 2, "{exchanges:?}");
    let mut previous_end = 0;
    for entry in &exchanges {
        assert_eq!(entry["outcome"]["result"], "positive", "{entry}");
        assert_eq!(entry["sent"], "confirmed", "{entry}");
        // The response items carry no transmit done, so the unbound transmit indication gave it.
        assert_eq!(entry["sent_evidence"], "tx_indication", "{entry}");
        let (tx_done, rx_start, rx_end) = (
            vci_us(entry, "tx_done"),
            vci_us(entry, "rx_start"),
            vci_us(entry, "rx_end"),
        );
        // The response carries no start either, so the start-of-message indication gave it: the
        // mock stamps it at the response's delivery, the same time as the response's item.
        assert!(tx_done < rx_start, "{entry}");
        assert_eq!(rx_start, rx_end, "{entry}");
        assert_eq!(
            entry["timing"]["measured_by"], "vci_tx_done_to_rx_start",
            "{entry}"
        );
        // The transmit done is this exchange's own, not the previous one's.
        assert!(tx_done > previous_end, "{entry}");
        previous_end = rx_end;
    }
}

/// The log of `session_then_read` under the mock's control `commands`: the session control
/// starts the mock's tester present, which raises an unbound transmit indication of its own with
/// no header (ADR-430). The agent cannot match it to a request, so it logs it as an `unattached`
/// transmit indication with no identifier. The VIN read that follows has its own transmit done,
/// taken from its own indication and later than the tester present's, and the tester present's
/// line changes neither exchange's `sent` nor `sent_evidence`.
fn check_read_after_tester_present(commands: &str, evidence_of_session: &str) -> String {
    let (lines, stderr) = run_log(&session_then_read(), commands);
    let exchanges: Vec<_> = lines
        .iter()
        .filter(|line| line["event"] == "exchange")
        .collect();
    assert_eq!(exchanges.len(), 2, "{lines:?}");
    let (session, read) = (exchanges[0], exchanges[1]);
    assert_eq!(session["outcome"]["result"], "positive", "{session}");
    assert_eq!(read["outcome"]["result"], "positive", "{read}");
    assert_eq!(read["sent"], "confirmed", "{read}");
    assert_eq!(read["sent_evidence"], "tx_indication", "{read}");
    // The tester present's indication, logged apart from the exchanges.
    let tester_present: Vec<_> = lines
        .iter()
        .filter(|line| line["event"] == "unattached" && line["item"] == "transmit_indication")
        .collect();
    assert!(!tester_present.is_empty(), "no unattached line: {lines:?}");
    for line in &tester_present {
        assert!(line.get("identifier").is_none(), "{line}");
        assert_eq!(line["clock"], "device", "{line}");
    }
    // It changes no exchange's evidence: the session control's is what it is without it.
    assert_eq!(session["sent"], "confirmed", "{session}");
    assert_eq!(session["sent_evidence"], evidence_of_session, "{session}");
    let session_end = vci_us(session, "rx_end");
    let (tx_done, rx_end) = (vci_us(read, "tx_done"), vci_us(read, "rx_end"));
    assert!(tx_done > session_end, "{read}");
    assert!(tx_done <= rx_end, "{read}");
    // The read's transmit done is its own: not any tester present's time, and later than the
    // first one's, which went out when the session control ended.
    let times: Vec<u64> = tester_present
        .iter()
        .map(|line| line["vci_us"].as_u64().expect("a raw VCI time"))
        .collect();
    assert!(times.iter().all(|time| *time != tx_done), "{lines:?}");
    assert!(times.iter().any(|time| *time > session_end), "{lines:?}");
    let first = times.iter().copied().filter(|t| *t > session_end).min();
    assert!(first.is_some_and(|first| tx_done > first), "{lines:?}");
    stderr
}

/// Unbound items: the agent's tester present starts after the session control and its headerless
/// unbound transmit indication is not taken for the VIN read's.
#[test]
fn the_tester_presents_headerless_indication_is_not_the_reads_transmit_done_unbound() {
    let stderr = check_read_after_tester_present(COMMANDS, "tx_indication");
    // The mock's control gives the request's indications a header with no ComPrimitive, which the
    // worker warns about once for the link and kind (ADR-430), and sums up when the link goes.
    let warnings: Vec<_> = stderr
        .lines()
        .filter(|line| line.contains("an extra info header (identifier 0x7e0)"))
        .collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(warnings[0].contains("transmit indication"), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("items of no ComPrimitive with an extra info header")),
        "no summary: {stderr}"
    );
}

/// Bound items (the default binding): the mock's tester present still raises its headerless
/// transmit indication, which the agent logs as an `unattached` line without an identifier, and
/// the VIN read's transmit done is its own bound indication.
#[test]
fn the_tester_presents_indication_is_logged_apart_and_the_read_keeps_its_own_when_bound() {
    let stderr = check_read_after_tester_present(BOUND_COMMANDS, "tx_indication");
    // The tester present's item has no header, and the others are bound: nothing to warn about.
    assert!(!stderr.contains("an extra info header"), "{stderr}");
}

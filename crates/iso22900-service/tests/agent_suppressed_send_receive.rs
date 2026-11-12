// Relies on the debug-only runtime VCI_CONFIG_PATH and NGR_PDU_API_ROOT_FILE overrides; see
// ADR-073 and ADR-282.
#![cfg(debug_assertions)]
//! A suppressed request through the generic send-receive (`Op::SendReceive`, ADR-321) end to end
//! on a D-PDU API link (ADR-408): the agent runs a procedure against a real `iso22900-service`
//! worker and `iso22900-mock`, whose simulated ECU drops its answer to `10 83` (the
//! suppress-positive-response bit set), so it stays silent (`NGR_SIM_ECU_COMMANDS`, ADR-423).
//!
//! The request's result is the empty answer and the run goes on. The link is one whose VCI
//! absorbs pendings, so the communication log classifies the silence `undetermined` (H4, design
//! 8.11): a pending the VCI absorbed may have obliged an answer. The second case lets the ECU
//! send a pending first: the agent turns the mock's `CP_RC78Handling` on, so the mock absorbs the
//! pending as a library does and the silence that follows is the receive timeout, with no
//! pending listed in the entry.
//! The J2534 counterpart is `crates/j2534-0404-service/tests/agent_end_to_end.rs`.
//!
//! Each case sets process-wide environment for the spawned service, so the cases take turns
//! (`CASE`).

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{CommLogSetup, CommLogStatus, Job, JobDocument, JobLimits, JobRun, run_program};
use diag_ir::declaration::{
    ComParamT, EcuDocumentT, EcuVariantT, NegativeShapeT, PositiveResponseT, ProtocolT, RequestT,
    ServiceT, encode_ecu_document,
};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "NGR Mock D-PDU API";

/// Held by a case while it uses the process environment and its worker.
static CASE: Mutex<()> = Mutex::new(());

/// P2* the document declares, in microseconds: the silence after a pending lasts this long. It is
/// larger than the agent's default P2 (1000 ms), so a silence that ends at P2 shows.
const P2_STAR_US: i64 = 1_500_000;

/// Sends `10 83` of the declared service 1, then a VIN read, which shows the run goes on.
fn program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::SendReceive { service: 1 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x22 },
        ],
        constants: vec![vec![0x10, 0x83], vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// A document with UDS over ISO 15765 on the simulated ECU's identifiers and one service, 1:
/// DiagnosticSessionControl, whose positive response starts with 0x50.
fn document(procedure: &Program) -> Vec<u8> {
    let mut positive = PositiveResponseT::default();
    positive.sid = 0x50;
    let mut service = ServiceT::default();
    service.id = 1;
    service.short_name = Some("sessionControl".into());
    let mut request = RequestT::default();
    request.sid = 0x10;
    service.request = Some(Box::new(request));
    service.positive = Some(Box::new(positive));
    let mut shape = NegativeShapeT::default();
    shape.marker = 0x7F;
    shape.echoes_sid = true;
    shape.code_at = 2;
    shape.pending = Some(vec![0x78]);
    let mut p2_star = ComParamT::default();
    p2_star.short_name = Some("CP_P2Star".into());
    p2_star.value = P2_STAR_US;
    let mut protocol = ProtocolT::default();
    protocol.params = Some(vec![p2_star]);
    protocol.short_name = Some("ISO15765".into());
    protocol.application_layer = Some("UDS".into());
    protocol.baud_rate = 500_000;
    protocol.tx_id = 0x7E0;
    protocol.rx_id = 0x7E8;
    protocol.negative_shape = Some(Box::new(shape));
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("sim".into());
    variant.protocols = Some(vec![protocol]);
    variant.services = Some(vec![service]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = Some(postcard::to_allocvec(procedure).expect("program should serialize"));
    encode_ecu_document(&document)
}

fn job_guards(dir: &Path) -> JobGuards {
    let setup = GuardSetup {
        dir: dir.to_owned(),
        vci: "agent-suppressed-dpdu-vci".to_owned(),
    };
    let never = AtomicBool::new(false);
    JobGuards::take(&setup, Duration::from_millis(10), &never).expect("guards should be free")
}

const DROP: &str = r#"{"inject_fault":{"fault":"drop_response","request":[16,131]}}"#;
const PENDING: &str = r#"{"inject_fault":{"fault":{"response_pending":{"count":1,"interval_ms":20}},"request":[16,131]}}"#;

#[test]
fn a_suppressed_send_receive_silence_on_a_d_pdu_link_is_undetermined_h4() {
    // The ECU drops its answer to `10 83`, so it stays silent.
    run_case(&format!("[{DROP}]"), Duration::ZERO);
}

/// The ECU sends a response pending message to `10 83` and then nothing. The mock absorbs the
/// pending (`CP_RC78Handling` is on), so the request ends in the receive timeout, as it would
/// on a library that handles the pending itself.
#[test]
fn a_suppressed_send_receive_silence_after_an_absorbed_pending_is_undetermined_h4() {
    // The pending comes 20 ms in and the receive timer reloads to P2*: the silence lasts that long.
    run_case(
        &format!("[{PENDING},{DROP}]"),
        Duration::from_micros(P2_STAR_US as u64),
    );
}

fn run_case(ecu_commands: &str, silence_at_least: Duration) {
    let _case = CASE.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("temporary directory");
    let root = temp.path();
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    // The mock attaches its simulated ECU only when it is given a configuration.
    let ecu_path = root.join("ecu.json");
    std::fs::write(
        &ecu_path,
        r#"{"vin": "NGRSIMECU00000005", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false, "s3_server_ms": 5000}"#,
    )
    .expect("ECU config file should be writable");
    let library = iso22900_mock::mock_library_path().expect("the mock cdylib should be built");
    let root_file = iso22900_mock::write_root_file(root, LIBRARY_NAME, &library)
        .expect("root description file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_PDU_API_ROOT_FILE", &root_file);
        std::env::set_var(iso22900_mock::ECU_CONFIG_ENV, &ecu_path);
        std::env::set_var(iso22900_mock::ECU_COMMANDS_ENV, ecu_commands);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(120),
                run_the_job(root, silence_at_least),
            )
            .await
        })
        .expect("the flow should finish in time");
}

async fn run_the_job(root: &Path, silence_at_least: Duration) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_iso22900-service")),
        ServiceKind::Iso22900,
        LIBRARY_NAME,
        OperatingMode::User,
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");
    // The worker has copied its environment; the other tests of a process must not see it.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let setup = CommLogSetup {
        dir: root.join("commlog"),
        key: agent::journal::JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5f90".to_owned()),
            generation: 1,
        },
    };
    std::fs::create_dir_all(&setup.dir).expect("log directory");
    let job = Job::from_document(JobDocument::new(document(&program())).expect("document loads"))
        .expect("the document carries its procedure")
        .with_comm_log(setup.clone());
    let JobRun {
        result,
        guards: _guards,
        comm_log,
    } = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &job.declared_link().expect("the document declares its link"),
        job,
        JobLimits::default(),
        job_guards(&root.join(format!("locks-{nanos}"))),
    )
    .await;
    let state = result.expect("a suppressed request's silence is not a failure");
    assert_eq!(comm_log, CommLogStatus::Complete);
    let [Value::Bytes(silence), Value::Bytes(read)] = state.stack.as_slice() else {
        panic!("{state:?}");
    };
    assert!(silence.is_empty(), "{state:?}");
    assert!(read.starts_with(&[0x62, 0xF1, 0x90]), "{state:?}");

    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    let entries: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
        .filter(|line| line["event"] == "exchange")
        .collect();
    let silent = &entries[0];
    assert_eq!(silent["request"]["bytes"]["clear"], "1083", "{silent}");
    assert_eq!(silent["request"]["suppressed"], true, "{silent}");
    assert_eq!(silent["outcome"]["result"], "no_response", "{silent}");
    assert!(silent["detail"].get("host_error").is_none(), "{silent}");
    assert_eq!(silent["classification"], "undetermined", "{silent}");
    let reason = silent["reason"].as_str().expect("a reason");
    assert!(reason.contains("(H4)"), "{reason}");
    // The exchange, by the agent's clock, lasted at least as long as the silence: P2* after the
    // absorbed pending, not only P2.
    let agent_us = |key: &str| {
        silent["times"]["agent"][key]
            .as_u64()
            .expect("an agent time")
    };
    let lasted = Duration::from_micros(agent_us("end_us") - agent_us("before_us"));
    assert!(
        lasted >= silence_at_least,
        "the exchange took {lasted:?}, less than {silence_at_least:?}: {silent}"
    );
    // The VCI absorbed any pending, so none is listed.
    assert!(
        silent
            .get("pending")
            .is_none_or(|pending| pending.as_array().is_some_and(Vec::is_empty)),
        "{silent}"
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("the worker stops");
}

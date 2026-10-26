// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! An agent job end to end without hardware (ADR-235): `worker-host` launches the real
//! `j2534-0404-service` binary against the `sim-vci` cdylib, and `agent::run_program` runs a
//! diag-ir procedure on a CAN link to the simulated ECU behind it.
//!
//! A second job switches the ECU to its programming session and runs a routine, which only a
//! debug build allows and only on the simulator (ADR-247). A third reads the VIN again and
//! decodes it through the job's document (ADR-316). A fourth runs a procedure of the example
//! ECU that reads DTCs with `diag.read_dtc`, which lowers to the generic send-receive (ADR-321).
//! A fifth keeps a communication log (design 8.11, ADR-347): it reads the VIN, a DID the ECU
//! does not know, and a DID whose response `sim-vci` drops, and the log holds an exchange entry
//! for each, classified by what the host saw. A sixth sends a request longer than ISO 15765 can
//! carry, which the worker refuses with its reason (ADR-350), and its log shows the request not
//! sent, refused by the worker. Every job comes from a document and opens the link its variant
//! declares (ADR-324).
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it sets for the
//! spawned service cannot race with another test.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{CommLogSetup, CommLogStatus, Job, JobDocument, JobLimits, JobRun, run_program};
use diag_ir::declaration::{
    DataKind, EcuDocumentT, EcuVariantT, FieldT, NegativeShapeT, Placement, PositiveResponseT,
    ProtocolT, ServiceT, encode_ecu_document,
};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// The simulated ECU's check-programming-dependencies routine (`sim_ecu::RID_CHECK_PROGRAMMING_DEPENDENCIES`).
const RID_CHECK_PROGRAMMING_DEPENDENCIES: u16 = 0xFF01;

/// `sim-vci` is a dev-dependency, so cargo builds its cdylib into the same `deps` directory
/// as this test executable.
fn sim_vci_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("test executable has a directory");
    let name = format!(
        "{}sim_vci{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = deps.join(&name);
    assert!(path.is_file(), "{} should be built", path.display());
    path
}

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>() as u8
    }
}

/// Removes the temporary config file however the test ends.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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

/// [`program`], then a read of the spare part number, whose response the test has `sim-vci`
/// drop.
fn comm_log_program() -> Program {
    let mut program = program();
    program.constants.push(vec![0xF1, 0x87]);
    program
        .code
        .extend([Op::PushBytes(2), Op::ServiceRequest { service: 0x22 }]);
    program
}

/// Switches to the programming session (ISO 14229-1:2026 clause 9.2), then starts the check
/// programming dependencies routine, which exists only in that session.
fn write_program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x10 },
            Op::PushBytes(1),
            Op::RoutineControl {
                routine: RID_CHECK_PROGRAMMING_DEPENDENCIES,
                sub: 0x01,
            },
        ],
        constants: vec![vec![0x02], Vec::new()],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

/// A variant's document with one protocol, UDS over ISO 15765 at 500 kbit/s on the simulated
/// ECU's identifiers, and one service, 1: ReadDataByIdentifier of the VIN, whose positive
/// response is `62 F1 90` and the 17 ASCII bytes of field 1. Its procedure is `procedure`.
fn vin_document(procedure: &Program) -> Vec<u8> {
    let mut vin = FieldT::default();
    vin.id = 1;
    vin.short_name = Some("vin".into());
    vin.placement = Placement::AfterPrevious;
    vin.bit_length = 17 * 8;
    vin.kind = DataKind::Ascii;
    let mut positive = PositiveResponseT::default();
    positive.sid = 0x62;
    positive.prefix = Some(vec![0xF1, 0x90]);
    positive.fields = Some(vec![vin]);
    let mut service = ServiceT::default();
    service.id = 1;
    service.short_name = Some("readVin".into());
    service.positive = Some(Box::new(positive));
    let mut shape = NegativeShapeT::default();
    shape.marker = 0x7F;
    shape.echoes_sid = true;
    shape.code_at = 2;
    shape.pending = Some(vec![0x78]);
    let mut protocol = ProtocolT::default();
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

/// The job of a [`vin_document`] whose procedure is `procedure`.
fn document_job(procedure: &Program) -> Job {
    let document = JobDocument::new(vin_document(procedure)).expect("the document loads");
    Job::from_document(document).expect("the document carries its procedure")
}

/// A document whose procedure reads the VIN and decodes its field through the document.
fn decoding_job() -> Job {
    let procedure = Program {
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::DecodeField {
                service: 1,
                field: 1,
            },
        ],
        constants: vec![vec![0xF1, 0x90]],
        ..program()
    };
    document_job(&procedure)
}

/// A procedure that reads the DTCs whose status matches 0x08 and fails unless the ECU answers
/// positively with every status bit available, as the simulated ECU does
/// (`sim_ecu::DTC_STATUS_AVAILABILITY_MASK`).
const READ_DTC_PROCEDURE: &str = "\
def main():
    dtcs = diag.read_dtc(0x08)
    if not dtcs[\"ok\"]:
        diag.fail(\"NO_DTCS\", \"the ECU refused the DTC read\")
    if dtcs[\"fields\"][\"mask\"] != 0xFF:
        diag.fail(\"MASK\", \"not every status bit is available\")
";

/// The first variant's document of the example ECU (`crates/diag-frontend/examples/example-ecu`)
/// with [`READ_DTC_PROCEDURE`] as its procedure.
fn read_dtc_job() -> Job {
    use diag_frontend::convert::{lower_procedures, to_ir};
    use diag_frontend::tables::{read_files, resolve};

    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../diag-frontend/examples/example-ecu");
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .expect("example directory")
        .map(|entry| {
            let entry = entry.expect("entry");
            let name = entry.file_name().into_string().expect("UTF-8 name");
            // A Windows checkout may have CRLF line ends.
            let text = std::fs::read_to_string(entry.path())
                .expect("example file")
                .replace("\r\n", "\n");
            (name, text.into_bytes())
        })
        .collect();
    files.push((
        "read_dtc.star".to_owned(),
        READ_DTC_PROCEDURE.as_bytes().to_vec(),
    ));
    files.sort();
    let tables = read_files(files).unwrap_or_else(|e| panic!("does not read: {e:?}"));
    let definition = resolve(tables).unwrap_or_else(|e| panic!("does not resolve: {e:?}"));
    let procedures =
        lower_procedures(&definition).unwrap_or_else(|e| panic!("does not lower: {e:?}"));
    let converted = to_ir(&definition, &procedures);
    let document = converted.documents.into_iter().next().expect("a document");
    let document = JobDocument::new(document.bytes).expect("the document loads");
    let job = Job::from_document(document).expect("the document carries its procedure");
    assert!(
        job.program()
            .code
            .iter()
            .any(|op| matches!(op, Op::SendReceive { .. })),
        "diag.read_dtc lowers to the generic send-receive"
    );
    job
}

#[test]
fn agent_job_reads_the_vin_from_sim_vci() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let config_path = std::env::temp_dir().join(format!("agent-e2e-{nanos}-sim-vci.toml"));
    let config = TempFile(config_path.clone());
    std::fs::write(&config_path, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    let definition_dir = std::env::temp_dir().join(format!("agent-e2e-{nanos}-definitions"));
    // The control directory `sim-vci` takes faults from (`crates/sim-vci/docs/simulated-vci.md`),
    // and the fifth job's communication log.
    let control_dir = std::env::temp_dir().join(format!("agent-e2e-{nanos}-control"));
    let comm_log_dir = std::env::temp_dir().join(format!("agent-e2e-{nanos}-commlog"));
    for dir in [&control_dir, &comm_log_dir] {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("test directory");
    }
    let _ = std::fs::remove_dir_all(&definition_dir);
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &control_dir);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        // The simulated ECU uses its built-in configuration.
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }

    // The job's VM runs on a blocking thread that blocks on the runtime for every primitive,
    // so the runtime must be multi-threaded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(120),
                run_the_job(&control_dir, &comm_log_dir),
            )
            .await
        })
        .expect("the end-to-end flow should finish in time");
    drop(config);
    for dir in [&control_dir, &comm_log_dir] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Guards for one job, in a lock directory of this call's own: the slot is device-wide, so
/// jobs of parallel tests must not share a directory. `writes` takes the reprogramming slot.
fn job_guards(writes: bool) -> JobGuards {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let setup = GuardSetup {
        dir: std::env::temp_dir().join(format!(
            "agent-e2e-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-e2e-vci".to_owned(),
    };
    let never = AtomicBool::new(false);
    let poll = Duration::from_millis(10);
    if writes {
        JobGuards::take(&setup, poll, &never)
    } else {
        JobGuards::take_vci_only(&setup, poll, &never)
    }
    .expect("guards should be free")
}

async fn run_the_job(control_dir: &std::path::Path, comm_log_dir: &std::path::Path) {
    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            long_size: Some(long_size()),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");
    let client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");

    let job = document_job(&program());
    let JobRun {
        result: state,
        guards: _guards,
        ..
    } = run_program(
        client,
        &job.declared_link().expect("the document declares its link"),
        job,
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    let state = state.expect("the job should finish");

    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    // A negative response is a result for the procedure to inspect, not a job failure.
    assert_eq!(
        state.stack,
        [Value::Bytes(vin), Value::Bytes(vec![0x7F, 0x22, 0x31])],
        "{state:?}"
    );

    // The link of the first job is closed, so the worker can serve the next one. The agent
    // identified `sim-vci` from the module's version, which lets this job write.
    let job = document_job(&write_program());
    let JobRun {
        result: state,
        guards: _guards,
        ..
    } = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &job.declared_link().expect("the document declares its link"),
        job,
        JobLimits::default(),
        job_guards(true),
    )
    .await;
    let state = state.expect("the write job should finish");
    let [Value::Bytes(session), Value::Bytes(routine)] = state.stack.as_slice() else {
        panic!("{state:?}");
    };
    // Positive response to the session change, echoing the sub-function and the timing.
    assert!(session.starts_with(&[0x50, 0x02]), "{state:?}");
    // The routine ran in the programming session and got past the session and sub-function
    // checks; without an image transferred it ends in a request sequence error (0x24) instead
    // of the request out of range (0x31) it answers in the default session.
    assert_eq!(routine, &[0x7F, 0x31, 0x24], "{state:?}");

    // The VIN as the procedure reads it: decoded through the job's document.
    let job = decoding_job();
    let JobRun {
        result: state,
        guards: _guards,
        ..
    } = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &job.declared_link().expect("the document declares its link"),
        job,
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    let state = state.expect("the decoding job should finish");
    assert_eq!(state.stack, [Value::Bytes(VIN.to_vec())], "{state:?}");

    // DTCs read through the generic send-receive. The procedure fails unless the answer
    // matched its declared response and the mask decoded, and the job only reads. The example
    // ECU's link carries its own ComParams, which the agent sets on the link.
    let job = read_dtc_job();
    assert!(!job.writes());
    let link = job
        .declared_link()
        .expect("the example ECU declares its link");
    assert_eq!(
        link.params,
        [("CP_BlockSize".to_owned(), 0), ("CP_StMin".to_owned(), 0)]
    );
    let JobRun {
        result: state,
        guards: _guards,
        ..
    } = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &link,
        job,
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    state.expect("the DTC job should finish");

    // A job with a communication log. `sim-vci` drops the response to the F187 read, which it
    // takes from the control directory before the job's first request.
    std::fs::write(
        control_dir.join("001.tmp"),
        r#"{"command": "inject_fault", "request": [34, 241, 135], "fault": "drop_response"}"#,
    )
    .expect("the control file should be writable");
    std::fs::rename(control_dir.join("001.tmp"), control_dir.join("001.json"))
        .expect("the control file should be renamed");
    // And it answers the VIN read with two response pendings before the response, which the
    // worker passes on and the log lists with their intervals (design 8.11 `pending`, ADR-355).
    std::fs::write(
        control_dir.join("002.tmp"),
        r#"{"command": "inject_fault", "request": [34, 241, 144], "fault": {"response_pending": {"count": 2, "interval_ms": 30}}}"#,
    )
    .expect("the control file should be writable");
    std::fs::rename(control_dir.join("002.tmp"), control_dir.join("002.json"))
        .expect("the control file should be renamed");
    // And it refuses the read of 1234 with an ECU-state code, conditionsNotCorrect.
    std::fs::write(
        control_dir.join("003.tmp"),
        r#"{"command": "inject_fault", "request": [34, 18, 52], "fault": {"negative_response": {"nrc": "conditions_not_correct"}}}"#,
    )
    .expect("the control file should be writable");
    std::fs::rename(control_dir.join("003.tmp"), control_dir.join("003.json"))
        .expect("the control file should be renamed");
    let setup = CommLogSetup {
        dir: comm_log_dir.to_owned(),
        key: agent::journal::JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e72".to_owned()),
            generation: 1,
        },
    };
    let job = document_job(&comm_log_program()).with_comm_log(setup.clone());
    let JobRun {
        result: state,
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
        job_guards(false),
    )
    .await;
    assert!(
        matches!(state, Err(agent::JobError::Host { .. })),
        "the dropped response ends the job: {state:?}"
    );
    assert_eq!(comm_log, CommLogStatus::Complete);
    assert_comm_log(&setup);

    // A request longer than ISO 15765 can carry (SAE J2534-1 v04.04 clause 8.3 limits an
    // ISO15765 message to 4095 bytes of data after its CAN ID): the worker refuses it before
    // sending, naming the reason.
    let setup = CommLogSetup {
        dir: comm_log_dir.to_owned(),
        key: agent::journal::JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e73".to_owned()),
            generation: 1,
        },
    };
    let too_long = Program {
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        constants: vec![vec![0xF1; 5000]],
        ..program()
    };
    let job = document_job(&too_long).with_comm_log(setup.clone());
    let JobRun {
        result: state,
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
        job_guards(false),
    )
    .await;
    assert!(
        matches!(state, Err(agent::JobError::Host { .. })),
        "the refusal ends the job: {state:?}"
    );
    assert_eq!(comm_log, CommLogStatus::Complete);
    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    let entry: serde_json::Value = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
        .find(|line| line["event"] == "exchange")
        .unwrap_or_else(|| panic!("an exchange entry in {text}"));
    assert_eq!(entry["outcome"]["result"], "not_sent", "{entry}");
    assert_eq!(entry["outcome"]["cause"], "system_own_reasons", "{entry}");
    assert_eq!(entry["sent_evidence"], "refused", "{entry}");
    assert_eq!(entry["sent"], "not_sent", "{entry}");
    assert_eq!(entry["classification"], "system", "{entry}");
    assert_eq!(
        entry["detail"]["worker_detail"], "REFUSAL_REASON_PROTOCOL_CANNOT_CARRY",
        "{entry}"
    );

    // A response that does not answer its request (ADR-359): the ECU cuts its answer to the VIN
    // read to `62 F1`, which the worker passes on as the primitive's result and the host does
    // not take, so the entry lists it under `rejected` and the read ends with no response.
    std::fs::write(
        control_dir.join("004.tmp"),
        r#"{"command": "inject_fault", "request": [34, 241, 144], "fault": {"truncate_response": {"len": 2}}}"#,
    )
    .expect("the control file should be writable");
    std::fs::rename(control_dir.join("004.tmp"), control_dir.join("004.json"))
        .expect("the control file should be renamed");
    let setup = CommLogSetup {
        dir: comm_log_dir.to_owned(),
        key: agent::journal::JobKey {
            job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e74".to_owned()),
            generation: 1,
        },
    };
    let vin_read = Program {
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        constants: vec![vec![0xF1, 0x90]],
        ..program()
    };
    let job = document_job(&vin_read).with_comm_log(setup.clone());
    let JobRun {
        result: state,
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
        job_guards(false),
    )
    .await;
    assert!(
        matches!(state, Err(agent::JobError::Host { .. })),
        "the read gets no answer: {state:?}"
    );
    assert_eq!(comm_log, CommLogStatus::Complete);
    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    let entry: serde_json::Value = text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
        .find(|line| line["event"] == "exchange")
        .unwrap_or_else(|| panic!("an exchange entry in {text}"));
    let rejected = entry["rejected"]
        .as_array()
        .unwrap_or_else(|| panic!("{entry}"));
    assert_eq!(rejected.len(), 1, "{entry}");
    assert_eq!(rejected[0]["response"]["clear"], "62f1", "{entry}");
    assert_eq!(rejected[0]["reason"], "wrong_echo", "{entry}");
    assert_eq!(rejected[0]["rx_id"], 0x7E8, "{entry}");
    assert!(rejected[0]["agent_us"].is_u64(), "{entry}");

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

/// The fifth job's exchange entries (design 8.11 "Exchange entry"): one per request, at the
/// instruction that sent it, classified as today's evidence allows. The J2534 worker says its
/// link forwards every pending (ADR-355), so no absorbed pending stands behind an answer or a
/// silence (H4): a confirmed positive response is `none`, while no time is judged yet.
fn assert_comm_log(setup: &CommLogSetup) {
    let text = std::fs::read_to_string(setup.path().expect("the log's path")).expect("the log");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    // The link's settings come before anything is sent on it (design 8.11 "Header").
    let events: Vec<&str> = lines
        .iter()
        .map(|line| line["event"].as_str().expect("an event"))
        .collect();
    assert_eq!(
        &events[..4],
        ["header", "job_start", "vci_connected", "link_opened"],
        "{text}"
    );
    // The job was not told which API it runs on, so its version data has the D-PDU form.
    let connected = &lines[2];
    assert!(connected.get("api").is_none(), "{connected}");
    assert_eq!(connected["version"]["vendor"], "j2534-0404", "{connected}");
    let opened = &lines[3];
    assert_eq!(opened["link"], 1, "{opened}");
    assert_eq!(opened["protocol"], "ISO_14229_3_on_ISO_15765_2", "{opened}");
    assert!(opened["baud_rate"].as_u64().is_some(), "{opened}");
    let comparams = opened["comparams"].as_array().expect("the ComParams");
    for (name, set_by) in [
        ("CP_P2Max", "link"),
        ("CP_P2Star", "link"),
        ("CP_RC78Handling", "link"),
        ("CP_RC78CompletionTimeout", "link"),
        ("CP_RC21Handling", "link"),
        ("CP_RC23Handling", "link"),
        ("CP_TransmitIndEnable", "link"),
        ("CP_StartMsgIndEnable", "link"),
        ("CP_TesterPresentHandling", "library_default"),
    ] {
        assert!(
            comparams
                .iter()
                .any(|param| param["name"] == name && param["set_by"] == set_by),
            "{name} {set_by} in {opened}"
        );
    }
    // The link turns off the API's repeat on a busy answer, and the J2534 worker takes both
    // indications (design 8.11 "Link settings for the log").
    let comparam = |name: &str| {
        comparams
            .iter()
            .find(|param| param["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {opened}"))
    };
    for name in ["CP_RC21Handling", "CP_RC23Handling"] {
        assert_eq!(comparam(name)["value"], 0, "{name} in {opened}");
    }
    for name in ["CP_TransmitIndEnable", "CP_StartMsgIndEnable"] {
        assert_eq!(comparam(name)["value"], 1, "{name} in {opened}");
        assert_eq!(comparam(name)["accepted"], true, "{name} in {opened}");
    }
    // The J2534 worker says it passes on every pending (ADR-355).
    assert_eq!(opened["forwards_pendings"], true, "{opened}");
    // `sim-vci` gives a TxDone indication for each request (SAE J2534-1 v04.04 clause 8.6),
    // which the worker passes on with no primitive under `CP_TransmitIndEnable` (ADR-151).
    assert_eq!(
        opened["transmit_confirmation"],
        serde_json::json!(["transmit_indication"]),
        "{opened}"
    );
    // Each request's TxDone carries its request identifier, and no earlier exchange could own
    // it, so it is the exchange's send evidence rather than an `unattached` item (design 8.11
    // `sent_evidence`, ADR-346 item 4).
    assert_eq!(
        &events[4..events.len() - 1],
        ["exchange"].repeat(3),
        "{text}"
    );
    let entries: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["event"] == "exchange")
        .collect();
    assert_eq!(entries.len(), 3, "{text}");
    for (entry, (pc, request)) in entries
        .iter()
        .zip([(1, "22f190"), (3, "221234"), (5, "22f187")])
    {
        assert_eq!(entry["origin"], "procedure", "{entry}");
        assert_eq!(entry["link"], 1, "{entry}");
        assert_eq!(entry["target"]["link"], 1, "{entry}");
        assert_eq!(entry["at"]["pc"], pc, "{entry}");
        assert_eq!(entry["at"]["steps"], pc, "{entry}");
        assert_eq!(entry["request"]["sid"], "0x22", "{entry}");
        assert_eq!(
            entry["request"]["service"], "ReadDataByIdentifier",
            "{entry}"
        );
        assert_eq!(entry["request"]["bytes"]["clear"], request, "{entry}");
        assert_eq!(entry["pending_visibility"], "forwarded", "{entry}");
        assert_eq!(entry["timing"]["window"], "not_judged", "{entry}");
        assert!(
            entry["times"]["agent"]["end_us"].as_u64()
                >= entry["times"]["agent"]["before_us"].as_u64()
        );
        assert!(
            entry["text"].as_str().is_some_and(|text| !text.is_empty()),
            "{entry}"
        );
    }
    // Sequence numbers rise from line to line.
    let seqs: Vec<u64> = lines
        .iter()
        .map(|line| line["seq"].as_u64().expect("a seq"))
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{seqs:?}"
    );

    let (vin, unknown, dropped) = (entries[0], entries[1], entries[2]);
    assert_eq!(vin["outcome"]["result"], "positive", "{vin}");
    assert_eq!(vin["sent"], "confirmed", "{vin}");
    assert_eq!(vin["sent_evidence"], "tx_indication", "{vin}");
    // The worker forwards every pending, so none can have been absorbed (H4, ADR-355), and a
    // positive response confirmed on the bus leaves nothing open.
    assert_eq!(vin["classification"], "none", "{vin}");
    // Its pendings are listed, with the VCI time on the device clock, the agent's read time and
    // the interval from the transmit indication or the previous pending, on the device clock.
    let pending = vin["pending"].as_array().expect("a pending list");
    assert_eq!(pending.len(), 2, "{vin}");
    for (index, item) in pending.iter().enumerate() {
        assert!(item["vci_us"].is_u64(), "{vin}");
        assert_eq!(item["clock"], "device", "{vin}");
        assert!(item["agent_us"].is_u64(), "{vin}");
        let interval = item["since_previous_ms"]
            .as_f64()
            .unwrap_or_else(|| panic!("pending {index} has an interval: {vin}"));
        assert!(interval >= 0.0, "{vin}");
    }
    // The second follows the first by the injected interval: sim-vci stamps each pending from
    // its scheduled time, so the difference is exact whatever the runner's load.
    assert!(
        (29.9..=30.1).contains(&pending[1]["since_previous_ms"].as_f64().unwrap()),
        "{vin}"
    );
    assert_eq!(vin["link_history"]["this_exchange"], true, "{vin}");
    // Its VCI times, each with its own clock (design 8.11 `times`, ADR-353): the transmit
    // indication and the response's end are frames, on the device clock.
    let vci = &vin["times"]["vci"];
    for time in ["tx_done", "rx_end"] {
        assert_eq!(vci[time]["clock"], "device", "{vin}");
        assert_eq!(vci[time]["epoch"], 0, "{vin}");
    }
    let tx_done = vci["tx_done"]["us"].as_u64().expect("a transmit done");
    assert!(vci["rx_end"]["us"].as_u64().unwrap() >= tx_done, "{vin}");
    // The first pending's interval runs from that transmit done.
    let first = pending[0]["vci_us"].as_u64().unwrap() - tx_done;
    assert!(
        (pending[0]["since_previous_ms"].as_f64().unwrap() - first as f64 / 1000.0).abs() < 1e-9,
        "{vin}"
    );

    // The ECU's negative response, with its code named.
    assert_eq!(unknown["outcome"]["result"], "negative", "{unknown}");
    assert_eq!(unknown["outcome"]["nrc"], "0x22", "{unknown}");
    assert_eq!(
        unknown["outcome"]["nrc_name"], "conditionsNotCorrect",
        "{unknown}"
    );
    assert_eq!(
        unknown["outcome"]["response"]["clear"], "7f2222",
        "{unknown}"
    );
    // An ECU-state code says the vehicle refused it, and the reason names the code.
    assert_eq!(unknown["classification"], "vehicle", "{unknown}");
    let unknown_reason = unknown["reason"].as_str().expect("a reason");
    assert!(
        unknown_reason.contains("conditionsNotCorrect"),
        "{unknown_reason}"
    );
    assert_eq!(unknown["link_history"]["answered"], 1, "{unknown}");

    // The dropped response: its TxDone shows the request reached the bus, so the no-response
    // rows judge it by what else is open, not by the send (H1). The link forwards every
    // pending, so no absorbed pending explains it (H4, ADR-355); no answer was matched this
    // way, so a dropped response is (H2).
    assert_eq!(dropped["outcome"]["result"], "no_response", "{dropped}");
    assert_eq!(dropped["sent"], "confirmed", "{dropped}");
    assert_eq!(dropped["sent_evidence"], "tx_indication", "{dropped}");
    assert_eq!(dropped["classification"], "undetermined", "{dropped}");
    assert_eq!(dropped["detail"]["host_error"], "NoResponse", "{dropped}");
    let reason = dropped["reason"].as_str().expect("a reason");
    assert!(
        reason.contains("(H2)") && !reason.contains("(H4)") && !reason.contains("(H1)"),
        "{reason}"
    );
    assert_eq!(dropped["link_history"]["answered"], 2, "{dropped}");
    // The transmit indication is a frame, on the device clock; the receive timeout is the J2534
    // worker's own event, on its clock (design 8.11 "On one clock").
    let vci = &dropped["times"]["vci"];
    assert_eq!(vci["tx_done"]["clock"], "device", "{dropped}");
    assert_eq!(vci["error"]["clock"], "worker", "{dropped}");
    assert!(vci["error"]["us"].is_u64(), "{dropped}");
    assert!(vci.get("rx_end").is_none(), "{dropped}");

    assert_eq!(lines.last().expect("a line")["event"], "job_end", "{text}");
}

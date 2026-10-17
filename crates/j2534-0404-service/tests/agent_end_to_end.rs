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
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it sets for the
//! spawned service cannot race with another test.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{Job, JobDocument, JobLimits, LinkConfig, run_program};
use diag_ir::declaration::{
    DataKind, EcuDocumentT, EcuVariantT, FieldT, Placement, PositiveResponseT, ServiceT,
    encode_ecu_document,
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

/// A variant's document with one service, 1: ReadDataByIdentifier of the VIN, whose positive
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
    let mut variant = EcuVariantT::default();
    variant.id = 1;
    variant.short_name = Some("sim".into());
    variant.services = Some(vec![service]);
    let mut document = EcuDocumentT::default();
    document.schema_version = IR_SCHEMA_VERSION;
    document.source_format = Some("csv-starlark".into());
    document.source_digest = Some("00".into());
    document.variant = Some(Box::new(variant));
    document.procedure = Some(postcard::to_allocvec(procedure).expect("program should serialize"));
    encode_ecu_document(&document)
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
    let document = JobDocument::new(vin_document(&procedure)).expect("the document loads");
    Job::from_document(document).expect("the document carries its procedure")
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
    let _ = std::fs::remove_dir_all(&definition_dir);
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
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
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), run_the_job()).await })
        .expect("the end-to-end flow should finish in time");
    drop(config);
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

async fn run_the_job() {
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

    let (state, _guards) = run_program(
        client,
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        program(),
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
    let (state, _guards) = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        write_program(),
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
    let (state, _guards) = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        decoding_job(),
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    let state = state.expect("the decoding job should finish");
    assert_eq!(state.stack, [Value::Bytes(VIN.to_vec())], "{state:?}");

    // DTCs read through the generic send-receive. The procedure fails unless the answer
    // matched its declared response and the mask decoded, and the job only reads.
    let job = read_dtc_job();
    assert!(!job.writes());
    let (state, _guards) = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        job,
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    state.expect("the DTC job should finish");

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

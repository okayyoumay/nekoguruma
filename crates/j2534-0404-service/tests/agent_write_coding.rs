// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! The fictional vehicle's engine-controller coding change (`write-coding.star`, README 11 of
//! `crates/diag-frontend/examples/fictional-vehicle`) as a configuration write job: the agent
//! runs it through the real `j2534-0404-service` binary against `sim-vci`, whose ECU is
//! configured from `sim/ecm.json` (a write needs the extended session, security level 1 and the
//! gateway's authentication).
//!
//! The job runs four times against one simulated ECU. First the gateway has not authenticated:
//! the write is refused, the procedure reads the coding, finds it unchanged and ends as
//! `WRITE_REFUSED`. Then the test sends `set_gateway_authentication` through the control
//! directory and arms a refusal of the second read of the coding (`occurrence` of `inject_fault`):
//! the write is accepted but its read-back is refused, so the procedure writes the saved coding
//! back, reads it again and ends as `CODING_ROLLED_BACK` with the coding as it was. The next run
//! succeeds, leaving the coding `00 03 03 20` after the session request, the seed request, the
//! key request and one write, in that order in the request trace. A last run finds the
//! cruise-control bit set and sends nothing but the read.
//!
//! The dry runs in `crates/diag-frontend/tests/fictional_vehicle.rs` cover the other roll-back
//! outcomes (refused or unconfirmed roll-back writes).
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::inputs::ServiceSources;
use agent::journal::JobKey;
use agent::launch::Api;
use agent::{
    Job, JobDocument, JobError, JobLimits, JobRun, JournalSetup, LinkConfig, VciFacts, run_program,
    run_program_journaled,
};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";

#[path = "common/artifacts.rs"]
mod artifacts;

use artifacts::sim_vci_path;

#[path = "common/fictional_ecm.rs"]
mod fictional_ecm;

use fictional_ecm::{coding_document, send, vehicle};

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>() as u8
    }
}

/// Removes the test's temporary directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads the coding DID: the response, starting with its SID, is left on the stack.
fn read_coding() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        constants: vec![vec![0x01, 0x20]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// Guards for one job, in a lock directory of this call's own. `writes` takes the reprogramming
/// slot.
fn job_guards(writes: bool) -> JobGuards {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let setup = GuardSetup {
        dir: std::env::temp_dir().join(format!(
            "agent-write-coding-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-write-coding-vci".to_owned(),
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

fn vci_facts() -> VciFacts {
    VciFacts {
        api: Api::J2534,
        long_size: long_size(),
    }
}

/// The coding as the ECU reads it out now.
async fn coding(worker: &WorkerProcess) -> Vec<u8> {
    let client: WorkerClient = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program(
        client,
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        read_coding(),
        JobLimits::default(),
        job_guards(false),
    )
    .await;
    match result.expect("the read job finishes").stack.last() {
        Some(Value::Bytes(bytes)) => bytes.clone(),
        other => panic!("the read left {other:?}"),
    }
}

/// Runs the coding procedure as a journaled job; `n` makes its journal key.
async fn change_coding(
    worker: &WorkerProcess,
    journal_dir: &Path,
    n: u8,
) -> Result<diag_ir::VmState, JobError> {
    let job = Job::from_document(JobDocument::new(coding_document()).expect("the document loads"))
        .expect("the document carries its procedure")
        .with_vci(vci_facts());
    let link = job.declared_link().expect("the document declares its link");
    assert!(job.writes(), "the procedure writes");
    let client: WorkerClient = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    let JobRun {
        result,
        guards: _guards,
        ..
    } = run_program_journaled(
        client,
        &link,
        job,
        JobLimits::default(),
        JournalSetup {
            dir: journal_dir.to_owned(),
            key: JobKey {
                job_id: shared_proto::JobId(format!("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e{n:02}")),
                generation: 1,
            },
            sources: ServiceSources::default(),
            vin: None,
            intended_software_version: None,
        },
        job_guards(true),
        None,
    )
    .await;
    result
}

/// The requests the worker has sent since the last call, without the tester presents it sends on
/// its own. `seen` counts the lines already returned.
fn requests_since(trace: &Path, seen: &mut usize) -> Vec<String> {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let new = lines[*seen..]
        .iter()
        .filter(|line| !line.starts_with("uds 3E"))
        .cloned()
        .collect();
    *seen = lines.len();
    new
}

async fn run_the_jobs(dir: &Path) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            long_size: Some(long_size()),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");
    let trace = dir.join("trace.txt");
    let control_dir = dir.join("control");
    let journal_dir = dir.join("journal");
    let mut seen = 0;

    // The ECU as the simulation file declares it.
    assert_eq!(
        coding(&worker).await,
        [0x62, 0x01, 0x20, 0x00, 0x01, 0x03, 0x20]
    );
    requests_since(&trace, &mut seen);

    // R1: the gateway has not authenticated, so the ECU refuses the write; the coding reads as
    // saved, so the procedure ends as refused without a roll-back write. Nothing observable
    // carries the refusal's code: the trace lists requests only and the failure's message is the
    // procedure's fixed text, so the test cannot tell NRC 0x34 from another refusal; the dry-run
    // tests cover each NRC.
    let refused = change_coding(&worker, &journal_dir, 1).await;
    match refused {
        Err(JobError::ProcedureFailed { code, .. }) => assert_eq!(code, "WRITE_REFUSED"),
        other => panic!("the first run did not fail as refused: {other:?}"),
    }
    assert_eq!(
        requests_since(&trace, &mut seen),
        [
            "uds 22 0120",
            "uds 10 03",
            "uds 27",
            "uds 27",
            "uds 2E 0120",
            "uds 22 0120"
        ]
    );
    assert_eq!(
        coding(&worker).await,
        [0x62, 0x01, 0x20, 0x00, 0x01, 0x03, 0x20]
    );
    requests_since(&trace, &mut seen);

    // R1b: authenticated, the write is accepted but the read-back, the second read of the coding
    // since the fault was armed (the job's first read is the first), is refused. The procedure
    // writes the saved coding back, checks it with a third read and reports the roll-back.
    send(
        &control_dir,
        "authenticate",
        r#"{"command": "set_gateway_authentication", "authenticated": true}"#,
    );
    send(
        &control_dir,
        "refuse-read-back",
        r#"{"command": "inject_fault", "fault": {"negative_response": {"nrc": "conditions_not_correct"}}, "request": [34, 1, 32], "occurrence": 2}"#,
    );
    let rolled_back = change_coding(&worker, &journal_dir, 4).await;
    match rolled_back {
        Err(JobError::ProcedureFailed { code, .. }) => assert_eq!(code, "CODING_ROLLED_BACK"),
        other => panic!("the run with a refused read-back did not roll back: {other:?}"),
    }
    assert_eq!(
        requests_since(&trace, &mut seen),
        [
            "uds 22 0120",
            "uds 10 03",
            "uds 27",
            "uds 27",
            "uds 2E 0120",
            "uds 22 0120",
            "uds 2E 0120",
            "uds 22 0120"
        ]
    );
    assert_eq!(
        coding(&worker).await,
        [0x62, 0x01, 0x20, 0x00, 0x01, 0x03, 0x20]
    );
    requests_since(&trace, &mut seen);

    // R2: the whole procedure runs and the coding gets the cruise-control bit.
    change_coding(&worker, &journal_dir, 2)
        .await
        .expect("the third run finishes");
    // The trace lists a sub-function for 0x10 but not for 0x27, so the seed and the key
    // requests show as two `uds 27` lines.
    assert_eq!(
        requests_since(&trace, &mut seen),
        [
            "uds 22 0120",
            "uds 10 03",
            "uds 27",
            "uds 27",
            "uds 2E 0120",
            "uds 22 0120"
        ]
    );
    assert_eq!(
        coding(&worker).await,
        [0x62, 0x01, 0x20, 0x00, 0x03, 0x03, 0x20]
    );
    requests_since(&trace, &mut seen);

    // R3: the bit is set, so nothing but the read is sent.
    change_coding(&worker, &journal_dir, 3)
        .await
        .expect("the fourth run finishes");
    assert_eq!(requests_since(&trace, &mut seen), ["uds 22 0120"]);
    assert_eq!(
        coding(&worker).await,
        [0x62, 0x01, 0x20, 0x00, 0x03, 0x03, 0x20]
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

#[test]
#[expect(
    unsafe_code,
    reason = "`std::env::set_var`/`remove_var` are unsafe (edition 2024) and have no safe form (ADR-502)"
)]
fn the_coding_procedure_runs_as_a_write_job_against_the_simulators() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let root = TempDir(
        std::env::temp_dir().join(format!("agent-write-coding-{}-{nanos}", std::process::id())),
    );
    let definition_dir = root.0.join("definitions");
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
    std::fs::create_dir_all(root.0.join("control")).expect("control directory");
    std::fs::create_dir_all(root.0.join("journal")).expect("journal directory");
    let config_path = root.0.join("sim-vci.toml");
    std::fs::write(&config_path, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    j2534_0404_mock::write_definition(&definition_dir, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definition_dir);
        std::env::set_var("NGR_SIM_ECU_CONFIG", vehicle().join("sim/ecm.json"));
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", root.0.join("control"));
        std::env::set_var("NGR_SIM_VCI_TRACE", root.0.join("trace.txt"));
        // A state file would replace the configuration.
        std::env::remove_var("NGR_SIM_ECU_STATE");
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
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
            tokio::time::timeout(Duration::from_secs(120), run_the_jobs(&root.0)).await
        })
        .expect("the flow should finish in time");
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! The agent's job intake end to end without hardware (design 5.3, ADR-511): a verified
//! instruction the control channel would hand over is taken by [`agent::intake`], which fetches
//! its document from an object source, finds the VCI `sim-vci` through the registration
//! definition, launches the real `j2534-0404-service` for its ABI and runs the document's
//! procedure on the simulated ECU. The test then looks at what the intake left for the server:
//! the job's events in the outbox (`running`, then `completed`, one job after the other), its
//! result in the upload ledger, its communication log at the path a `logUploadCommand` reads, and
//! the outcome record's `reported`.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::JobDocument;
use agent::comm_log::comm_log_path;
use agent::control::{Intake, SharedOutbox};
use agent::deadline::{IssueMark, SystemClock};
use agent::fetch::{FetchError, Fetched, Kind as ObjectKind};
use agent::identification::NoPlans;
use agent::instruction::{self, VerifiedInstruction};
use agent::intake::{self, ObjectFuture, Objects, Parts};
use agent::journal::JobKey;
use agent::launch::Api;
use agent::log_upload::{LogUploader, LogUploads};
use agent::outbox::Outbox;
use agent::outcome_record::OutcomeRecord;
use agent::received_jobs::ReceivedJobs;
use agent::upload_ledger::{Kind as UploadKind, UploadLedger};
use agent::uploader::SharedLedger;
use agent::vci_shell::{VciLookup, VciRoute, VciShellError};
use diag_ir::{Value, VmState};
use shared_crypto::{Signer, SigningKey, instruction as envelope};
use shared_proto::{
    AgentId, INSTRUCTION_VERSION, JobId, JobInstruction, JobKind, LogUploadOutcome, VciId,
};
use worker_host::service::OperatingMode;

const LIBRARY_NAME: &str = "sim-vci";
const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// The longest the test waits for the whole flow.
const FLOW_TIMEOUT: Duration = Duration::from_secs(180);

#[path = "common/artifacts.rs"]
mod artifacts;

use artifacts::sim_vci_path;

#[path = "common/sim_worker.rs"]
mod sim_worker;

use sim_worker::{document, install_worker as install_service, program};

/// Installs the service binary of this package as `<workers>/<ABI name>/j2534-0404-service`.
fn install_worker(workers: &Path, abi_name: &str) {
    install_service(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        workers,
        abi_name,
    );
}

/// The object source: the document of the test, which every fetch has in its cache.
struct Served(HashMap<[u8; 32], Vec<u8>>);

impl Objects for Served {
    fn fetch_all<'a>(
        &'a self,
        instruction: &'a VerifiedInstruction,
        _attempts: u32,
    ) -> ObjectFuture<'a, Vec<Fetched>> {
        let objects = instruction
            .instruction()
            .document_hashes
            .iter()
            .map(|_| Fetched {
                kind: ObjectKind::Document,
                path: PathBuf::from("cache"),
                length: 1,
            })
            .collect();
        Box::pin(std::future::ready(Ok(objects)))
    }

    fn read_document<'a>(
        &'a self,
        _instruction: &'a VerifiedInstruction,
        digest: &'a [u8; 32],
        _attempts: u32,
    ) -> ObjectFuture<'a, Vec<u8>> {
        Box::pin(std::future::ready(
            self.0
                .get(digest)
                .cloned()
                .ok_or(FetchError::Local { why: "not served" }),
        ))
    }
}

/// The VCI of the simulator, whose worker the test installed.
struct SimVci {
    workers: PathBuf,
    locks: PathBuf,
}

impl VciLookup for SimVci {
    fn find(&self, vci: &str) -> Result<VciRoute, VciShellError> {
        assert_eq!(vci, LIBRARY_NAME, "the instruction names the simulator");
        Ok(VciRoute {
            api: Api::J2534,
            workers: self.workers.clone(),
            locks: self.locks.clone(),
        })
    }
}

fn instruction_for(
    key: &SigningKey,
    job: u32,
    document: [u8; 32],
    deadline: u64,
) -> VerifiedInstruction {
    let body = JobInstruction {
        version: INSTRUCTION_VERSION,
        job_id: JobId(format!("0194f0a2-7c3e-7d11-8a52-{job:012x}")),
        ownership_generation: 1,
        agent_id: AgentId(AGENT_ID.to_owned()),
        operator_id: "operator".to_owned(),
        kind: JobKind::Acquire,
        vci: VciId(LIBRARY_NAME.to_owned()),
        vin: None,
        document_hashes: vec![document],
        artifact_hash: None,
        plan_hash: None,
        issued_at: 1,
        start_deadline: deadline,
    };
    let blob = envelope::seal(key, &body.encode().expect("the body encodes")).expect("sealed");
    instruction::open(&blob, &key.public_key(), AGENT_ID).expect("the instruction verifies")
}

fn job_key(job: u32) -> JobKey {
    JobKey {
        job_id: JobId(format!("0194f0a2-7c3e-7d11-8a52-{job:012x}")),
        generation: 1,
    }
}

#[test]
#[expect(
    unsafe_code,
    reason = "`std::env::set_var`/`remove_var` are unsafe (edition 2024) and have no safe form (ADR-502)"
)]
fn a_taken_instruction_runs_its_job_on_the_simulator_and_leaves_its_events_result_and_log() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let root = tempfile::tempdir().expect("temporary directory");
    let temp = std::env::temp_dir();
    let pid = std::process::id();
    let config = temp.join(format!("sim-vci-intake-{pid}-{nanos}.toml"));
    let definitions = temp.join(format!("sim-vci-intake-definitions-{pid}-{nanos}"));
    std::fs::write(&config, "").expect("test config file should be writable");
    std::fs::create_dir_all(&definitions).expect("definition directory");
    let sim_vci = sim_vci_path();
    j2534_0404_mock::write_definition(&definitions, LIBRARY_NAME, &sim_vci)
        .expect("definition file should be writable");
    let abi = worker_host::abi::detect_file(&sim_vci).expect("sim-vci should have a known ABI");
    install_worker(&root.path().join("workers"), abi.name());
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &definitions);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
        std::env::remove_var("NGR_SIM_ECU_STATE");
        std::env::remove_var("NGR_SIM_VCI_CONTROL_DIR");
    }
    // A job's thread blocks on the runtime for every primitive, so it must be multi-threaded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(async { tokio::time::timeout(FLOW_TIMEOUT, flow(root.path())).await })
        .expect("the end-to-end flow should finish in time");
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_dir_all(&definitions);
}

async fn flow(root: &Path) {
    let dirs = ["outbox", "uploads", "registration", "journal", "locks"].map(|name| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("state directory");
        dir
    });
    let [outbox_dir, uploads_dir, registration, journal, locks] = dirs;
    let bytes = document(&program());
    let hash = JobDocument::new(bytes.clone())
        .expect("the document loads")
        .sha256();
    let key = SigningKey::generate().expect("a key");
    let now = device_now();

    let outbox = SharedOutbox::new(Outbox::open_in(&outbox_dir, AGENT_ID).expect("outbox"));
    let uploads =
        SharedLedger::new(UploadLedger::open_in(&uploads_dir, AGENT_ID).expect("upload ledger"));
    let received = ReceivedJobs::open_in(&registration, AGENT_ID).expect("received jobs");
    let outcomes = OutcomeRecord::open_in(&registration, AGENT_ID).expect("outcome record");
    let (intake, handle) = intake::start(Parts {
        objects: Arc::new(Served(HashMap::from([(hash, bytes)]))),
        plans: Arc::new(NoPlans),
        vcis: Arc::new(SimVci {
            workers: root.join("workers"),
            locks,
        }),
        mode: OperatingMode::User,
        journal_dir: {
            let journal = journal.clone();
            Arc::new(move || Ok(journal.clone()))
        },
        outbox,
        uploads: uploads.clone(),
        marks: IssueMark::open_in(&registration, AGENT_ID).expect("marks"),
        clock: Arc::new(SystemClock),
        outcomes: outcomes.clone(),
    });

    // What the control channel does for each command: the ledger's record, the hand-over, and
    // (once the `received` event is appended) the word that the job may be served.
    for job in [1, 2] {
        let verified = instruction_for(&key, job, hash, now + 3_600);
        received
            .record(&job_key(job).job_id.0, 1, verified.envelope())
            .expect("the ledger records the job");
        intake
            .take(verified)
            .await
            .expect("the intake takes the job");
        intake.received(&job_key(job).job_id.0, 1);
    }
    // Both ended and reported: the second only starts when the first is over.
    for job in [1, 2] {
        wait_until(|| {
            outcomes
                .standing(&job_key(job))
                .is_some_and(|(_, reported)| reported)
        })
        .await;
    }

    // The log of a job the intake ran is where a `logUploadCommand` looks, and is queued.
    let log_uploads = LogUploader::in_dir(journal.clone(), received.clone(), uploads.clone());
    for job in [1, 2] {
        let answer = log_uploads
            .handle(
                job_key(job).job_id.0.clone(),
                1,
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .expect("the log request is answered");
        assert_eq!(answer, LogUploadOutcome::Queued, "job {job}");
        assert!(
            comm_log_path(&journal, &job_key(job))
                .expect("a path")
                .is_file(),
            "job {job}'s log is at the path the answer reads"
        );
    }
    handle.stop();
    handle.stopped().await;
    drop((intake, handle, uploads, log_uploads, outcomes, received));

    // What the server would be sent: running, then completed, job after job.
    let reopened = Outbox::open_in(&outbox_dir, AGENT_ID).expect("outbox reopens");
    let events: Vec<(String, String)> = reopened
        .pending()
        .map(|(_, text)| {
            let value: serde_json::Value = serde_json::from_str(text).expect("event text");
            (
                value["jobId"].as_str().expect("job").to_owned(),
                value["state"].as_str().expect("state").to_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = [1, 2]
        .into_iter()
        .flat_map(|job| {
            ["running", "completed"].map(|state| (job_key(job).job_id.0, state.to_owned()))
        })
        .collect();
    assert_eq!(events, expected);

    // The results: the final state of each run, in the upload ledger.
    let ledger = UploadLedger::open_in(&uploads_dir, AGENT_ID).expect("upload ledger reopens");
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    for job in [1, 2] {
        let entry = ledger
            .pending()
            .find(|entry| entry.kind == UploadKind::Result && entry.key == job_key(job))
            .unwrap_or_else(|| panic!("job {job} has a result in the upload ledger"));
        let state: VmState = serde_json::from_slice(
            &std::fs::read(ledger.object_path(entry)).expect("the result is stored"),
        )
        .expect("the result is the final state");
        assert_eq!(
            state.stack,
            [
                Value::Bytes(vin.clone()),
                Value::Bytes(vec![0x7F, 0x22, 0x31])
            ],
            "job {job}"
        );
        assert!(
            ledger
                .pending()
                .any(|entry| entry.kind == UploadKind::CommLog && entry.key == job_key(job)),
            "job {job}'s log is queued"
        );
    }
}

/// The device clock now, in Unix seconds.
fn device_now() -> u64 {
    use agent::deadline::Clock;
    SystemClock.now_unix()
}

/// Waits for `condition`, polling, within the flow's own timeout.
async fn wait_until(condition: impl Fn() -> bool) {
    while !condition() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

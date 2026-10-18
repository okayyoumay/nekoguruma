// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! One ECU's CSV + Starlark definition end to end through the j2534-0404 worker (M2 exit
//! criterion 1). The example ECU converts to one document per variant; the variant is identified
//! against the simulated ECU behind `sim-vci` from its software version, and its read procedure
//! (identification DIDs, the active session under a text table, a battery voltage and a boot time
//! under linear conversions, the confirmed DTCs) and its
//! routine procedure (the programming session's P2* through a linear conversion, then a routine
//! the ECU refuses for want of an image) run as agent jobs.
//! Each procedure checks every value it decodes and fails on the first that differs, so a
//! finished job means the decoded results are the expected ones. The D-PDU worker runs the same
//! procedures in `crates/iso22900-service/tests/ngr_agent_definition_end_to_end.rs`.
//!
//! This file holds a single test, so the process-wide variables it sets for the spawned service
//! cannot race with another test.

#[path = "../../diag-frontend/tests/support/definition_end_to_end.rs"]
mod definition;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{Job, JobDocument, JobLimits, run_program};
use diag_ir::VmState;
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";

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

/// Removes the test's temporary directory however the test ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_definition_runs_end_to_end_through_the_j2534_worker() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let root = TempDir(std::env::temp_dir().join(format!("definition-e2e-{nanos}")));
    let definition_dir = root.0.join("definitions");
    std::fs::create_dir_all(&definition_dir).expect("definition directory");
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
        std::env::set_var("NGR_SIM_ECU_CONFIG", definition::ecu_config());
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
            tokio::time::timeout(Duration::from_secs(120), run_the_definition(&root.0)).await
        })
        .expect("the end-to-end flow should finish in time");
}

/// Guards for one job in `dir`; `writes` takes the reprogramming slot.
fn job_guards(dir: &Path, writes: bool) -> JobGuards {
    let setup = GuardSetup {
        dir: dir.join("locks"),
        vci: "definition-e2e-vci".to_owned(),
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

/// Runs `document`'s procedure on the link it declares and returns the final state.
async fn run(worker: &WorkerProcess, dir: &Path, document: Vec<u8>) -> VmState {
    let job = Job::from_document(JobDocument::new(document).expect("the document loads"))
        .expect("the document carries its procedure");
    let link = job.declared_link().expect("the document declares its link");
    let writes = job.writes();
    let client: WorkerClient = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    let (state, _guards) = run_program(
        client,
        &link,
        job,
        JobLimits::default(),
        job_guards(dir, writes),
    )
    .await;
    state.expect("the job should finish")
}

async fn run_the_definition(dir: &Path) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
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

    // Identify the variant: each variant's identification services, sent to the ECU.
    let reads = definition::documents("read.star");
    let mut runs = Vec::new();
    for (_, document) in &reads {
        let (identification, services) = definition::identification(document);
        runs.push((services, run(&worker, dir, identification).await));
    }
    let variant = definition::identified(&reads, &runs).expect("a variant is identified");
    // The simulated ECU reports software version 1.0.0, the first generation's.
    assert_eq!(reads[variant].0, "exampleEcuV1");

    // The read procedure checks every value it decodes.
    let state = run(&worker, dir, reads[variant].1.clone()).await;
    assert!(state.call_stack.is_empty(), "{state:?}");

    // The routine changes the session, so it runs last.
    let routines = definition::documents("routine.star");
    assert_eq!(routines[variant].0, reads[variant].0);
    let state = run(&worker, dir, routines[variant].1.clone()).await;
    assert!(state.call_stack.is_empty(), "{state:?}");

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

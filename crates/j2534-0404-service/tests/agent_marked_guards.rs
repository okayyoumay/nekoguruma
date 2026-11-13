// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! A close that stays unconfirmed keeps the job's guards marked until the worker is reaped
//! (ADR-258, ADR-422): `worker-host` launches the real `j2534-0404-service` binary against the
//! `sim-vci` cdylib, a job runs a program that waits between two requests, and this test unplugs
//! the simulated VCI during the wait and makes the library's close fail (`fail_closes`).
//!
//! First flow: both closes of the lost device fail, so the worker keeps its device slot (ADR-422)
//! and the job's guards come back marked. A second taker of the same VCI lock blocks for as long
//! as the marked guards live; `JobGuards::worker_gone`, once the worker has been stopped, lets
//! it through, and the next job on a fresh worker reads the VIN. Second flow: only the first
//! close fails and the second is answered as the device being gone, which the worker counts as
//! closed, so the guards come back unmarked (ADR-393's retry, end to end).
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{Job, JobError, JobLimits, JobRun, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// Instructions of [`waiting_program`]: the second request is the last one.
const SECOND_REQUEST_PC: u32 = 5;

#[path = "common/artifacts.rs"]
mod artifacts;

use artifacts::sim_vci_path;

/// Width of J2534 `unsigned long` in `sim-vci` on this platform.
fn long_size() -> u8 {
    if cfg!(windows) {
        4
    } else {
        std::mem::size_of::<std::os::raw::c_ulong>() as u8
    }
}

/// Removes the temporary files however the test ends.
struct TempPaths {
    config: PathBuf,
    control_dir: PathBuf,
    definitions: PathBuf,
    ecu_state: PathBuf,
    locks: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_file(&self.ecu_state);
        let _ = std::fs::remove_dir_all(&self.control_dir);
        let _ = std::fs::remove_dir_all(&self.definitions);
        let _ = std::fs::remove_dir_all(&self.locks);
    }
}

/// Writes `command` as the control file `name`, under a temporary name first so `sim-vci`
/// never reads it half-written. It takes effect at the next J2534 call.
fn send(control_dir: &Path, name: &str, command: &str) {
    let tmp = control_dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, command).expect("control file should be writable");
    std::fs::rename(&tmp, control_dir.join(format!("{name}.json")))
        .expect("control file should be renamed");
}

fn program(code: Vec<Op>) -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
        targets: Vec::new(),
    }
}

/// Reads the VIN, waits 3 s, and reads it again.
fn waiting_program() -> Program {
    program(vec![
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x22 },
        Op::Pop,
        Op::Wait {
            millis: WAIT.as_millis() as u32,
        },
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x22 },
    ])
}

fn read_vin() -> Program {
    program(vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }])
}

fn link_config() -> LinkConfig {
    LinkConfig::iso15765(0x7E0, 0x7E8)
}

fn launch_options() -> LaunchOptions {
    LaunchOptions {
        // Generous timeouts for a debug binary on a busy CI runner.
        startup_timeout: Duration::from_secs(20),
        request_timeout: Duration::from_secs(5),
        long_size: Some(long_size()),
        ..LaunchOptions::default()
    }
}

async fn launch_worker() -> (WorkerProcess, worker_host::client::WorkerClient) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        OperatingMode::User,
        &launch_options(),
    )
    .await
    .expect("worker should launch");
    let client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");
    (worker, client)
}

fn take_guards(setup: &GuardSetup) -> JobGuards {
    JobGuards::take_vci_only(setup, Duration::from_millis(10), &AtomicBool::new(false))
        .expect("guards should be taken")
}

fn temp_paths() -> TempPaths {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let temp = std::env::temp_dir();
    TempPaths {
        config: temp.join(format!("sim-vci-marked-{pid}-{nanos}.toml")),
        control_dir: temp.join(format!("sim-vci-marked-{pid}-{nanos}")),
        definitions: temp.join(format!("sim-vci-marked-definitions-{pid}-{nanos}")),
        ecu_state: temp.join(format!("sim-vci-marked-{pid}-{nanos}.ecu")),
        locks: temp.join(format!("sim-vci-marked-locks-{pid}-{nanos}")),
    }
}

#[test]
fn marked_guards_are_kept_until_the_worker_is_gone_and_a_retried_close_confirms() {
    let paths = temp_paths();
    std::fs::write(&paths.config, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    std::fs::create_dir_all(&paths.definitions).expect("definition directory");
    j2534_0404_mock::write_definition(&paths.definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    std::fs::create_dir(&paths.control_dir).expect("control directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &paths.config);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &paths.definitions);
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &paths.control_dir);
        // The ECU's state file appears when the job's open creates the ECU, which tells this
        // test when the job has started.
        std::env::set_var("NGR_SIM_ECU_STATE", &paths.ecu_state);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        // The simulated ECU uses its built-in configuration.
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }
    let setup = GuardSetup {
        dir: paths.locks.clone(),
        vci: "sim-marked-vci".to_owned(),
    };

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
                Duration::from_secs(180),
                flow(&paths.control_dir, &paths.ecu_state, setup),
            )
            .await
        })
        .expect("the end-to-end flow should finish in time");
    drop(paths);
}

/// How long the program waits between its two requests.
const WAIT: Duration = Duration::from_secs(6);
/// How long after the ECU exists the VCI is unplugged: long enough for the rest of the open and
/// the first request, well inside [`WAIT`].
const UNPLUG_AFTER: Duration = Duration::from_secs(2);
/// How long a taker of a held VCI lock is given to show that it stays blocked.
const STAYS_BLOCKED: Duration = Duration::from_millis(1500);

/// Waits until the job's open has created the ECU (`sim-vci` writes its state file then), and
/// [`UNPLUG_AFTER`] more. The file is also rewritten on every request, so the caller removes it
/// before each job starts.
async fn wait_for_first_request(ecu_state: &Path) {
    while !ecu_state.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(UNPLUG_AFTER).await;
}

/// Cancels a blocked guard taker however the test ends, so a failed assertion does not leave the
/// runtime waiting for its blocking thread.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Runs the waiting program on a fresh worker, unplugs the VCI during the wait and makes the
/// next `closes` closes fail. Returns the worker and the finished job.
async fn lost_job(
    control_dir: &Path,
    ecu_state: &Path,
    setup: &GuardSetup,
    closes: u32,
    name: &str,
) -> (WorkerProcess, JobRun) {
    let _ = std::fs::remove_file(ecu_state);
    let (worker, client) = launch_worker().await;
    let job = tokio::spawn({
        let guards = take_guards(setup);
        async move {
            run_program(
                client,
                &link_config(),
                Job::from(waiting_program()),
                JobLimits {
                    vci_return_wait: Duration::from_secs(2),
                    ..JobLimits::default()
                },
                guards,
            )
            .await
        }
    });
    wait_for_first_request(ecu_state).await;
    send(
        control_dir,
        &format!("{name}-a"),
        &format!(r#"{{"command": "fail_closes", "times": {closes}}}"#),
    );
    send(
        control_dir,
        &format!("{name}-b"),
        r#"{"command": "disconnect_vci"}"#,
    );
    let run = job.await.expect("the job task should not panic");
    // A loss during the open would be a `JobError::Link`; this one is the second request's.
    assert!(
        matches!(
            run.result,
            Err(JobError::Host {
                pc: SECOND_REQUEST_PC,
                ..
            })
        ),
        "{:?}",
        run.result
    );
    (worker, run)
}

async fn flow(control_dir: &Path, ecu_state: &Path, setup: GuardSetup) {
    // Flow 1: both closes fail, the worker keeps its slot and the guards come back marked.
    let (worker, JobRun { mut guards, .. }) =
        lost_job(control_dir, ecu_state, &setup, 2, "m1").await;
    assert!(
        guards.link_unconfirmed(),
        "a close that failed twice leaves the guards marked"
    );

    // A second taker of the VCI lock blocks while the marked guards live.
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancel));
    let taker = tokio::task::spawn_blocking({
        let setup = setup.clone();
        let cancel = Arc::clone(&cancel);
        move || JobGuards::take_vci_only(&setup, Duration::from_millis(10), &cancel)
    });
    tokio::time::sleep(STAYS_BLOCKED).await;
    assert!(
        !taker.is_finished(),
        "the VCI lock stays held while the guards are marked"
    );

    // Reaping the worker clears the mark; the guards then release the locks and the taker gets
    // them.
    let stopped = worker.stop(Duration::from_secs(5)).await;
    assert!(stopped.is_ok(), "{stopped:?}");
    guards.worker_gone();
    drop(guards);
    let guards = tokio::time::timeout(Duration::from_secs(10), taker)
        .await
        .expect("the blocked taker should get the lock once the worker is gone")
        .expect("the taker task should not panic")
        .expect("guards should be taken");

    // The next job runs on a fresh worker and reads the VIN.
    let _ = std::fs::remove_file(ecu_state);
    let (worker, client) = launch_worker().await;
    let JobRun { result, guards, .. } = run_program(
        client,
        &link_config(),
        Job::from(read_vin()),
        JobLimits::default(),
        guards,
    )
    .await;
    let state = result.expect("the job on the fresh worker should finish");
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    assert_eq!(state.stack, [Value::Bytes(vin)], "{state:?}");
    assert!(!guards.link_unconfirmed());
    drop(guards);
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");

    // Flow 2: the first close fails, the second is answered as the device being gone, which the
    // worker counts as closed: the guards are unmarked (ADR-393 item 2, ADR-422).
    let (worker, JobRun { guards, .. }) = lost_job(control_dir, ecu_state, &setup, 1, "m2").await;
    assert!(
        !guards.link_unconfirmed(),
        "the retried close of a lost device is confirmed"
    );
    let stopped = worker.stop(Duration::from_secs(5)).await;
    assert!(stopped.is_ok(), "{stopped:?}");
    drop(guards);
}

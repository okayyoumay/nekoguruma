// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! A link that cannot be confirmed closed keeps the job's guards (ADR-258): `worker-host`
//! launches the real `j2534-0404-service` binary against the `sim-vci` cdylib, a job runs a
//! program that waits between two requests, and this test unplugs the simulated VCI during the
//! wait. The second request fails, and so do the disconnects of the job's close, so the returned
//! guards are marked. Another job on the same VCI keeps waiting for the lock until the worker
//! has been stopped and `worker_gone` is called; then it takes the guards and reads the VIN
//! from a fresh worker.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{JobError, JobLimits, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// Instructions of [`waiting_program`]: the second request is the last one.
const SECOND_REQUEST_PC: u32 = 5;

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

/// Removes the temporary files however the test ends.
struct TempPaths {
    config: PathBuf,
    control_dir: PathBuf,
    ecu_state: PathBuf,
    locks: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_file(&self.ecu_state);
        let _ = std::fs::remove_dir_all(&self.control_dir);
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

#[test]
fn a_link_that_cannot_be_closed_keeps_the_guards_until_the_worker_is_gone() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let temp = std::env::temp_dir();
    let paths = TempPaths {
        config: temp.join(format!("sim-vci-unconfirmed-{nanos}.toml")),
        control_dir: temp.join(format!("sim-vci-unconfirmed-{nanos}")),
        ecu_state: temp.join(format!("sim-vci-unconfirmed-{nanos}.ecu")),
        locks: temp.join(format!("sim-vci-unconfirmed-locks-{nanos}")),
    };
    std::fs::write(
        &paths.config,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            sim_vci_path().display().to_string()
        ),
    )
    .expect("test config file should be writable");
    std::fs::create_dir(&paths.control_dir).expect("control directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &paths.config);
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
        vci: "sim-unconfirmed-vci".to_owned(),
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
                Duration::from_secs(120),
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

/// Waits until the job's open has created the ECU (`sim-vci` writes its state file then), and
/// [`UNPLUG_AFTER`] more. The file is also rewritten on every request, so its content cannot
/// tell the first request from the creation without a race; a fixed point inside the program's
/// wait can.
async fn wait_for_first_request(ecu_state: &Path) {
    while !ecu_state.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(UNPLUG_AFTER).await;
}

async fn flow(control_dir: &Path, ecu_state: &Path, setup: GuardSetup) {
    let (worker, client) = launch_worker().await;
    let job = tokio::spawn({
        let guards = take_guards(&setup);
        async move {
            run_program(
                client,
                &link_config(),
                waiting_program(),
                JobLimits::default(),
                guards,
            )
            .await
        }
    });

    // The VCI is unplugged during the wait, with the link open and the first request done.
    wait_for_first_request(ecu_state).await;
    send(control_dir, "001", r#"{"command": "disconnect_vci"}"#);
    let (result, mut guards) = job.await.expect("the job task should not panic");

    // A loss during the open would be a `JobError::Link`; this one is the second request's.
    assert!(
        matches!(
            result,
            Err(JobError::Host {
                pc: SECOND_REQUEST_PC,
                ..
            })
        ),
        "{result:?}"
    );
    assert!(
        guards.link_unconfirmed(),
        "the close of a lost device is not confirmed"
    );

    // Another job on this VCI keeps waiting, although the first one is over.
    let waiter = {
        let setup = setup.clone();
        std::thread::spawn(move || take_guards(&setup))
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished(), "the marked guards must stay held");

    let stopped = worker.stop(Duration::from_secs(5)).await;
    assert!(stopped.is_ok(), "{stopped:?}");
    assert!(!waiter.is_finished(), "stopping the worker is not enough");
    guards.worker_gone();
    drop(guards);
    let guards = waiter.join().expect("the waiter should take the guards");

    // The VCI is plugged back in for the fresh worker, which reads the VIN on the waiter's
    // guards.
    send(control_dir, "002", r#"{"command": "connect_vci"}"#);
    let (worker, client) = launch_worker().await;
    let (result, guards) = run_program(
        client,
        &link_config(),
        read_vin(),
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
}

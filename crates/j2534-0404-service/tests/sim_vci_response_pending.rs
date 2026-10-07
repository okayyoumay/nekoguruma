// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! Response pending (NRC 0x78) end to end: `sim-ecu` answers the agent's request with 0x78
//! several times before the final response. The worker keeps the request open through the
//! chain with the link settings the agent sets (`CP_RC78Handling`, `CP_RCByteOffset`,
//! `CP_P2Star`, `CP_RC78CompletionTimeout`), and the agent host skips the 0x78s the worker
//! passes on, so the procedure sees only the final response (ADR-235). A chain that outlasts
//! the completion timeout ends the job with `NoResponse`.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent::{HostError, JobError, JobLimits, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program, Value, VmState};
use worker_host::client::{ConnectOptions, WorkerClient};
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";

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

/// Removes the temporary config file and control directory however the test ends.
struct TempPaths {
    config: PathBuf,
    control_dir: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_dir_all(&self.control_dir);
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

/// Arms a response pending chain of `count` messages, `interval_ms` apart, for the next
/// request the ECU answers.
fn arm_response_pending(control_dir: &Path, name: &str, count: u32, interval_ms: u32) {
    send(
        control_dir,
        name,
        &format!(
            r#"{{"command": "inject_fault", "fault": {{"response_pending": {{"count": {count}, "interval_ms": {interval_ms}}}}}}}"#
        ),
    );
}

/// Reads the VIN.
fn read_vin() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }],
        constants: vec![vec![0xF1, 0x90]],
        sections: Vec::new(),
        source_map: Vec::new(),
    }
}

async fn run(client: &WorkerClient, config: &LinkConfig) -> Result<VmState, JobError> {
    run_program(client.clone(), config, read_vin(), JobLimits::default()).await
}

#[test]
fn the_worker_absorbs_response_pending_for_the_agent() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let temp = std::env::temp_dir();
    let paths = TempPaths {
        config: temp.join(format!("sim-vci-rc78-{nanos}.toml")),
        control_dir: temp.join(format!("sim-vci-rc78-{nanos}")),
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
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        // The simulated ECU uses its built-in configuration, without a state file.
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
        std::env::remove_var("NGR_SIM_ECU_STATE");
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
            tokio::time::timeout(Duration::from_secs(120), jobs(&paths.control_dir)).await
        })
        .expect("the end-to-end flow should finish in time");
    drop(paths);
}

async fn jobs(control_dir: &Path) {
    let worker = WorkerProcess::launch(
        Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
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
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);

    // Two 0x78s, 1.5 s apart, and the final response 1.5 s after the second: every gap is
    // longer than P2 (1 s) and shorter than P2* (5 s), so the job only gets the VIN if the
    // worker restarts its timer with P2* on each 0x78. The chain stays inside the completion
    // timeout (25 s). The worker passes the 0x78s on as results, and the agent host skips
    // them, so the procedure gets only the VIN.
    let config = LinkConfig::iso15765(0x7E0, 0x7E8);
    arm_response_pending(control_dir, "001", 2, 1_500);
    let start = Instant::now();
    let state = run(&client, &config)
        .await
        .expect("the job should get the final response");
    assert_eq!(state.stack, [Value::Bytes(vin.clone())], "{state:?}");
    assert!(
        start.elapsed() >= Duration::from_millis(3_000),
        "the final response follows the chain: {:?}",
        start.elapsed()
    );

    // A chain that outlasts the completion timeout: every 0x78 still arrives within P2*, but
    // the whole chain (20 x 300 ms) runs past 1.5 s, so the request ends without a response.
    let short_chain = LinkConfig {
        rc78_completion_ms: 1_500,
        ..LinkConfig::iso15765(0x7E0, 0x7E8)
    };
    arm_response_pending(control_dir, "002", 20, 300);
    let start = Instant::now();
    let timed_out = run(&client, &short_chain).await;
    assert!(
        matches!(
            timed_out,
            Err(JobError::Host {
                pc: 1,
                source: HostError::NoResponse
            })
        ),
        "{timed_out:?}"
    );
    // It ended at the completion timeout, long before the chain would have.
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! An agent job end to end without hardware (ADR-235, ADR-247): `worker-host` launches the real
//! `j2534-0404-service` binary against the `sim-vci` cdylib, and `agent::run_program` runs a
//! diag-ir procedure on a CAN link to the simulated ECU behind it.
//!
//! The `FlashTransfer` instruction against the simulated ECU (ADR-250): a download of more
//! blocks than the wire block counter holds before it wraps, followed by RequestTransferExit,
//! which the ECU accepts only when every block arrived in order. (A `FlashTransfer` with no
//! RequestDownload before it cannot pass the program validator, so the host's refusal is a unit
//! test in `crates/agent/src/host.rs`.)
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it sets for the
//! spawned service cannot race with another test.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::{JobLimits, LinkConfig, run_program};
use diag_ir::{
    FlashRecovery, IR_SCHEMA_VERSION, Op, Program, RecoveryBoundaries, RecoveryRequired,
    RecoveryTiming, Value,
};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's erase-memory routine (`sim_ecu::RID_ERASE_MEMORY`).
const RID_ERASE_MEMORY: u16 = 0xFF00;
/// More one-byte blocks than the counter holds before it wraps (255), and past its second wrap
/// point of 0x00 (block 256) so the counter is 0x00 and 0x01 again.
const BLOCKS: u32 = 300;

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

/// Programming session, erase, RequestDownload of `BLOCKS` bytes at 0, one `FlashTransfer` per
/// byte, then RequestTransferExit. The instruction's operand is the block number the compiler
/// would write; the host ignores it.
fn download_program() -> Program {
    let mut constants = vec![
        vec![0x02],
        Vec::new(),
        // dataFormatIdentifier, address and size lengths (4 and 4), address 0, size.
        [&[0x00, 0x44, 0, 0, 0, 0][..], &BLOCKS.to_be_bytes()].concat(),
        Vec::new(),
    ];
    let first_block = constants.len() as u32;
    constants.extend((0..BLOCKS).map(|i| vec![i as u8]));
    let mut code = vec![
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x10 },
        Op::PushBytes(1),
        Op::RoutineControl {
            routine: RID_ERASE_MEMORY,
            sub: 0x01,
        },
        Op::PushBytes(2),
        Op::ServiceRequest { service: 0x34 },
    ];
    for i in 0..BLOCKS {
        code.push(Op::PushBytes(first_block + i));
        code.push(Op::FlashTransfer { block: 7 });
    }
    code.push(Op::PushBytes(3));
    let exit_pc = code.len() as u32;
    code.push(Op::ServiceRequest { service: 0x37 });
    // The validator wants download instructions inside a flash session's plan. The plan never
    // allows a restart (a recovery point at the erase), so it declares nothing else.
    let flash = vec![FlashRecovery {
        flash_session: 1,
        stage: 1,
        max_resumes: 1,
        recovery_required: RecoveryRequired::FromPc(3),
        boundaries: RecoveryBoundaries {
            entry_pc: 0,
            erase_pc: 3,
            transfer_exit_pc: exit_pc,
            post_transfer_end_pc: exit_pc + 1,
        },
        timing: RecoveryTiming {
            session_timeout_millis: 5_000,
            teardown_margin_millis: 1_000,
            ecu_startup_millis: 1_000,
            confirmation_window_millis: 1_000,
        },
        version_read_retries: 0,
        no_application: None,
    }];
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code,
        constants,
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash,
    }
}

#[test]
fn agent_job_downloads_more_blocks_than_the_counter_holds() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let config_path = std::env::temp_dir().join(format!("agent-flash-{nanos}-sim-vci.toml"));
    let ecu_path = std::env::temp_dir().join(format!("agent-flash-{nanos}-ecu.json"));
    let _config = TempFile(config_path.clone());
    let _ecu = TempFile(ecu_path.clone());
    std::fs::write(
        &config_path,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            sim_vci_path().display().to_string()
        ),
    )
    .expect("test config file should be writable");
    // No security access, so the program needs no seed-key computation.
    std::fs::write(
        &ecu_path,
        r#"{"vin": "NGRSIMECU00000003", "part_number": "NGR-SIM-ECU", "sw_version": "1.0.0",
            "response_delay_ms": 0, "drop_at_block": null, "require_security_access": false,
            "require_gateway_auth": false, "fail_checksum": false}"#,
    )
    .expect("ECU config file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::set_var("NGR_SIM_ECU_CONFIG", &ecu_path);
        std::env::remove_var("NGR_SIM_ECU_STATE");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A wedged service must fail the test, not hang the CI job.
    runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), run_the_job()).await })
        .expect("the flow should finish in time");
}

async fn run_the_job() {
    let worker = WorkerProcess::launch(
        std::path::Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")),
        ServiceKind::J2534V0404,
        LIBRARY_NAME,
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            long_size: Some(long_size()),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");

    let state = run_program(
        worker
            .connect(&ConnectOptions::default())
            .await
            .expect("client should connect"),
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        download_program(),
        JobLimits::default(),
    )
    .await
    .expect("the download job should finish");
    // RequestTransferExit is positive only when every block, in counter order across the wrap
    // from 0xFF to 0x00, arrived.
    assert_eq!(
        state.stack.last(),
        Some(&Value::Bytes(vec![0x77])),
        "{:?}",
        state.stack.last()
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! The refusing side of the agent's request policy end to end (ADR-247): `worker-host` launches
//! the real `j2534-0404-service` binary against the J2534 mock library, which is not `sim-vci`
//! (its version strings differ), so the agent leaves the link read-only. A program that reads
//! first and writes second is then refused at the write's instruction, before the read has run.
//!
//! The mock lives in the service process, so this test process cannot count the messages it was
//! sent; the refusal's pc (the read at pc 1 did not run, or the error would differ) is the
//! evidence that nothing was sent.
//!
//! This file holds a single test, so the process-wide `VCI_CONFIG_PATH` it sets for the
//! spawned service cannot race with another test.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::policy::READ_ONLY_SERVICES;
use agent::{HostError, JobError, JobLimits, LinkConfig, run_program};
use diag_ir::{IR_SCHEMA_VERSION, Op, Program};
use j2534_0404_mock::mock_library_path;
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "mock-agent-refuses-writes";

/// Removes the temporary config file however the test ends.
struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A read (pc 0 and 1), then a session change (pc 2 and 3), which is not read-only.
fn program() -> Program {
    Program {
        schema_version: IR_SCHEMA_VERSION,
        code: vec![
            Op::PushBytes(0),
            Op::ServiceRequest { service: 0x22 },
            Op::PushBytes(1),
            Op::ServiceRequest { service: 0x10 },
        ],
        constants: vec![vec![0xF1, 0x90], vec![0x02]],
        sections: Vec::new(),
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
}

#[test]
fn agent_refuses_a_write_on_a_vci_that_is_not_the_simulator() {
    let mock_library =
        mock_library_path().expect("mock library should be discoverable after build");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let config_path = std::env::temp_dir().join(format!("agent-refuses-{nanos}-mock.toml"));
    let config = TempFile(config_path.clone());
    std::fs::write(
        &config_path,
        format!(
            "[config.apis.j2534-0404.libs.{LIBRARY_NAME:?}]\nlibrary_path = {:?}\n",
            mock_library.display().to_string()
        ),
    )
    .expect("test config file should be writable");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &config_path);
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
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), run_the_job()).await })
        .expect("the flow should finish in time");
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
            "agent-refuses-locks-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
        vci: "agent-refuses-vci".to_owned(),
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
        // Generous timeouts for a debug binary on a busy CI runner.
        &LaunchOptions {
            startup_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(5),
            ..LaunchOptions::default()
        },
    )
    .await
    .expect("worker should launch");
    let client = worker
        .connect(&ConnectOptions::default())
        .await
        .expect("client should connect");

    assert!(!READ_ONLY_SERVICES.contains(&0x10));
    // The program writes, so its guards hold the slot: the refusal below is the policy's, not
    // the missing slot's.
    let (result, _guards) = run_program(
        client,
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        program(),
        JobLimits::default(),
        job_guards(true),
    )
    .await;
    let error = result.expect_err("the write should be refused");
    // The debug build's ceiling lets the program through before anything opens, so this is the
    // check after the link is open: it refuses at the session change, and the read before it
    // never ran (a read on the mock's channel would have failed the job another way).
    assert!(
        matches!(
            &error,
            JobError::Refused {
                pc: 3,
                source: HostError::NotAllowed(0x10)
            }
        ),
        "{error:?}"
    );

    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

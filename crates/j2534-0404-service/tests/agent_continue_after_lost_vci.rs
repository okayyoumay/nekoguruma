// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! A run whose VCI is lost goes on in the same run when the VCI comes back in time (design
//! 8.2.5, ADR-393): `worker-host` launches the real `j2534-0404-service` binary against the
//! `sim-vci` cdylib, a job reads the VIN, waits, and reads it again, and this test unplugs the
//! simulated VCI during the wait and plugs it back in at once. The simulator keeps the device
//! lost, as J2534-1 6.10.1 has it, until the library closes it, so the second read fails with
//! the loss although the VCI is present again. With the reads `Safe` the run closes the
//! lost link (the J2534 library releases the device on the first close yet reports the loss
//! from it, so the close is made once more and the second finds the link gone), opens a new
//! one and finishes with the read's value; the log shows the loss, the reconnect, the
//! settlement and the new link in that order. With the reads `Unsafe` the second read is
//! delayed by the ECU and the VCI is lost while it is pending, so the request may have reached
//! the ECU: the run ends on site with no reopen, and its close of the lost link is confirmed all
//! the same, since the path was lost.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::{
    CommLogSetup, CommLogStatus, Job, JobError, JobLimits, JobRun, LinkConfig, VciFacts,
    run_program,
};
use diag_ir::{IR_SCHEMA_VERSION, Idempotency, Interruptible, Op, Program, Section, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, WorkerProcess};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// How long the program waits between its two reads.
const WAIT: Duration = Duration::from_secs(6);
/// The ECU's delay of the second read's answer in the `Unsafe` scenario: far past the read's timeout, so the
/// read stays pending until the VCI is unplugged, which the test does as soon as the request reached the ECU.
const ANSWER_DELAY_MS: u32 = 20_000;
/// The longest the test waits for an event it observes (a log line, a control file taken).
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest the test waits for the run to end: the run itself waits at most
/// [`RETURN_WAIT`] for the VCI.
const GENEROUS: Duration = Duration::from_secs(60);
/// How long the run waits for the VCI to come back; the VCI is back before the run asks.
const RETURN_WAIT: Duration = Duration::from_secs(30);

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
    definitions: PathBuf,
    ecu_state: PathBuf,
    locks: PathBuf,
    comm_log: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_file(&self.ecu_state);
        let _ = std::fs::remove_dir_all(&self.control_dir);
        let _ = std::fs::remove_dir_all(&self.definitions);
        let _ = std::fs::remove_dir_all(&self.locks);
        let _ = std::fs::remove_dir_all(&self.comm_log);
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

/// Reads the VIN, waits, and reads it again, the whole program under one section of
/// `idempotency`.
fn waiting_program(idempotency: Idempotency) -> Program {
    let code = vec![
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x22 },
        Op::Pop,
        Op::Wait {
            millis: WAIT.as_millis() as u32,
        },
        Op::PushBytes(0),
        Op::ServiceRequest { service: 0x22 },
    ];
    Program {
        schema_version: IR_SCHEMA_VERSION,
        sections: vec![Section {
            start_pc: 0,
            end_pc: code.len() as u32,
            interruptible: Interruptible::Yes,
            idempotency,
            expected_millis: 0,
        }],
        code,
        constants: vec![vec![0xF1, 0x90]],
        source_map: Vec::new(),
        identity: Default::default(),
        preconditions: Default::default(),
        flash: Vec::new(),
    }
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

/// Small limits: the VCI is polled often and the run gives up well before the test does.
fn limits() -> JobLimits {
    JobLimits {
        reopen_poll: Duration::from_millis(100),
        vci_return_wait: RETURN_WAIT,
        ..JobLimits::default()
    }
}

/// A job of `program` that writes the communication log in `dir`, told it runs on J2534.
fn logged(program: Program, dir: &Path) -> Job {
    Job::from(program)
        .with_comm_log(CommLogSetup {
            dir: dir.to_owned(),
            key: agent::journal::JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e81".to_owned()),
                generation: 1,
            },
        })
        .with_vci(VciFacts {
            api: agent::launch::Api::J2534,
            long_size: long_size(),
        })
}

/// The log's lines that parse; a line the agent is still writing is left out.
fn log_lines(dir: &Path) -> Vec<serde_json::Value> {
    let Some(path) = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .map(|entry| entry.expect("an entry").path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
    else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Polls `condition` until it holds, and panics with `what` after [`EVENT_TIMEOUT`].
async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < EVENT_TIMEOUT,
            "gave up waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// How many exchanges the log in `dir` holds: the first read is over when it holds one.
fn exchanges(dir: &Path) -> usize {
    log_lines(dir)
        .iter()
        .filter(|line| line["event"] == "exchange")
        .count()
}

/// Waits until the ECU's state file differs from `previous`, and returns its new content.
async fn changed_from(ecu_state: &Path, previous: Vec<u8>) -> Vec<u8> {
    let mut now = previous.clone();
    wait_for("the ECU's state to change", || {
        now = std::fs::read(ecu_state).unwrap_or_default();
        !now.is_empty() && now != previous
    })
    .await;
    now
}

#[test]
fn a_run_goes_on_on_a_new_link_when_the_vci_returns_in_time() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let temp = std::env::temp_dir();
    let paths = TempPaths {
        config: temp.join(format!("sim-vci-continue-{nanos}.toml")),
        control_dir: temp.join(format!("sim-vci-continue-{nanos}")),
        definitions: temp.join(format!("sim-vci-continue-definitions-{nanos}")),
        ecu_state: temp.join(format!("sim-vci-continue-{nanos}.ecu")),
        locks: temp.join(format!("sim-vci-continue-locks-{nanos}")),
        comm_log: temp.join(format!("sim-vci-continue-commlog-{nanos}")),
    };
    std::fs::write(&paths.config, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    std::fs::create_dir_all(&paths.definitions).expect("definition directory");
    j2534_0404_mock::write_definition(&paths.definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    std::fs::create_dir(&paths.control_dir).expect("control directory should be created");
    std::fs::create_dir(&paths.comm_log).expect("log directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &paths.config);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &paths.definitions);
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &paths.control_dir);
        std::env::set_var("NGR_SIM_ECU_STATE", &paths.ecu_state);
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }
    let setup = GuardSetup {
        dir: paths.locks.clone(),
        vci: "sim-continue-vci".to_owned(),
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
            tokio::time::timeout(Duration::from_secs(180), flow(&paths, setup)).await
        })
        .expect("the end-to-end flow should finish in time");
    drop(paths);
}

async fn flow(paths: &TempPaths, setup: GuardSetup) {
    safe_reads_go_on(paths, &setup).await;
    unsafe_reads_end_on_site(paths, &setup).await;
}

/// Unplugs the VCI during the wait (`in_read`: while the second read is pending), plugs it back
/// in, and returns the run's end, the log's lines and the worker. Every step waits on an event
/// the test observes, never on a pause.
async fn lose_and_return(
    paths: &TempPaths,
    setup: &GuardSetup,
    idempotency: Idempotency,
    command_prefix: &str,
    in_read: bool,
) -> (JobRun, Vec<serde_json::Value>, WorkerProcess) {
    let log_dir = paths.comm_log.join(command_prefix);
    std::fs::create_dir(&log_dir).expect("log directory should be created");
    let (worker, client) = launch_worker().await;
    let _ = std::fs::remove_file(&paths.ecu_state);
    let job = tokio::spawn({
        let guards = take_guards(setup);
        let log_dir = log_dir.clone();
        async move {
            run_program(
                client,
                &LinkConfig::iso15765(0x7E0, 0x7E8),
                logged(waiting_program(idempotency), &log_dir),
                limits(),
                guards,
            )
            .await
        }
    });
    // The first read is over when its exchange is in the log: the program is then in its wait.
    wait_for("the first read's exchange", || exchanges(&log_dir) >= 1).await;
    if in_read {
        // The ECU holds back the answer of the next read, the second, beyond the read's
        // timeout; the VCI is lost while that read is pending, so the request may have reached
        // the ECU.
        let before = std::fs::read(&paths.ecu_state).expect("the ECU state file");
        send(
            &paths.control_dir,
            &format!("{command_prefix}-0"),
            &format!(
                r#"{{"command": "inject_fault", "service": 34, "fault": {{"delay_response": {{"ms": {ANSWER_DELAY_MS}}}}}}}"#
            ),
        );
        // `sim-vci` saves the ECU's state when it takes the fault (the fault is armed in it),
        // and again when the second read's request reaches the ECU and consumes the fault.
        let armed = changed_from(&paths.ecu_state, before).await;
        changed_from(&paths.ecu_state, armed).await;
    }
    send(
        &paths.control_dir,
        &format!("{command_prefix}-1"),
        r#"{"command": "disconnect_vci"}"#,
    );
    // Both files take effect at the next J2534 call, in name order: the device is lost, and the
    // VCI is back by the time the run closes the lost link and opens a new one.
    send(
        &paths.control_dir,
        &format!("{command_prefix}-2"),
        r#"{"command": "connect_vci"}"#,
    );
    let run = tokio::time::timeout(GENEROUS, job)
        .await
        .expect("the run should end")
        .expect("the job task should not panic");
    let lines = log_lines(&log_dir);
    (run, lines, worker)
}

async fn safe_reads_go_on(paths: &TempPaths, setup: &GuardSetup) {
    let (run, lines, worker) =
        lose_and_return(paths, setup, Idempotency::Safe, "safe", false).await;
    let JobRun {
        result,
        guards,
        comm_log: status,
    } = run;
    let text = || format!("{lines:#?}");
    let state = result.unwrap_or_else(|error| panic!("{error}\n{}", text()));
    let mut vin = vec![0x62, 0xF1, 0x90];
    vin.extend_from_slice(VIN);
    assert_eq!(state.stack, [Value::Bytes(vin)], "{state:?}");
    assert_eq!(status, CommLogStatus::Complete);
    // The lost link was closed on the module's disconnect and the new one's close was clean.
    assert!(!guards.link_unconfirmed(), "{}", text());

    // The loss, the reconnect, the settlement and the new link, in that order, after the first
    // link's own records.
    let order: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["event"].as_str())
        .filter(|event| {
            [
                "vci_loss",
                "vci_connected",
                "vci_loss_settled",
                "link_opened",
            ]
            .contains(event)
        })
        .collect();
    assert_eq!(
        order,
        [
            "vci_connected",
            "link_opened",
            "vci_loss",
            "vci_connected",
            "vci_loss_settled",
            "link_opened"
        ],
        "{}",
        text()
    );
    drop(guards);
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

async fn unsafe_reads_end_on_site(paths: &TempPaths, setup: &GuardSetup) {
    // An `Unsafe` read that may have reached the ECU is not sent again, so the run asks for no
    // new link and ends where the loss caught it.
    let (run, lines, worker) =
        lose_and_return(paths, setup, Idempotency::Unsafe, "unsafe", true).await;
    let JobRun {
        result,
        guards,
        comm_log: _,
    } = run;
    let text = || format!("{lines:#?}");
    assert!(
        matches!(&result, Err(JobError::OnSiteInterventionRequired(_))),
        "{result:?}\n{}",
        text()
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["event"] == "link_opened")
            .count(),
        1,
        "no second link: {}",
        text()
    );
    // The run's path was lost, so the module's disconnect confirms the lost link closed.
    assert!(!guards.link_unconfirmed(), "{}", text());
    drop(guards);
    worker
        .stop(Duration::from_secs(5))
        .await
        .expect("worker should stop");
}

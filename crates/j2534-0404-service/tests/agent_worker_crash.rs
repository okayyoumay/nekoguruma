// Relies on the debug-only runtime VCI_CONFIG_PATH override; see ADR-073.
#![cfg(debug_assertions)]
//! A worker that crashes during a run is started again by the run (design 8.2.5, ADR-394):
//! `worker-host` launches the real `j2534-0404-service` binary against the `sim-vci` cdylib
//! through the agent's `RelaunchingWorker`, a job reads the VIN, waits, and reads it again, and
//! this test kills the worker process during the wait, as a crash would. With the reads `Safe`
//! the second read cannot reach the dead worker; the run sees the process has exited, closes
//! nothing through it, starts a new worker, opens a new link on it and finishes with the read's
//! value; the log shows the worker's exit and restart and then the new connection, and no
//! lost VCI. With the reads `Unsafe` the run cannot go on and ends on site as before, but it
//! reaps the dead worker at its end: the log has the exit and no restart, and the guards are
//! not marked, since a worker that no longer exists holds no link.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::launch::{LaunchRecipe, RelaunchingWorker, WorkerSlot};
use agent::{
    CommLogSetup, CommLogStatus, HostError, Job, JobError, JobLimits, JobRun, LinkConfig, VciFacts,
    WorkerSource, WorkerStatus, run_program,
};
use diag_ir::{IR_SCHEMA_VERSION, Idempotency, Interruptible, Op, Program, Section, Value};
use worker_host::client::ConnectOptions;
use worker_host::service::{LaunchOptions, OperatingMode, ServiceKind, Stopped};

const LIBRARY_NAME: &str = "sim-vci";
/// The simulated ECU's built-in VIN (`crates/sim-vci/docs/simulated-vci.md`).
const VIN: &[u8] = b"NGRSIMECU00000001";
/// How long the program waits between its two reads.
const WAIT: Duration = Duration::from_secs(4);
/// The longest the test waits for an event it observes (a log line).
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest the test waits for the run to end.
const GENEROUS: Duration = Duration::from_secs(60);

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
    definitions: PathBuf,
    ecu_state: PathBuf,
    locks: PathBuf,
    comm_log: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config);
        let _ = std::fs::remove_file(&self.ecu_state);
        let _ = std::fs::remove_dir_all(&self.definitions);
        let _ = std::fs::remove_dir_all(&self.locks);
        let _ = std::fs::remove_dir_all(&self.comm_log);
    }
}

/// Ends the worker process `pid` at once, as a crash would: nothing in it is closed or flushed.
#[cfg(unix)]
fn kill_worker(pid: u32) {
    let pid = i32::try_from(pid).expect("a process ID fits in pid_t");
    // SAFETY: kill(2) takes a process ID and a signal number and touches no memory.
    let killed = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(
        killed,
        0,
        "kill of worker {pid} failed: {}",
        std::io::Error::last_os_error()
    );
}

/// Ends the worker process `pid` at once, as a crash would: `TerminateProcess`, so nothing in
/// it is closed or flushed.
#[cfg(windows)]
fn kill_worker(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: OpenProcess takes an access mask, a flag and a process ID and touches no memory.
    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    assert!(
        !process.is_null(),
        "opening worker {pid} failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `process` is the handle opened above, closed only after this call.
    let terminated = unsafe { TerminateProcess(process, 1) };
    let error = std::io::Error::last_os_error();
    // SAFETY: `process` is open and not used after this.
    unsafe { CloseHandle(process) };
    assert_ne!(terminated, 0, "terminating worker {pid} failed: {error}");
}

/// How the killed worker's exit reads in the log: `kill_worker` ends it with SIGKILL, or with
/// exit code 1 where there are no signals.
fn killed_exit() -> serde_json::Value {
    if cfg!(unix) {
        serde_json::json!({"signal": 9})
    } else {
        serde_json::json!({"code": 1})
    }
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
        targets: Vec::new(),
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

/// Launches the first worker and gives the run its source, with the slot that holds the
/// current worker's process.
async fn launch_source() -> (RelaunchingWorker, WorkerSlot) {
    let recipe = LaunchRecipe {
        binary: Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")).to_owned(),
        kind: ServiceKind::J2534V0404,
        vci: LIBRARY_NAME.to_owned(),
        mode: OperatingMode::User,
        options: launch_options(),
        connect: ConnectOptions::default(),
    };
    let (process, client) = recipe.launch().await.expect("worker should launch");
    let source = RelaunchingWorker::new(recipe, process, client);
    let slot = source.slot();
    (source, slot)
}

fn take_guards(setup: &GuardSetup) -> JobGuards {
    JobGuards::take_vci_only(setup, Duration::from_millis(10), &AtomicBool::new(false))
        .expect("guards should be taken")
}

/// Small limits: the new worker is asked for at once and the run gives up well before the test
/// does.
fn limits() -> JobLimits {
    JobLimits {
        reopen_poll: Duration::from_millis(100),
        vci_return_wait: Duration::from_secs(30),
        ..JobLimits::default()
    }
}

/// A job of `program` that writes the communication log in `dir`, told it runs on J2534.
fn logged(program: Program, dir: &Path) -> Job {
    Job::from(program)
        .with_comm_log(CommLogSetup {
            dir: dir.to_owned(),
            key: agent::journal::JobKey {
                job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e82".to_owned()),
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

#[test]
fn a_run_starts_a_worker_that_crashed_again() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_nanos();
    let pid = std::process::id();
    let temp = std::env::temp_dir();
    let paths = TempPaths {
        config: temp.join(format!("sim-vci-crash-{pid}-{nanos}.toml")),
        definitions: temp.join(format!("sim-vci-crash-definitions-{pid}-{nanos}")),
        ecu_state: temp.join(format!("sim-vci-crash-{pid}-{nanos}.ecu")),
        locks: temp.join(format!("sim-vci-crash-locks-{pid}-{nanos}")),
        comm_log: temp.join(format!("sim-vci-crash-commlog-{pid}-{nanos}")),
    };
    std::fs::write(&paths.config, "").expect("test config file should be writable");
    // The registration definition names the library, through the debug-only
    // `NGR_J2534_DEFINITION_DIR` override (ADR-282).
    std::fs::create_dir_all(&paths.definitions).expect("definition directory");
    j2534_0404_mock::write_definition(&paths.definitions, LIBRARY_NAME, &sim_vci_path())
        .expect("definition file should be writable");
    std::fs::create_dir(&paths.comm_log).expect("log directory should be created");
    // SAFETY: this test binary runs only this test, and the runtime, the only other source of
    // threads here, is built below, so nothing reads the environment concurrently.
    unsafe {
        std::env::set_var("VCI_CONFIG_PATH", &paths.config);
        std::env::set_var("NGR_J2534_DEFINITION_DIR", &paths.definitions);
        // The simulated ECU keeps its state in a file (ADR-241), so a new worker finds the ECU
        // as the killed one left it.
        std::env::set_var("NGR_SIM_ECU_STATE", &paths.ecu_state);
        std::env::remove_var("NGR_SIM_VCI_CONTROL_DIR");
        std::env::remove_var("VCI_SERVICE_INSECURE_NO_AUTH");
        std::env::remove_var("NGR_SIM_ECU_CONFIG");
    }
    let setup = GuardSetup {
        dir: paths.locks.clone(),
        vci: "sim-crash-vci".to_owned(),
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
    a_worker_dead_before_the_run_ends_it_in_the_link_error(&setup).await;
    a_start_the_system_refuses_is_a_spawn_failure().await;
}

/// Waits until the process in `slot` has exited, which is when its status can be read.
async fn exited_in(slot: &WorkerSlot) {
    wait_for("the worker's exit", || {
        slot.lock()
            .unwrap()
            .as_mut()
            .is_some_and(|process| matches!(process.try_wait(), Ok(Some(_))))
    })
    .await;
}

/// A worker that died before the run asked for its first client: the source does not start a
/// new one there (it cannot, in async context, and the log would not tell the exit), so the
/// run ends in the link error with nothing to close, and no panic.
async fn a_worker_dead_before_the_run_ends_it_in_the_link_error(setup: &GuardSetup) {
    let (source, slot) = launch_source().await;
    let pid = slot
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|p| p.id())
        .expect("it runs");
    kill_worker(pid);
    exited_in(&slot).await;
    let run = run_program(
        source,
        &LinkConfig::iso15765(0x7E0, 0x7E8),
        Job::from(waiting_program(Idempotency::Safe)),
        limits(),
        take_guards(setup),
    )
    .await;
    assert!(
        matches!(&run.result, Err(JobError::Link(_))),
        "{:?}",
        run.result
    );
    assert!(!run.guards.link_unconfirmed());
    drop(run.guards);
    let process = slot
        .lock()
        .unwrap()
        .take()
        .expect("the slot holds a worker");
    assert_eq!(
        process
            .stop(Duration::from_secs(5))
            .await
            .expect("worker should stop"),
        Stopped::Exited
    );
}

/// A recipe whose binary does not exist: once the worker has exited, the source's start fails
/// as a start the system refuses, which the run takes as the end of the attempts.
async fn a_start_the_system_refuses_is_a_spawn_failure() {
    let good = LaunchRecipe {
        binary: Path::new(env!("CARGO_BIN_EXE_j2534-0404-service")).to_owned(),
        kind: ServiceKind::J2534V0404,
        vci: LIBRARY_NAME.to_owned(),
        mode: OperatingMode::User,
        options: launch_options(),
        connect: ConnectOptions::default(),
    };
    let (process, client) = good.launch().await.expect("worker should launch");
    let pid = process.id().expect("it runs");
    // The same worker, under a recipe that names no binary.
    let recipe = LaunchRecipe {
        binary: PathBuf::from("/nonexistent/j2534-0404-service"),
        kind: ServiceKind::J2534V0404,
        vci: LIBRARY_NAME.to_owned(),
        mode: OperatingMode::User,
        options: launch_options(),
        connect: ConnectOptions::default(),
    };
    let source = RelaunchingWorker::new(recipe, process, client);
    let slot = source.slot();
    kill_worker(pid);
    exited_in(&slot).await;
    let (error, process) = tokio::task::spawn_blocking(move || {
        let mut source = source;
        assert!(matches!(source.status(), WorkerStatus::Exited(_)));
        let error = source.client().expect_err("no worker can be started");
        (error, slot.lock().unwrap().take())
    })
    .await
    .expect("no panic");
    assert!(
        matches!(
            error,
            HostError::WorkerStart {
                spawn_failed: true,
                ..
            }
        ),
        "{error:?}"
    );
    drop(process);
}

/// Runs the waiting program, kills the worker during the wait, and returns the run's end, the
/// log's lines, the killed worker's process ID and the slot with the process left at the end.
async fn crash_during_the_wait(
    paths: &TempPaths,
    setup: &GuardSetup,
    idempotency: Idempotency,
    name: &str,
) -> (JobRun, Vec<serde_json::Value>, u32, WorkerSlot) {
    let log_dir = paths.comm_log.join(name);
    std::fs::create_dir(&log_dir).expect("log directory should be created");
    let (source, slot) = launch_source().await;
    let killed = slot
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|process| process.id())
        .expect("the worker runs");
    let job = tokio::spawn({
        let guards = take_guards(setup);
        let log_dir = log_dir.clone();
        async move {
            run_program(
                source,
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
    kill_worker(killed);
    let run = tokio::time::timeout(GENEROUS, job)
        .await
        .expect("the run should end")
        .expect("the job task should not panic");
    let lines = log_lines(&log_dir);
    (run, lines, killed, slot)
}

/// The events of `lines` the crash and the new connection are told by, in order.
fn crash_events(lines: &[serde_json::Value]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|line| line["event"].as_str())
        .filter(|event| {
            [
                "worker_exit",
                "worker_restart",
                "vci_loss",
                "vci_loss_settled",
                "vci_connected",
                "link_opened",
            ]
            .contains(event)
        })
        .collect()
}

/// The first line of `lines` that is `event`.
fn event<'a>(lines: &'a [serde_json::Value], event: &str) -> &'a serde_json::Value {
    lines
        .iter()
        .find(|line| line["event"] == event)
        .unwrap_or_else(|| panic!("no {event} in {lines:#?}"))
}

async fn safe_reads_go_on(paths: &TempPaths, setup: &GuardSetup) {
    let (run, lines, killed, slot) =
        crash_during_the_wait(paths, setup, Idempotency::Safe, "safe").await;
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
    // Nothing was closed through the dead worker, and the new worker's link closed cleanly.
    assert!(!guards.link_unconfirmed(), "{}", text());

    // The exit, the restart, the new connection and the new link, in that order, after the
    // first link's own records; no VCI was lost.
    assert_eq!(
        crash_events(&lines),
        [
            "vci_connected",
            "link_opened",
            "worker_exit",
            "worker_restart",
            "vci_connected",
            "link_opened"
        ],
        "{}",
        text()
    );
    let exit = event(&lines, "worker_exit");
    assert_eq!(exit["pid"], killed, "{exit}");
    assert_eq!(exit["exit"], killed_exit(), "{exit}");
    // The worker ended between two primitives, so none was in flight.
    assert_eq!(exit["refers_to"], serde_json::json!([]), "{exit}");
    let restarted = event(&lines, "worker_restart")["pid"]
        .as_u64()
        .expect("the new worker's process ID");
    assert_ne!(restarted, u64::from(killed));
    // The second read was sent again on the new link.
    let origins: Vec<&str> = lines
        .iter()
        .filter(|line| line["event"] == "exchange")
        .map(|line| line["origin"].as_str().expect("an origin"))
        .collect();
    assert_eq!(origins, ["procedure", "procedure", "repeat"], "{}", text());
    drop(guards);
    // The process the slot holds is the new worker's, and it stops by itself.
    let process = slot
        .lock()
        .unwrap()
        .take()
        .expect("the slot holds a worker");
    assert_eq!(process.id().map(u64::from), Some(restarted));
    assert_eq!(
        process
            .stop(Duration::from_secs(5))
            .await
            .expect("worker should stop"),
        Stopped::Exited
    );
}

async fn unsafe_reads_end_on_site(paths: &TempPaths, setup: &GuardSetup) {
    // An `Unsafe` read that may have reached the ECU is not sent again, so the run starts no
    // worker and ends where the crash caught it.
    let (run, lines, killed, slot) =
        crash_during_the_wait(paths, setup, Idempotency::Unsafe, "unsafe").await;
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
        crash_events(&lines),
        ["vci_connected", "link_opened", "worker_exit"],
        "{}",
        text()
    );
    let exit = event(&lines, "worker_exit");
    assert_eq!(exit["pid"], killed, "{exit}");
    assert_eq!(exit["exit"], killed_exit(), "{exit}");
    // The dead worker was reaped at the run's end, so its link counts as closed with no close
    // made through it, which would have failed and marked the guards.
    assert!(!guards.link_unconfirmed(), "{}", text());
    drop(guards);
    // The process in the slot is the dead one, already reaped: stopping it finds it gone.
    let process = slot
        .lock()
        .unwrap()
        .take()
        .expect("the slot holds a worker");
    assert_eq!(
        process
            .stop(Duration::from_secs(5))
            .await
            .expect("worker should stop"),
        Stopped::Exited
    );
}

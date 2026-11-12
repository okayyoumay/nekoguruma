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
//! The reads go on because the worker died between two primitives, so the next read was refused
//! unsent. A worker killed while an `Unsafe` request is outstanding (the ECU answers it with a
//! long response pending chain) is another case: the request may have reached the ECU, so the
//! run ends in on-site intervention with the request marker kept in the journal, and nothing
//! is sent again on a new worker.
//!
//! This file holds a single test, so the process-wide environment it sets for the spawned
//! service cannot race with another test.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent::guards::{GuardSetup, JobGuards};
use agent::inputs::ServiceSources;
use agent::journal::Journal;
use agent::launch::{LaunchRecipe, RelaunchingWorker, WorkerSlot};
use agent::{
    CommLogSetup, CommLogStatus, HostError, Job, JobError, JobLimits, JobRun, JournalSetup,
    LinkConfig, VciFacts, WorkerSource, WorkerStatus, run_program, run_program_journaled,
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
    control_dir: PathBuf,
    trace: PathBuf,
}

impl Drop for TempPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.control_dir);
        let _ = std::fs::remove_file(&self.trace);
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

/// One `Unsafe` VIN read, the only request of the program.
fn unsafe_read_program() -> Program {
    let code = vec![Op::PushBytes(0), Op::ServiceRequest { service: 0x22 }];
    Program {
        schema_version: IR_SCHEMA_VERSION,
        sections: vec![Section {
            start_pc: 0,
            end_pc: code.len() as u32,
            interruptible: Interruptible::Yes,
            idempotency: Idempotency::Unsafe,
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

/// Takes the guards like [`take_guards`], but gives up after 30 s with the lock directory's
/// listing: the take polls without a bound, and a lock still held by an earlier case would
/// otherwise hang the test until the runner's timeout without a message.
async fn take_guards_bounded(setup: &GuardSetup) -> JobGuards {
    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let take = tokio::task::spawn_blocking({
        let setup = setup.clone();
        let cancelled = std::sync::Arc::clone(&cancelled);
        move || JobGuards::take_vci_only(&setup, Duration::from_millis(10), &cancelled)
    });
    let mut take = std::pin::pin!(take);
    match tokio::time::timeout(Duration::from_secs(30), &mut take).await {
        Ok(taken) => taken
            .expect("the take should not panic")
            .expect("guards should be taken"),
        Err(_) => {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = take.await;
            let listing: Vec<_> = std::fs::read_dir(&setup.dir)
                .into_iter()
                .flatten()
                .map(|entry| entry.map(|entry| entry.path()))
                .collect();
            panic!("the guards were not taken within 30 s; lock directory: {listing:?}");
        }
    }
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
        control_dir: temp.join(format!("sim-vci-crash-control-{pid}-{nanos}")),
        trace: temp.join(format!("sim-vci-crash-trace-{pid}-{nanos}.txt")),
    };
    std::fs::create_dir(&paths.control_dir).expect("control directory should be created");
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
        // The control directory stays empty except in the case that arms a response pending
        // chain, so the other cases see no fault.
        std::env::set_var("NGR_SIM_VCI_CONTROL_DIR", &paths.control_dir);
        // Every worker appends the requests it sends to one trace file (the variable cannot
        // be set later, with the runtime's threads running); the case that needs it counts the
        // lines already there.
        std::env::set_var("NGR_SIM_VCI_TRACE", &paths.trace);
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
    reads_go_on(paths, &setup, Idempotency::Safe, "safe").await;
    reads_go_on(paths, &setup, Idempotency::Unsafe, "unsafe").await;
    an_unsafe_request_outstanding_at_the_crash_ends_on_site(paths, &setup).await;
    a_worker_dead_before_the_run_ends_it_in_the_link_error(&setup).await;
    a_start_the_system_refuses_is_a_spawn_failure().await;
}

/// How many VIN reads the trace file holds.
fn vin_reads_traced(trace: &Path) -> usize {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.trim() == "uds 22 F190")
        .count()
}

/// The worker crashes while an `Unsafe` request is outstanding: the ECU answers it with a long
/// response pending chain, and the worker is killed once the request marker is in the journal
/// and the chain has begun. The request may have reached the ECU, so the run does not send it
/// again on a new worker: it ends in on-site intervention (ADR-389, ADR-392), and the request
/// marker stays in the journal.
async fn an_unsafe_request_outstanding_at_the_crash_ends_on_site(
    paths: &TempPaths,
    setup: &GuardSetup,
) {
    let dir = paths.comm_log.join("outstanding");
    std::fs::create_dir(&dir).expect("journal directory should be created");
    let key = agent::journal::JobKey {
        job_id: shared_proto::JobId("0190f5a8-7c2e-7d4b-9a6e-3f1c2b4d5e83".to_owned()),
        generation: 1,
    };
    // 0x78 every 1.5 s, ten times: the request is still open when the worker is killed, and
    // the chain (15 s) is inside the completion timeout.
    std::fs::write(
        paths.control_dir.join("001.json"),
        r#"{"command": "inject_fault", "fault": {"response_pending": {"count": 10, "interval_ms": 1500}}}"#,
    )
    .expect("control file should be writable");
    // The VIN reads the trace holds before this case's worker exists.
    let sent_before = vin_reads_traced(&paths.trace);
    let (source, slot) = launch_source().await;
    let killed = slot
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|process| process.id())
        .expect("the worker runs");
    let job = tokio::spawn({
        let guards = take_guards_bounded(setup).await;
        let journal = JournalSetup {
            dir: dir.clone(),
            key: key.clone(),
            sources: ServiceSources::default(),
            vin: None,
            intended_software_version: None,
        };
        async move {
            run_program_journaled(
                source,
                &LinkConfig::iso15765(0x7E0, 0x7E8),
                Job::from(unsafe_read_program()).with_vci(VciFacts {
                    api: agent::launch::Api::J2534,
                    long_size: long_size(),
                }),
                limits(),
                journal,
                guards,
            )
            .await
        }
    });
    // The marker is committed before the request is sent; the worker's trace shows the send
    // (a line is appended once the ECU has handled the request, before its response is queued),
    // and the chain's first 0x78 is 1.5 s later, so the request is outstanding from the trace
    // line on.
    wait_for("the request marker", || {
        Journal::read(&dir, &key).is_ok_and(|state| state.facts.last_request.is_some())
    })
    .await;
    let started = std::time::Instant::now();
    while vin_reads_traced(&paths.trace) <= sent_before {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "gave up waiting for the request in the trace"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !job.is_finished(),
        "the request should still be outstanding"
    );
    kill_worker(killed);
    let run = tokio::time::timeout(GENEROUS, job)
        .await
        .expect("the run should end")
        .expect("the job task should not panic");
    let lines = log_lines(&dir);
    let text = || format!("{lines:#?}");
    let at = agent::journal::StepRef { pc: 1, steps: 1 };
    assert!(
        matches!(
            &run.result,
            Err(JobError::OnSiteInterventionRequired(
                agent::restart::OnSiteReason::UnconfirmedRequest { at: reason_at }
            )) if *reason_at == at
        ),
        "{:?}\n{}",
        run.result,
        text()
    );
    // The marker stays: the request is not provably unsent.
    let facts = Journal::read(&dir, &key).expect("the job's journal").facts;
    assert_eq!(facts.last_request, Some(at));
    assert_eq!(facts.last_request_unsent, None);
    assert_eq!(facts.standing_request, Some(at));
    // Nothing was sent again: the one exchange is the request the crash cut off (logged as
    // sent, its outcome unknown), with no repeat and no new worker.
    let exchanges: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["event"] == "exchange")
        .collect();
    assert_eq!(exchanges.len(), 1, "{}", text());
    assert_eq!(exchanges[0]["origin"], "procedure", "{}", text());
    // Killed at the trace line, the worker may or may not have reported the transmission yet,
    // so the log reads the send as confirmed or as unknown, never as unsent.
    assert!(
        ["confirmed", "unknown"].contains(&exchanges[0]["sent"].as_str().unwrap_or("")),
        "{}",
        text()
    );
    assert_eq!(exchanges[0]["outcome"]["result"], "unknown", "{}", text());
    assert!(
        !lines.iter().any(|line| line["event"] == "worker_restart"),
        "{}",
        text()
    );
    let exit = event(&lines, "worker_exit");
    assert_eq!(exit["pid"], killed, "{exit}");
    drop(run.guards);
    // The slot's process is the killed one, reaped and so without an ID: no new worker was
    // started.
    let process = slot
        .lock()
        .unwrap()
        .take()
        .expect("the slot holds a worker");
    assert_eq!(process.id(), None);
    drop(process);
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

/// The worker crashes during the wait, so the next read is refused unsent by the host (the
/// event stream failed, ADR-420 item 8) and sent again on the new worker, whatever its
/// idempotency: it never reached the ECU, so even an `Unsafe` read goes on.
async fn reads_go_on(paths: &TempPaths, setup: &GuardSetup, idempotency: Idempotency, name: &str) {
    let (run, lines, killed, slot) = crash_during_the_wait(paths, setup, idempotency, name).await;
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
